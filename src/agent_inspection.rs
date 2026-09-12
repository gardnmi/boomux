//! Bounded, read-only inventory on the Agent's owner. Configuration is not runtime telemetry.
use crate::protocol::AgentInstanceSnapshot;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const MAX_ITEMS: usize = 128;
const MAX_WARNINGS: usize = 16;
const MAX_FILE: usize = 256 * 1024;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENTRIES: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    User,
    Project,
    System,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: PathBuf,
    pub scope: Scope,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServer {
    pub name: String,
    pub source: PathBuf,
    pub scope: Scope,
    pub transport: String,
    pub enabled: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub agent: AgentInstanceSnapshot,
    pub inspected_at_ms: u64,
    pub skills: Vec<Skill>,
    pub mcp_servers: Vec<McpServer>,
    pub warnings: Vec<String>,
    pub truncated: bool,
}

struct Inventory {
    skills: Vec<Skill>,
    servers: Vec<McpServer>,
    warnings: Vec<String>,
    remaining_bytes: usize,
    remaining_entries: usize,
    visited: BTreeSet<PathBuf>,
    truncated: bool,
}

impl Inventory {
    fn warning(&mut self, message: String) {
        if self.warnings.len() < MAX_WARNINGS && !self.warnings.contains(&message) {
            self.warnings.push(message);
        }
    }

    fn read(&mut self, path: &Path) -> Option<String> {
        // Nonblocking open rejects device/FIFO inputs without waiting for a writer.
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
            Err(_) => {
                self.warning(format!("Could not read {}", path.display()));
                return None;
            }
        };
        let metadata = file.metadata().ok()?;
        if !metadata.is_file() {
            self.warning(format!("Skipped non-file {}", path.display()));
            return None;
        }
        let limit = MAX_FILE.min(self.remaining_bytes);
        if metadata.len() > limit as u64 || limit == 0 {
            self.truncated = true;
            self.warning(format!("Inspection limit reached at {}", path.display()));
            return None;
        }
        let mut bytes = Vec::new();
        if (&mut file)
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .is_err()
        {
            self.warning(format!("Could not read {}", path.display()));
            return None;
        }
        self.remaining_bytes = self.remaining_bytes.saturating_sub(bytes.len());
        if bytes.len() > limit {
            self.truncated = true;
            return None;
        }
        match String::from_utf8(bytes) {
            Ok(text) => Some(text),
            Err(_) => {
                self.warning(format!("Invalid text in {}", path.display()));
                None
            }
        }
    }

    fn skills(&mut self, root: &Path, scope: Scope, depth: usize) {
        if depth > 8 || self.remaining_entries == 0 || self.skills.len() == MAX_ITEMS {
            self.truncated = true;
            return;
        }
        let Ok(canonical) = root.canonicalize() else {
            return;
        };
        if !self.visited.insert(canonical) {
            return;
        }
        if root.join("SKILL.md").is_file() {
            if let Some(text) = self.read(&root.join("SKILL.md")) {
                let (name, description) = frontmatter(&text);
                self.skills.push(Skill {
                    name: name.unwrap_or_else(|| {
                        clean(&root.file_name().unwrap_or_default().to_string_lossy(), 128)
                    }),
                    description: description.unwrap_or_else(|| "Description not available".into()),
                    source: root.join("SKILL.md"),
                    scope,
                });
            }
            return;
        }
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => {
                self.warning(format!("Could not inspect {}", root.display()));
                return;
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            if self.remaining_entries == 0 {
                self.truncated = true;
                break;
            }
            self.remaining_entries -= 1;
            if let Ok(entry) = entry {
                paths.push(entry.path());
            }
        }
        paths.sort();
        for path in paths {
            if path.is_dir() {
                self.skills(&path, scope.clone(), depth + 1);
            }
            if self.skills.len() == MAX_ITEMS {
                self.truncated = true;
                break;
            }
        }
    }

    fn config(&mut self, path: &Path, scope: Scope, toml: bool, key: &str, project: Option<&Path>) {
        let Some(text) = self.read(path) else {
            return;
        };
        let value = if toml {
            toml::from_str::<toml::Value>(&text)
                .ok()
                .and_then(|v| serde_json::to_value(v).ok())
        } else {
            serde_json::from_str::<serde_json::Value>(&json_without_comments(&text)).ok()
        };
        let Some(value) = value else {
            // Parser diagnostics can contain credentials from the offending source line.
            self.warning(format!(
                "Could not parse {} (contents omitted)",
                path.display()
            ));
            return;
        };
        self.server_map(value.get(key), path, scope.clone());
        if let Some(project) = project {
            self.server_map(
                value
                    .get("projects")
                    .and_then(|v| v.get(project.to_string_lossy().as_ref()))
                    .and_then(|v| v.get(key)),
                path,
                Scope::Project,
            );
        }
    }

    fn server_map(&mut self, map: Option<&serde_json::Value>, path: &Path, scope: Scope) {
        let Some(map) = map.and_then(|v| v.as_object()) else {
            return;
        };
        for (name, server) in map {
            if self.servers.len() == MAX_ITEMS {
                self.truncated = true;
                break;
            }
            if !server.is_object() {
                self.warning(format!("Invalid MCP entry in {}", path.display()));
                continue;
            }
            let transport = match server.get("type").and_then(|v| v.as_str()) {
                Some("stdio" | "local") => "stdio",
                Some("http" | "remote") => "http",
                Some("sse") => "sse",
                _ if server.get("command").is_some() => "stdio",
                _ if server.get("url").is_some() => "http",
                _ => "not specified",
            };
            self.servers.push(McpServer {
                name: clean(name, 128),
                source: path.into(),
                scope: scope.clone(),
                transport: transport.into(),
                enabled: server
                    .get("enabled")
                    .and_then(|v| v.as_bool())
                    .or_else(|| server.get("disabled").and_then(|v| v.as_bool()).map(|v| !v)),
            });
        }
    }
}

pub fn inspect(agent: AgentInstanceSnapshot) -> Inspection {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    inspect_at(agent, home.as_deref())
}

fn inspect_at(agent: AgentInstanceSnapshot, home: Option<&Path>) -> Inspection {
    let mut inventory = Inventory {
        skills: vec![],
        servers: vec![],
        warnings: vec![],
        remaining_bytes: MAX_BYTES,
        remaining_entries: MAX_ENTRIES,
        visited: BTreeSet::new(),
        truncated: false,
    };
    inventory.warning("Standard configuration locations only: loading, connection health, authentication and tools are not reported by this Agent. Environment and run-specific overrides, managed configuration and plugin/package inventories are not included.".into());
    let mut ancestors = Vec::new();
    if let Some(cwd) = agent.cwd.as_ref().filter(|p| p.is_absolute()) {
        for directory in cwd.ancestors().take(16) {
            ancestors.push(directory);
            if directory.join(".git").exists() {
                break;
            }
        }
    } else {
        inventory.warning(
            "Agent working directory is not reported; project configuration was not inspected."
                .into(),
        );
    }
    let roots: &[&str] = match agent.integration.as_str() {
        "codex" => &[".agents/skills", ".codex/skills"],
        "claude" => &[".claude/skills"],
        "opencode" => &[".opencode/skills", ".claude/skills", ".agents/skills"],
        "pi" => &[".pi/skills", ".agents/skills"],
        _ => &[],
    };
    if roots.is_empty() {
        inventory.warning("Skill and MCP inventory is not supported for this harness yet.".into());
    }
    if let Some(home) = home.filter(|p| p.is_absolute()) {
        match agent.integration.as_str() {
            "codex" => {
                inventory.skills(&home.join(".agents/skills"), Scope::User, 0);
                inventory.skills(&home.join(".codex/skills"), Scope::User, 0);
                inventory.skills(Path::new("/etc/codex/skills"), Scope::System, 0);
                inventory.config(
                    &home.join(".codex/config.toml"),
                    Scope::User,
                    true,
                    "mcp_servers",
                    None,
                );
            }
            "claude" => {
                inventory.skills(&home.join(".claude/skills"), Scope::User, 0);
                inventory.config(
                    &home.join(".claude.json"),
                    Scope::User,
                    false,
                    "mcpServers",
                    ancestors.last().copied(),
                );
            }
            "opencode" => {
                for path in [
                    ".config/opencode/skills",
                    ".claude/skills",
                    ".agents/skills",
                ] {
                    inventory.skills(&home.join(path), Scope::User, 0);
                }
                for path in [
                    ".config/opencode/opencode.json",
                    ".config/opencode/opencode.jsonc",
                ] {
                    inventory.config(&home.join(path), Scope::User, false, "mcp", None);
                }
            }
            "pi" => {
                for path in [".pi/agent/skills", ".agents/skills"] {
                    inventory.skills(&home.join(path), Scope::User, 0);
                }
                inventory.warning("Pi MCP extensions are not inspected.".into());
            }
            _ => {}
        }
    } else {
        inventory.warning(
            "Owner home directory is unavailable; user configuration was not inspected.".into(),
        );
    }
    for directory in ancestors {
        for root in roots {
            inventory.skills(&directory.join(root), Scope::Project, 0);
        }
        match agent.integration.as_str() {
            "codex" => inventory.config(
                &directory.join(".codex/config.toml"),
                Scope::Project,
                true,
                "mcp_servers",
                None,
            ),
            "claude" => inventory.config(
                &directory.join(".mcp.json"),
                Scope::Project,
                false,
                "mcpServers",
                None,
            ),
            "opencode" => {
                for name in ["opencode.json", "opencode.jsonc"] {
                    inventory.config(&directory.join(name), Scope::Project, false, "mcp", None);
                }
            }
            _ => {}
        }
    }
    inventory
        .skills
        .sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
    inventory
        .servers
        .sort_by(|a, b| (&a.name, &a.source).cmp(&(&b.name, &b.source)));
    Inspection {
        agent,
        inspected_at_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64),
        skills: inventory.skills,
        mcp_servers: inventory.servers,
        warnings: inventory.warnings,
        truncated: inventory.truncated,
    }
}

fn clean(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

// Read only common scalar/folded metadata. Never return the skill's instruction body.
fn frontmatter(text: &str) -> (Option<String>, Option<String>) {
    let mut lines = text.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (None, None);
    }
    let mut name = None;
    let mut description = None;
    let mut folded = false;
    for line in lines.take(128) {
        if line.trim() == "---" {
            break;
        }
        if folded && line.starts_with(char::is_whitespace) {
            if let Some(value) = description.as_mut() {
                *value = clean(format!("{value} {}", line.trim()).trim(), 512);
            }
            continue;
        }
        folded = false;
        if let Some((key, value)) = line.split_once(':') {
            let value = value.trim();
            match key {
                "name" => name = Some(clean(value.trim_matches(['\'', '"']), 128)),
                "description" => {
                    folded = matches!(value, ">" | ">-" | "|" | "|-");
                    description = Some(if folded {
                        String::new()
                    } else {
                        clean(value.trim_matches(['\'', '"']), 512)
                    });
                }
                _ => {}
            }
        }
    }
    (
        name.filter(|s| !s.is_empty()),
        description.filter(|s| !s.is_empty()),
    )
}

// JSONC comments/trailing commas are removed only outside string literals.
fn json_without_comments(text: &str) -> String {
    let mut bytes = text.as_bytes().to_vec();
    let (mut i, mut quoted) = (0, false);
    while i < bytes.len() {
        if quoted {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                quoted = false;
            }
        } else if bytes[i] == b'"' {
            quoted = true;
        } else if bytes.get(i..i + 2) == Some(b"//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                bytes[i] = b' ';
                i += 1;
            }
            continue;
        } else if bytes.get(i..i + 2) == Some(b"/*") {
            bytes[i] = b' ';
            bytes[i + 1] = b' ';
            i += 2;
            while i < bytes.len() && bytes.get(i..i + 2) != Some(b"*/") {
                bytes[i] = b' ';
                i += 1;
            }
            if i + 1 < bytes.len() {
                bytes[i] = b' ';
                bytes[i + 1] = b' ';
                i += 2;
            } else {
                return String::new();
            }
            continue;
        }
        i += 1;
    }
    quoted = false;
    i = 0;
    while i < bytes.len() {
        if quoted {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                quoted = false;
            }
        } else if bytes[i] == b'"' {
            quoted = true;
        } else if bytes[i] == b',' {
            let next = bytes[i + 1..].iter().find(|b| !b.is_ascii_whitespace());
            if matches!(next, Some(b'}' | b']')) {
                bytes[i] = b' ';
            }
        }
        i += 1;
    }
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root =
                std::env::temp_dir().join(format!("boomux-inventory-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(root.join("project/.git")).unwrap();
            fs::create_dir_all(root.join("home")).unwrap();
            Self(root)
        }
        fn write(&self, name: &str, text: &str) {
            let path = self.0.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        fn agent(&self, integration: &str) -> AgentInstanceSnapshot {
            serde_json::from_value(serde_json::json!({
                "id":"agent", "workspace_id":"workspace", "shell_id":"shell", "run_id":"run",
                "name":"test", "integration":integration, "external_session_id":null,
                "cwd":self.0.join("project"), "started_at_ms":1, "ended_at_ms":null,
                "observation":{"revision":1,"state":"idle","authority":"lifecycle_integration","evidence":"fixture","confidence":100,"observed_at_ms":1}
            })).unwrap()
        }
        fn inspect(&self, integration: &str) -> Inspection {
            inspect_at(self.agent(integration), Some(&self.0.join("home")))
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn inventory_keeps_sources_and_excludes_mcp_credentials_commands_and_skill_body() {
        let f = Fixture::new();
        f.write("home/.codex/config.toml", "[mcp_servers.github]\ncommand='secret-command'\nargs=['SECRET-ARG']\nenabled=false\n[mcp_servers.github.env]\nTOKEN='SECRET-TOKEN'\n");
        f.write("project/.codex/config.toml", "[mcp_servers.github]\nurl='https://user:SECRET-PASSWORD@example.com/?token=SECRET-QUERY'\nenabled=true\nhttp_headers={Authorization='SECRET-HEADER'}\n");
        f.write("home/.agents/skills/review/SKILL.md", "---\nname: review\ndescription: >-\n  Review changes\n  carefully.\n---\nSECRET-INSTRUCTION-BODY");
        let result = f.inspect("codex");
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].description, "Review changes carefully.");
        assert_eq!(result.mcp_servers.len(), 2);
        assert!(
            result
                .mcp_servers
                .iter()
                .any(|s| s.enabled == Some(false) && s.scope == Scope::User)
        );
        assert!(
            result
                .mcp_servers
                .iter()
                .any(|s| s.enabled == Some(true) && s.scope == Scope::Project)
        );
        let json = serde_json::to_string(&result).unwrap();
        assert!(!json.contains("SECRET"));
        assert!(!json.contains("secret-command"));
        assert!(!json.contains("connected"));
    }

    #[test]
    fn inventory_jsonc_preserves_string_slashes_and_reports_invalid_files_without_contents() {
        let f = Fixture::new();
        f.write("project/opencode.jsonc", r#"{ // note
            "mcp": { "source": { "url": "https://example.com/a//b", "enabled": false, }, }, /* trailing */
        }"#);
        let result = f.inspect("opencode");
        assert_eq!(result.mcp_servers.len(), 1);
        assert_eq!(result.mcp_servers[0].transport, "http");
        assert_eq!(result.mcp_servers[0].enabled, Some(false));
        f.write(
            "project/opencode.jsonc",
            "{ \"token\": SECRET-PARSE-ERROR }",
        );
        let result = f.inspect("opencode");
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("Could not parse"))
        );
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("SECRET-PARSE-ERROR")
        );
    }

    #[test]
    fn inventory_follows_skill_symlinks_without_cycles_or_crossing_repo_boundary() {
        use std::os::unix::fs::symlink;
        let f = Fixture::new();
        f.write(
            "project/.agents/skills/a/SKILL.md",
            "---\nname: a\ndescription: test\n---",
        );
        f.write(
            ".agents/skills/outside/SKILL.md",
            "---\nname: outside\ndescription: wrong scope\n---",
        );
        symlink(
            f.0.join("project/.agents/skills"),
            f.0.join("project/.agents/skills/loop"),
        )
        .unwrap();
        symlink(
            f.0.join("project/.agents/skills/a"),
            f.0.join("project/.agents/skills/alias"),
        )
        .unwrap();
        let result = f.inspect("codex");
        assert_eq!(result.skills.len(), 1);
        assert_eq!(result.skills[0].name, "a");
    }

    #[test]
    fn inventory_limits_files_entries_and_unknown_harnesses_explicitly() {
        let f = Fixture::new();
        f.write("home/.codex/config.toml", &"x".repeat(MAX_FILE + 1));
        for i in 0..MAX_ITEMS + 2 {
            f.write(
                &format!("project/.agents/skills/s{i}/SKILL.md"),
                "---\nname: test\ndescription: test\n---",
            );
        }
        let result = f.inspect("codex");
        assert!(result.truncated);
        assert_eq!(result.skills.len(), MAX_ITEMS);
        assert!(result.mcp_servers.is_empty());
        assert!(
            f.inspect("unknown")
                .warnings
                .iter()
                .any(|w| w.contains("not supported"))
        );
    }

    #[test]
    fn inventory_never_opens_fifo_as_a_blocking_file() {
        let f = Fixture::new();
        fs::create_dir_all(f.0.join("home/.codex")).unwrap();
        let path =
            std::ffi::CString::new(f.0.join("home/.codex/config.toml").to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
        let result = f.inspect("codex");
        assert!(result.warnings.iter().any(|w| w.contains("non-file")));
    }
}
