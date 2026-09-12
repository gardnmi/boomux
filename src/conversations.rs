//! Workspace-owned conversation entries derived only from durable Agent records.
use crate::protocol::{
    AgentInstanceSnapshot, AgentState, ShellSnapshot, ShellSpec, ShellStatus, WorkspaceSnapshot,
};
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Conversation {
    pub agent_id: String,
    pub integration: String,
    pub external_session_id: String,
    pub title: String,
    pub updated_at_ms: u64,
    pub running_shell: Option<String>,
    pub resumable: bool,
}

fn running_shell<'a>(
    shells: &HashMap<&str, &'a ShellSnapshot>,
    agent: &AgentInstanceSnapshot,
) -> Option<&'a ShellSnapshot> {
    if matches!(
        agent.observation.state,
        AgentState::Inactive | AgentState::Done
    ) {
        return None;
    }
    shells
        .get(agent.shell_id.as_str())
        .copied()
        .filter(|shell| {
            matches!(shell.status, ShellStatus::Running)
                && shell.run.as_ref().is_some_and(|run| run.id == agent.run_id)
        })
}

pub fn list(workspace: &WorkspaceSnapshot) -> Vec<Conversation> {
    let shells: HashMap<_, _> = workspace
        .shells
        .iter()
        .map(|shell| (shell.id.as_str(), shell))
        .collect();
    // An exact native resume may be running before its lifecycle integration
    // reports a session. A current exact session report supersedes startup argv.
    let claimed_runs: HashSet<_> = workspace
        .agents
        .iter()
        .filter(|agent| agent.workspace_id == workspace.id && agent.external_session_id.is_some())
        .map(|agent| (agent.shell_id.as_str(), agent.run_id.as_str()))
        .collect();
    let mut resumes: HashMap<&[String], &ShellSnapshot> = HashMap::new();
    for shell in &workspace.shells {
        if shell.workspace_id == workspace.id
            && matches!(shell.status, ShellStatus::Running)
            && shell
                .run
                .as_ref()
                .is_some_and(|run| !claimed_runs.contains(&(shell.id.as_str(), run.id.as_str())))
        {
            let current = resumes.entry(shell.command.as_slice()).or_insert(shell);
            if shell.id < current.id {
                *current = shell;
            }
        }
    }
    let mut entries = BTreeMap::new();
    for agent in &workspace.agents {
        if agent.workspace_id != workspace.id {
            continue;
        }
        let Some(session) = agent
            .external_session_id
            .as_deref()
            .filter(|id| crate::integrations::validate_external_session_id(id).is_ok())
        else {
            continue;
        };
        let entry = Conversation {
            agent_id: agent.id.clone(),
            integration: agent.integration.clone(),
            external_session_id: session.into(),
            title: agent.name.clone(),
            updated_at_ms: agent.observation.observed_at_ms.max(agent.started_at_ms),
            running_shell: running_shell(&shells, agent).map(|shell| shell.id.clone()),
            resumable: crate::integrations::by_key(&agent.integration)
                .is_some_and(|integration| integration.resume.is_some()),
        };
        let key = (entry.integration.clone(), entry.external_session_id.clone());
        let replace = entries.get(&key).is_none_or(|old: &Conversation| {
            (
                entry.running_shell.is_some(),
                entry.updated_at_ms,
                &entry.agent_id,
            ) > (
                old.running_shell.is_some(),
                old.updated_at_ms,
                &old.agent_id,
            )
        });
        if replace {
            entries.insert(key, entry);
        }
    }
    let mut entries: Vec<_> = entries.into_values().collect();
    for entry in &mut entries {
        if entry.running_shell.is_none()
            && let Some(command) = crate::integrations::by_key(&entry.integration)
                .and_then(|integration| integration.resume)
                .and_then(|resume| resume.command(&[], &entry.external_session_id))
            && let Some(shell) = resumes.get(command.as_slice())
        {
            entry.running_shell = Some(shell.id.clone());
        }
    }
    entries.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then(a.agent_id.cmp(&b.agent_id))
    });
    entries
}

/// Resolve only registered aliases; unknown harness keys remain distinct.
pub(crate) fn canonical_integration(key: &str) -> &str {
    crate::integrations::by_key(key).map_or(key, |descriptor| descriptor.key)
}

/// Enrich recorded entries from an owner-scoped catalog by exact harness and external identity.
/// Catalog-only history never creates a Workspace conversation.
pub(crate) fn enrich_titles(
    entries: &mut [Conversation],
    sessions: &[crate::host_session_titles::HostSession],
) {
    let titles: HashMap<_, _> = sessions
        .iter()
        .map(|session| {
            (
                (
                    canonical_integration(&session.integration),
                    session.root_id.as_str(),
                ),
                (session.title.trim(), session.updated_at_ms),
            )
        })
        .filter(|(_, (title, _))| !title.is_empty())
        .collect();
    for entry in entries.iter_mut() {
        if let Some((title, updated_at_ms)) = titles.get(&(
            canonical_integration(&entry.integration),
            entry.external_session_id.as_str(),
        )) {
            entry.title = (*title).to_owned();
            entry.updated_at_ms = entry.updated_at_ms.max(*updated_at_ms);
        }
    }
    entries.sort_by(|a, b| {
        b.updated_at_ms
            .cmp(&a.updated_at_ms)
            .then(a.agent_id.cmp(&b.agent_id))
    });
}

pub enum OpenPlan {
    Existing(Box<ShellSnapshot>),
    Resume(ShellSpec),
}

/// Called by the owner inside its durable mutation gate. Never discovers history,
/// creates a Workspace, or infers conversation identity from a directory or title.
pub fn plan(
    workspace: &WorkspaceSnapshot,
    agent_id: &str,
    shell_id: &str,
) -> Result<OpenPlan, String> {
    let agent = workspace
        .agents
        .iter()
        .find(|agent| agent.id == agent_id && agent.workspace_id == workspace.id)
        .ok_or("Conversation no longer belongs to this Workspace")?;
    let session = agent
        .external_session_id
        .as_deref()
        .ok_or("Conversation has no exact harness session ID")?;
    crate::integrations::validate_external_session_id(session).map_err(str::to_owned)?;
    if session.starts_with('-') {
        return Err("Conversation ID cannot be passed as a harness option".into());
    }
    let entry = list(workspace)
        .into_iter()
        .find(|entry| {
            entry.integration == agent.integration && entry.external_session_id == session
        })
        .ok_or("Conversation is unavailable")?;
    if let Some(id) = entry.running_shell {
        return Ok(OpenPlan::Existing(Box::new(
            workspace
                .shells
                .iter()
                .find(|shell| shell.id == id)
                .unwrap()
                .clone(),
        )));
    }
    let newest = workspace
        .agents
        .iter()
        .find(|agent| agent.id == entry.agent_id)
        .unwrap();
    let resume = crate::integrations::by_key(&agent.integration)
        .and_then(|integration| integration.resume)
        .ok_or("This harness does not support exact conversation resume")?;
    let command = resume
        .command(&[], session)
        .ok_or("This conversation cannot be resumed")?;
    // Reuse an in-flight resume across double clicks and concurrent clients.
    if let Some(shell) = workspace.shells.iter().find(|shell| {
        shell.command == command
            && (shell.id == shell_id
                || matches!(shell.status, ShellStatus::Pending)
                || (matches!(shell.status, ShellStatus::Running)
                    && !workspace.agents.iter().any(|a| {
                        a.shell_id == shell.id
                            && a.external_session_id.is_some()
                            && shell.run.as_ref().is_some_and(|run| run.id == a.run_id)
                    })))
    }) {
        return Ok(OpenPlan::Existing(Box::new(shell.clone())));
    }
    let cwd = newest
        .cwd
        .clone()
        .or_else(|| workspace.default_cwd.clone())
        .ok_or("Conversation has no saved working directory")?;
    let mut shell = ShellSpec::login(
        crate::generated_names::random_excluding(
            workspace.shells.iter().map(|shell| shell.name.as_str()),
        )
        .ok_or("Shell names exhausted")?,
        cwd,
    );
    shell.command = command;
    Ok(OpenPlan::Resume(shell))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn workspace() -> WorkspaceSnapshot {
        serde_json::from_value(serde_json::json!({"id":"workspace-a", "name":"Project", "default_cwd":"/tmp", "shells":[], "agents":[
            {"id":"a1", "workspace_id":"workspace-a", "shell_id":"removed", "run_id":"r1", "name":"Investigate issue", "integration":"codex", "external_session_id":"thread-1", "started_at_ms":1, "observation":{"revision":1,"state":"inactive","authority":"lifecycle_integration","evidence":"","confidence":100,"observed_at_ms":2}},
            {"id":"a2", "workspace_id":"workspace-a", "shell_id":"removed", "run_id":"r2", "name":"Investigate issue", "integration":"codex", "external_session_id":"thread-1", "started_at_ms":3, "observation":{"revision":1,"state":"inactive","authority":"lifecycle_integration","evidence":"","confidence":100,"observed_at_ms":4}}
        ]})).unwrap()
    }
    #[test]
    fn conversation_resume_becomes_open_before_session_report_and_stops_on_switch_or_exit() {
        let mut workspace = workspace();
        for agent in &mut workspace.agents {
            agent.integration = "opencode".into();
        }
        let command = crate::integrations::by_key("opencode")
            .unwrap()
            .resume
            .unwrap()
            .command(&[], "thread-1")
            .unwrap();
        workspace.shells.push(serde_json::from_value(serde_json::json!({
            "id":"resumed", "workspace_id":"workspace-a", "name":"resumed", "cwd":"/tmp", "command":command,
            "status":"running", "run":{"id":"resume-run", "generation":1, "started_at_ms":5, "ended_at_ms":null, "exit_reason":null, "output_revision":0, "environment_has_run_id":true}
        })).unwrap());
        assert_eq!(
            list(&workspace)[0].running_shell.as_deref(),
            Some("resumed")
        );
        assert!(
            matches!(plan(&workspace, "a1", "another-shell").unwrap(), OpenPlan::Existing(shell) if shell.id == "resumed")
        );
        let mut placeholder = workspace.agents[0].clone();
        placeholder.id = "placeholder".into();
        placeholder.shell_id = "resumed".into();
        placeholder.run_id = "resume-run".into();
        placeholder.external_session_id = None;
        placeholder.observation.state = AgentState::Idle;
        workspace.agents.push(placeholder);
        assert_eq!(
            list(&workspace)[0].running_shell.as_deref(),
            Some("resumed")
        );
        workspace.agents.last_mut().unwrap().external_session_id = Some("another-thread".into());
        assert!(
            list(&workspace)
                .iter()
                .find(|entry| entry.external_session_id == "thread-1")
                .unwrap()
                .running_shell
                .is_none()
        );
        assert!(matches!(
            plan(&workspace, "a1", "another-shell").unwrap(),
            OpenPlan::Resume(_)
        ));
        workspace.agents.pop();
        workspace.shells[0].status = ShellStatus::Exited { code: Some(0) };
        assert!(list(&workspace)[0].running_shell.is_none());
        workspace.shells[0].status = ShellStatus::Pending;
        assert!(list(&workspace)[0].running_shell.is_none());
        workspace.shells[0].status = ShellStatus::Running;
        workspace.shells[0].workspace_id = "other-workspace".into();
        assert!(list(&workspace)[0].running_shell.is_none());
    }

    #[test]
    fn conversation_titles_match_exact_harness_and_session_without_importing_history() {
        let mut entries = list(&workspace());
        let mut title = crate::host_session_titles::HostSession {
            integration: "codex".into(),
            root_id: "thread-1".into(),
            title: "Respond to greeting".into(),
            directory: "/tmp".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        };
        enrich_titles(&mut entries, &[title.clone()]);
        assert_eq!(entries[0].title, "Respond to greeting");
        title.title = "Renamed thread".into();
        enrich_titles(&mut entries, &[title.clone()]);
        assert_eq!(entries[0].title, "Renamed thread");
        title.integration = "claude".into();
        title.title = "Wrong harness".into();
        enrich_titles(&mut entries, &[title.clone()]);
        assert_eq!(entries[0].title, "Renamed thread");
        title.integration = "codex".into();
        title.root_id = "outside-boomux".into();
        enrich_titles(&mut entries, &[title]);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Renamed thread");
    }
    #[test]
    fn kiro_legacy_lifecycle_key_matches_v3_catalog_only() {
        assert_eq!(canonical_integration("kiro"), "kiro-v3");
        assert_eq!(canonical_integration("kiro-v2"), "kiro-v2");
        assert_eq!(canonical_integration("unknown"), "unknown");
        let mut entries = list(&workspace());
        entries[0].integration = "kiro".into();
        let mut title = crate::host_session_titles::HostSession {
            integration: "kiro-v3".into(),
            root_id: entries[0].external_session_id.clone(),
            title: "Saved Kiro title".into(),
            directory: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        enrich_titles(&mut entries, &[title.clone()]);
        assert_eq!(entries[0].title, "Saved Kiro title");
        assert_eq!(entries[0].integration, "kiro");
        title.integration = "kiro-v2".into();
        title.title = "Wrong engine".into();
        enrich_titles(&mut entries, &[title]);
        assert_eq!(entries[0].title, "Saved Kiro title");
    }

    #[test]
    fn conversation_recency_includes_harness_activity_without_importing_entries() {
        let mut entries = list(&workspace());
        let mut other = entries[0].clone();
        other.agent_id = "other".into();
        other.external_session_id = "other-session".into();
        other.updated_at_ms = 100;
        entries.push(other);
        let title = crate::host_session_titles::HostSession {
            integration: "codex".into(),
            root_id: "thread-1".into(),
            title: "Latest work".into(),
            directory: "/tmp".into(),
            created_at_ms: 1,
            updated_at_ms: 200,
        };
        enrich_titles(&mut entries, &[title]);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].external_session_id, "thread-1");
        assert_eq!(entries[0].updated_at_ms, 200);
        assert_eq!(entries[1].updated_at_ms, 100);
    }

    #[test]
    fn conversation_entries_survive_shell_removal_and_deduplicate_runs() {
        let workspace = workspace();
        let entries = list(&workspace);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].agent_id, "a2");
        let OpenPlan::Resume(spec) = plan(&workspace, "a1", "shell-new").unwrap() else {
            panic!("expected resume")
        };
        assert_eq!(spec.command, ["codex", "resume", "thread-1"]);
        assert_eq!(spec.cwd, std::path::Path::new("/tmp"));
    }
    #[test]
    fn conversation_ownership_is_workspace_identity_not_name_or_directory() {
        let mut workspace = workspace();
        workspace.id = "recreated-workspace".into();
        assert!(list(&workspace).is_empty());
        assert!(plan(&workspace, "a1", "new").is_err());
        workspace.agents.clear();
        assert!(list(&workspace).is_empty());
    }
    #[test]
    fn conversations_keep_harness_identity_and_literal_session_arguments() {
        let mut workspace = workspace();
        workspace.agents[1].integration = "claude".into();
        workspace.agents[1].external_session_id = Some("literal;$(echo)".into());
        assert_eq!(list(&workspace).len(), 2);
        let OpenPlan::Resume(spec) = plan(&workspace, "a2", "new").unwrap() else {
            panic!()
        };
        assert_eq!(spec.command, ["claude", "--resume", "literal;$(echo)"]);
    }
}
