use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{
    EventCursor, NodeProjectionHealth, NodeProjectionHealthCode, NodeProjectionSnapshot,
    NodeRegistrationSnapshot,
};

#[derive(Clone)]
pub(crate) struct NodeProjectionView {
    pub(crate) health: NodeProjectionHealth,
    pub(crate) projection: Option<NodeProjectionSnapshot>,
}
use crate::state_store::{effective_uid, secure_state_dir, state_directory_from_environment};

const NODE_CACHE_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NODES: usize = 128;
const MAX_WORKSPACES_PER_NODE: usize = 1_024;
const MAX_SHELLS_PER_NODE: usize = 4_096;
const MAX_LAUNCHERS_PER_NODE: usize = 4_096;
const MAX_AGENTS_PER_NODE: usize = 4_096;
const MAX_SCHEDULES_PER_NODE: usize = 1_024;
const MAX_CAPABILITIES: usize = 96;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_NAME_BYTES: usize = 256;

pub(crate) struct NodeProjectionCache {
    path: PathBuf,
    state: Mutex<CacheState>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CacheState {
    version: u32,
    nodes: Vec<CachedNode>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedNode {
    node_id: String,
    registration_revision: u64,
    tombstone_epoch: u64,
    generation: u64,
    health: NodeProjectionHealthCode,
    last_attempt_at_ms: Option<u64>,
    last_success_at_ms: Option<u64>,
    retry_at_ms: Option<u64>,
    capabilities: Vec<String>,
    cursor: Option<EventCursor>,
    projection: Option<NodeProjectionSnapshot>,
}

impl NodeProjectionCache {
    pub(crate) fn load_from_environment() -> Self {
        let path = state_directory_from_environment()
            .map(|directory| directory.join("node-cache.json"))
            .unwrap_or_default();
        Self::load_at(path)
    }

    fn load_at(path: PathBuf) -> Self {
        let mut state = match load(&path) {
            Ok(state) => state,
            Err(error) if error.kind() == io::ErrorKind::NotFound => empty_state(),
            Err(error) => {
                if !path.as_os_str().is_empty() {
                    quarantine(&path);
                }
                eprintln!("boomux: discarded invalid disposable Node cache: {error}");
                empty_state()
            }
        };
        let mut changed = false;
        for node in &mut state.nodes {
            if node.health == NodeProjectionHealthCode::Online {
                node.health = NodeProjectionHealthCode::Stale;
                changed = true;
            }
        }
        if changed && let Err(error) = save(&path, &state) {
            eprintln!("boomux: could not mark recovered Node projections stale: {error}");
        }
        Self {
            path,
            state: Mutex::new(state),
        }
    }

    pub(crate) fn health(
        &self,
        registration: &NodeRegistrationSnapshot,
    ) -> io::Result<NodeProjectionHealth> {
        let state = self.lock()?;
        Ok(state
            .nodes
            .iter()
            .find(|node| node.node_id == registration.node_id)
            .filter(|node| node.tombstone_epoch == registration.tombstone_epoch)
            .map(CachedNode::health_snapshot)
            .unwrap_or_else(unobserved_health))
    }

    pub(crate) fn view(
        &self,
        registration: &NodeRegistrationSnapshot,
    ) -> io::Result<NodeProjectionView> {
        let state = self.lock()?;
        Ok(state
            .nodes
            .iter()
            .find(|node| node.node_id == registration.node_id)
            .filter(|node| node.tombstone_epoch == registration.tombstone_epoch)
            .map(|node| NodeProjectionView {
                health: node.health_snapshot(),
                projection: node.projection.clone(),
            })
            .unwrap_or_else(|| NodeProjectionView {
                health: unobserved_health(),
                projection: None,
            }))
    }

    pub(crate) fn cursor_and_generation(
        &self,
        registration: &NodeRegistrationSnapshot,
    ) -> io::Result<(Option<EventCursor>, u64)> {
        let state = self.lock()?;
        Ok(state
            .nodes
            .iter()
            .find(|node| {
                node.node_id == registration.node_id
                    && node.tombstone_epoch == registration.tombstone_epoch
            })
            .map(|node| (node.cursor.clone(), node.generation))
            .unwrap_or((None, 0)))
    }

    pub(crate) fn commit_projection(
        &self,
        registration: &NodeRegistrationSnapshot,
        expected_generation: u64,
        cursor: EventCursor,
        projection: NodeProjectionSnapshot,
        capabilities: Vec<String>,
        now_ms: u64,
    ) -> io::Result<Option<u64>> {
        validate_projection(&projection)?;
        validate_capabilities(&capabilities)?;
        if projection.node_id != registration.node_id {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "projection owner does not match the pinned Node identity",
            ));
        }
        let mut state = self.lock()?;
        let current = state
            .nodes
            .iter()
            .position(|node| node.node_id == registration.node_id);
        let actual_generation = current
            .filter(|index| state.nodes[*index].tombstone_epoch == registration.tombstone_epoch)
            .map_or(0, |index| state.nodes[index].generation);
        if actual_generation != expected_generation {
            return Ok(None);
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Node cache generation exhausted"))?;
        let replacement = CachedNode {
            node_id: registration.node_id.clone(),
            registration_revision: registration.revision,
            tombstone_epoch: registration.tombstone_epoch,
            generation,
            health: NodeProjectionHealthCode::Online,
            last_attempt_at_ms: Some(now_ms),
            last_success_at_ms: Some(now_ms),
            retry_at_ms: None,
            capabilities,
            cursor: Some(cursor),
            projection: Some(projection),
        };
        let mut next = state.clone();
        match current {
            Some(index) => next.nodes[index] = replacement,
            None if next.nodes.len() < MAX_NODES => next.nodes.push(replacement),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Node cache registration limit reached",
                ));
            }
        }
        sort_nodes(&mut next);
        save(&self.path, &next)?;
        *state = next;
        Ok(Some(generation))
    }

    pub(crate) fn mark_health(
        &self,
        registration: &NodeRegistrationSnapshot,
        expected_generation: u64,
        health: NodeProjectionHealthCode,
        attempt_at_ms: u64,
        retry_at_ms: Option<u64>,
    ) -> io::Result<Option<u64>> {
        let mut state = self.lock()?;
        let current = state
            .nodes
            .iter()
            .position(|node| node.node_id == registration.node_id);
        let actual_generation = current
            .filter(|index| state.nodes[*index].tombstone_epoch == registration.tombstone_epoch)
            .map_or(0, |index| state.nodes[index].generation);
        if actual_generation != expected_generation {
            return Ok(None);
        }
        let generation = expected_generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Node cache generation exhausted"))?;
        let mut replacement = current.map_or_else(
            || CachedNode {
                node_id: registration.node_id.clone(),
                registration_revision: registration.revision,
                tombstone_epoch: registration.tombstone_epoch,
                generation,
                health,
                last_attempt_at_ms: Some(attempt_at_ms),
                last_success_at_ms: None,
                retry_at_ms,
                capabilities: Vec::new(),
                cursor: None,
                projection: None,
            },
            |index| state.nodes[index].clone(),
        );
        replacement.registration_revision = registration.revision;
        replacement.tombstone_epoch = registration.tombstone_epoch;
        replacement.generation = generation;
        replacement.health = health;
        replacement.last_attempt_at_ms = Some(attempt_at_ms);
        replacement.retry_at_ms = retry_at_ms;
        let mut next = state.clone();
        match current {
            Some(index) => next.nodes[index] = replacement,
            None if next.nodes.len() < MAX_NODES => next.nodes.push(replacement),
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Node cache limit reached",
                ));
            }
        }
        sort_nodes(&mut next);
        save(&self.path, &next)?;
        *state = next;
        Ok(Some(generation))
    }

    pub(crate) fn remove(&self, node_id: &str) -> io::Result<bool> {
        let mut state = self.lock()?;
        let mut next = state.clone();
        next.nodes.retain(|node| node.node_id != node_id);
        if next.nodes.len() == state.nodes.len() {
            return Ok(false);
        }
        save(&self.path, &next)?;
        *state = next;
        Ok(true)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, CacheState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("Node cache lock is poisoned"))
    }
}

impl CachedNode {
    fn health_snapshot(&self) -> NodeProjectionHealth {
        NodeProjectionHealth {
            code: self.health,
            stale: self.health != NodeProjectionHealthCode::Online,
            cache_generation: self.generation,
            stream_id: self.cursor.as_ref().map(|cursor| cursor.stream_id.clone()),
            cursor: self.cursor.as_ref().map(|cursor| cursor.event_id),
            last_attempt_at_ms: self.last_attempt_at_ms,
            last_success_at_ms: self.last_success_at_ms,
            retry_at_ms: self.retry_at_ms,
            capabilities: self.capabilities.clone(),
        }
    }
}

fn unobserved_health() -> NodeProjectionHealth {
    NodeProjectionHealth {
        code: NodeProjectionHealthCode::Unobserved,
        stale: true,
        cache_generation: 0,
        stream_id: None,
        cursor: None,
        last_attempt_at_ms: None,
        last_success_at_ms: None,
        retry_at_ms: None,
        capabilities: Vec::new(),
    }
}

fn empty_state() -> CacheState {
    CacheState {
        version: NODE_CACHE_VERSION,
        nodes: Vec::new(),
    }
}

fn validate_projection(projection: &NodeProjectionSnapshot) -> io::Result<()> {
    if Uuid::parse_str(&projection.node_id).is_err()
        || projection.workspaces.len() > MAX_WORKSPACES_PER_NODE
        || projection.shells.len() > MAX_SHELLS_PER_NODE
        || projection.launchers.len() > MAX_LAUNCHERS_PER_NODE
        || projection.agents.len() > MAX_AGENTS_PER_NODE
        || projection.schedules.len() > MAX_SCHEDULES_PER_NODE
        || projection.executions.len()
            > usize::from(crate::protocol::MAX_NODE_PROJECTION_EXECUTIONS)
    {
        return Err(invalid("Node projection exceeds its structural bounds"));
    }
    let mut ids = HashSet::new();
    for workspace in &projection.workspaces {
        validate_name(&workspace.id)?;
        validate_name(&workspace.name)?;
        if !ids.insert(("workspace", workspace.id.as_str())) {
            return Err(invalid("Node projection contains duplicate workspace IDs"));
        }
    }
    for name in projection
        .shells
        .iter()
        .map(|item| item.name.as_str())
        .chain(projection.launchers.iter().map(|item| item.name.as_str()))
        .chain(projection.agents.iter().map(|item| item.name.as_str()))
        .chain(projection.schedules.iter().map(|item| item.name.as_str()))
    {
        validate_name(name)?;
    }
    Ok(())
}

fn validate_name(value: &str) -> io::Result<()> {
    if value.is_empty() || value.len() > MAX_NAME_BYTES || value.chars().any(char::is_control) {
        Err(invalid(
            "Node projection contains an invalid bounded string",
        ))
    } else {
        Ok(())
    }
}

fn validate_capabilities(capabilities: &[String]) -> io::Result<()> {
    if capabilities.len() > MAX_CAPABILITIES
        || capabilities.iter().any(|capability| {
            capability.is_empty()
                || capability.len() > MAX_CAPABILITY_BYTES
                || !capability
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        })
    {
        return Err(invalid("Node projection capabilities exceed their bounds"));
    }
    Ok(())
}

fn load(path: &Path) -> io::Result<CacheState> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Node cache is not an owner-only regular file",
        ));
    }
    if metadata.len() > MAX_CACHE_BYTES {
        return Err(invalid("Node cache exceeds the file-size bound"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    let state: CacheState =
        serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
    if state.version != NODE_CACHE_VERSION || state.nodes.len() > MAX_NODES {
        return Err(invalid("unsupported or oversized Node cache"));
    }
    let mut node_ids = HashSet::new();
    for node in &state.nodes {
        if !node_ids.insert(node.node_id.as_str()) || node.generation == 0 {
            return Err(invalid(
                "Node cache contains duplicate or invalid generations",
            ));
        }
        validate_capabilities(&node.capabilities)?;
        if let Some(projection) = &node.projection {
            if projection.node_id != node.node_id {
                return Err(invalid("Node cache projection identity mismatch"));
            }
            validate_projection(projection)?;
        }
    }
    Ok(state)
}

fn save(path: &Path, state: &CacheState) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node cache path has no parent"))?;
    secure_state_dir(parent)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return Err(invalid("Node cache exceeds the file-size bound"));
    }
    let temporary = parent.join(format!(".node-cache-{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        let _ = File::open(parent).and_then(|directory| directory.sync_all());
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn quarantine(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let quarantine = parent.join(format!("node-cache.corrupt-{}.json", Uuid::new_v4()));
    let _ = fs::rename(path, quarantine);
}

fn sort_nodes(state: &mut CacheState) {
    state
        .nodes
        .sort_by(|left, right| left.node_id.cmp(&right.node_id));
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{SchedulerHealth, SchedulerState};

    fn projection(node_id: String) -> NodeProjectionSnapshot {
        NodeProjectionSnapshot {
            node_id,
            workspaces: Vec::new(),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
            executions: Vec::new(),
            executions_truncated: false,
            scheduler: SchedulerHealth {
                state: SchedulerState::Active,
                max_concurrent: 4,
                active_executions: 0,
            },
        }
    }

    #[test]
    fn reduced_cache_rejects_rich_and_corrupt_payloads() {
        let projection = projection(Uuid::from_u128(2).to_string());
        let json = serde_json::to_value(&projection).unwrap();
        for private in [
            "cwd",
            "command",
            "prompt",
            "evidence",
            "environment",
            "external_session_id",
            "runner_token",
        ] {
            assert!(!json.to_string().contains(private));
        }
        let mut rich = json;
        rich.as_object_mut()
            .unwrap()
            .insert("prompt".into(), serde_json::json!("secret"));
        assert!(serde_json::from_value::<NodeProjectionSnapshot>(rich).is_err());
    }

    #[test]
    fn cache_commit_is_generation_conditional_and_corruption_is_quarantined() {
        let root = std::env::temp_dir().join(format!("boomux-node-cache-{}", Uuid::new_v4()));
        let path = root.join("boomux/node-cache.json");
        let node_id = Uuid::from_u128(2).to_string();
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: node_id.clone(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let cache = NodeProjectionCache::load_at(path.clone());
        assert_eq!(
            cache
                .commit_projection(
                    &registration,
                    0,
                    EventCursor {
                        stream_id: Uuid::new_v4().to_string(),
                        event_id: 7
                    },
                    projection(node_id),
                    vec!["protocol_32".into()],
                    10,
                )
                .unwrap(),
            Some(1)
        );
        assert_eq!(
            cache
                .mark_health(&registration, 0, NodeProjectionHealthCode::Stale, 11, None)
                .unwrap(),
            None
        );
        drop(cache);
        assert_eq!(
            NodeProjectionCache::load_at(path.clone())
                .health(&registration)
                .unwrap()
                .cursor,
            Some(7)
        );

        fs::write(&path, b"not-json").unwrap();
        let discarded = NodeProjectionCache::load_at(path.clone());
        assert_eq!(
            discarded.health(&registration).unwrap().code,
            NodeProjectionHealthCode::Unobserved
        );
        assert!(fs::read_dir(path.parent().unwrap()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("node-cache.corrupt-")
        }));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_view_preserves_every_stable_health_code() {
        let root = std::env::temp_dir().join(format!("boomux-node-health-{}", Uuid::new_v4()));
        let path = root.join("boomux/node-cache.json");
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: Uuid::from_u128(2).to_string(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let cache = NodeProjectionCache::load_at(path);
        let mut generation = 0;
        for code in [
            NodeProjectionHealthCode::Unobserved,
            NodeProjectionHealthCode::Online,
            NodeProjectionHealthCode::Reconnecting,
            NodeProjectionHealthCode::Stale,
            NodeProjectionHealthCode::Unreachable,
            NodeProjectionHealthCode::AuthenticationRequired,
            NodeProjectionHealthCode::IdentityChanged,
            NodeProjectionHealthCode::IdentityConflict,
            NodeProjectionHealthCode::Unsupported,
        ] {
            generation = cache
                .mark_health(&registration, generation, code, generation + 1, None)
                .unwrap()
                .unwrap();
            let view = cache.view(&registration).unwrap();
            assert_eq!(view.health.code, code);
            assert_eq!(view.health.stale, code != NodeProjectionHealthCode::Online);
            assert!(view.projection.is_none());
        }
        fs::remove_dir_all(root).unwrap();
    }
}
