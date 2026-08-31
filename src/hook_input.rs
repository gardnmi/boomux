use std::io::{self, Read};
use std::path::PathBuf;

use boomux::protocol::MAX_AGENT_WORKING_CONTEXTS;

pub(crate) const MAX_HOOK_INPUT_BYTES: usize = 1024 * 1024;

pub(crate) fn read_bounded_hook_input(
    mut reader: impl Read,
    integration: &str,
) -> io::Result<Vec<u8>> {
    let mut input = Vec::new();
    reader
        .by_ref()
        .take((MAX_HOOK_INPUT_BYTES + 1) as u64)
        .read_to_end(&mut input)?;
    if input.len() > MAX_HOOK_INPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{integration} hook input exceeds {MAX_HOOK_INPUT_BYTES} bytes"),
        ));
    }
    Ok(input)
}

pub(crate) fn structured_working_contexts(
    cwd: Option<PathBuf>,
    tool_name: Option<&str>,
    tool_input: Option<&serde_json::Value>,
) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(cwd) = cwd.filter(|path| path.is_absolute()) {
        paths.push(cwd);
    }
    let Some(tool_name) = tool_name else {
        return paths;
    };
    let tool_name = tool_name
        .chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect::<String>();
    let Some(input) = tool_input.and_then(serde_json::Value::as_object) else {
        return paths;
    };
    let fields: &[&str] = match tool_name.as_str() {
        "read" | "write" | "edit" | "multiedit" | "notebookedit" => {
            &["file_path", "filePath", "notebook_path", "notebookPath"]
        }
        "glob" | "grep" | "list" | "ls" | "find" | "search" => &["path"],
        "bash" | "shell" | "execute" | "executebash" => &["workdir", "cwd"],
        _ => &[],
    };
    for field in fields {
        let Some(path) = input
            .get(*field)
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
        else {
            continue;
        };
        if !paths.contains(&path) {
            paths.push(path);
        }
        if paths.len() == MAX_AGENT_WORKING_CONTEXTS {
            break;
        }
    }
    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_input_and_names_the_integration() {
        assert_eq!(
            read_bounded_hook_input(&b"input"[..], "Test").unwrap(),
            b"input"
        );
        let error =
            read_bounded_hook_input(&vec![b'x'; MAX_HOOK_INPUT_BYTES + 1][..], "Test").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().starts_with("Test hook input exceeds"));
    }

    #[test]
    fn extracts_only_allowlisted_absolute_structured_paths() {
        assert_eq!(
            structured_working_contexts(
                Some("/repo".into()),
                Some("Read"),
                Some(&serde_json::json!({
                    "file_path": "/other/src/main.rs",
                    "command": "cd /private && cat secret"
                })),
            ),
            [PathBuf::from("/repo"), PathBuf::from("/other/src/main.rs")]
        );
        assert_eq!(
            structured_working_contexts(
                Some("relative".into()),
                Some("Bash"),
                Some(&serde_json::json!({
                    "workdir": "/worktree",
                    "path": "/ignored"
                })),
            ),
            [PathBuf::from("/worktree")]
        );
    }
}
