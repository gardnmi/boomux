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

const NODE_CACHE_VERSION: u32 = 4;
const MAX_CACHE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_NODES: usize = 128;
const MAX_WORKSPACES_PER_NODE: usize = 1_024;
const MAX_SHELLS_PER_NODE: usize = 4_096;
const MAX_LAUNCHERS_PER_NODE: usize = 4_096;
const MAX_AGENTS_PER_NODE: usize = 4_096;
const MAX_SCHEDULES_PER_NODE: usize = 1_024;
const MAX_CAPABILITIES: usize = 96;
const MAX_CAPABILITY_BYTES: usize = 128;
const MAX_HELPER_VERSION_BYTES: usize = 128;
const MAX_NAME_BYTES: usize = 256;
const MAX_NOTIFICATION_CLAIMS_PER_NODE: usize = 512;
const MAX_DIGEST_CLAIMS_PER_NODE: usize = 128;
const MAX_NOTIFICATION_REASON_BYTES: usize = 64;
const MAX_DISMISSED_SHELLS_PER_NODE: usize = 4_096;

pub(crate) struct NodeProjectionCache {
    path: PathBuf,
    state: Mutex<CacheState>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectionCommit {
    pub(crate) generation: u64,
    pub(crate) previous_health: Option<NodeProjectionHealthCode>,
    pub(crate) previous_cursor: Option<EventCursor>,
}

pub(crate) struct ProjectionObservation {
    pub(crate) cursor: EventCursor,
    pub(crate) projection: NodeProjectionSnapshot,
    pub(crate) capabilities: Vec<String>,
    pub(crate) helper_version: String,
    pub(crate) observed_at_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteNotificationCategory {
    AgentBlocked,
    AgentCompleted,
    ScheduledDispatchFailed,
    ScheduledInterrupted,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteNotificationClaim {
    pub(crate) stream_id: String,
    pub(crate) entity_id: String,
    pub(crate) revision: u64,
    pub(crate) category: RemoteNotificationCategory,
    pub(crate) reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RemoteDigestClaim {
    pub(crate) stream_id: String,
    pub(crate) prior_cursor: u64,
    pub(crate) through_cursor: u64,
    pub(crate) enabled_categories: Vec<RemoteNotificationCategory>,
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
    #[serde(default)]
    observed_helper_version: Option<String>,
    cursor: Option<EventCursor>,
    projection: Option<NodeProjectionSnapshot>,
    notification_claims: Vec<RemoteNotificationClaim>,
    digest_claims: Vec<RemoteDigestClaim>,
    dismissed_shell_ids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionOneCacheState {
    version: u32,
    nodes: Vec<VersionOneCachedNode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionOneCachedNode {
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTwoCacheState {
    version: u32,
    nodes: Vec<VersionTwoCachedNode>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionTwoCachedNode {
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
    notification_claims: Vec<RemoteNotificationClaim>,
    digest_claims: Vec<RemoteDigestClaim>,
}

impl NodeProjectionCache {
    pub(crate) fn load_from_environment() -> Self {
        let path = state_directory_from_environment()
            .map(|directory| directory.join("node-cache.json"))
            .unwrap_or_default();
        Self::load_at(path)
    }

    fn load_at(path: PathBuf) -> Self {
        let (mut state, migrated) = match load(&path) {
            Ok(state) => state,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (empty_state(), false),
            Err(error) => {
                if !path.as_os_str().is_empty() {
                    quarantine(&path);
                }
                eprintln!("boomux: discarded invalid disposable Node cache: {error}");
                (empty_state(), false)
            }
        };
        let mut changed = migrated;
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
                projection: node.visible_projection(),
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
        observation: ProjectionObservation,
    ) -> io::Result<Option<ProjectionCommit>> {
        let ProjectionObservation {
            cursor,
            projection,
            capabilities,
            helper_version: observed_helper_version,
            observed_at_ms: now_ms,
        } = observation;
        validate_projection(&projection)?;
        validate_capabilities(&capabilities)?;
        validate_helper_version(&observed_helper_version)?;
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
        let (
            previous_health,
            previous_cursor,
            notification_claims,
            digest_claims,
            dismissed_shell_ids,
        ) = current
            .filter(|index| state.nodes[*index].tombstone_epoch == registration.tombstone_epoch)
            .map_or((None, None, Vec::new(), Vec::new(), Vec::new()), |index| {
                let node = &state.nodes[index];
                (
                    Some(node.health),
                    node.cursor.clone(),
                    node.notification_claims.clone(),
                    node.digest_claims.clone(),
                    node.dismissed_shell_ids.clone(),
                )
            });
        let shell_ids = projection
            .shells
            .iter()
            .map(|shell| shell.id.clone())
            .collect::<HashSet<_>>();
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
            observed_helper_version: Some(observed_helper_version),
            cursor: Some(cursor),
            projection: Some(projection),
            notification_claims,
            digest_claims,
            dismissed_shell_ids: dismissed_shell_ids
                .into_iter()
                .filter(|shell_id| shell_ids.contains(shell_id))
                .collect(),
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
        Ok(Some(ProjectionCommit {
            generation,
            previous_health,
            previous_cursor,
        }))
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
                observed_helper_version: None,
                cursor: None,
                projection: None,
                notification_claims: Vec::new(),
                digest_claims: Vec::new(),
                dismissed_shell_ids: Vec::new(),
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

    pub(crate) fn claim_notifications(
        &self,
        registration: &NodeRegistrationSnapshot,
        expected_cursor: &EventCursor,
        claims: &[RemoteNotificationClaim],
        digest: Option<&RemoteDigestClaim>,
    ) -> io::Result<Option<(Vec<bool>, bool)>> {
        for claim in claims {
            validate_notification_claim(claim)?;
        }
        if let Some(digest) = digest {
            validate_digest_claim(digest)?;
        }
        let mut state = self.lock()?;
        let Some(index) = state.nodes.iter().position(|node| {
            node.node_id == registration.node_id
                && node.tombstone_epoch == registration.tombstone_epoch
                && node.cursor.as_ref() == Some(expected_cursor)
        }) else {
            return Ok(None);
        };
        let mut replacement = state.nodes[index].clone();
        let mut accepted = Vec::with_capacity(claims.len());
        for claim in claims {
            if replacement.notification_claims.contains(claim) {
                accepted.push(false);
                continue;
            }
            replacement.notification_claims.push(claim.clone());
            accepted.push(true);
        }
        if replacement.notification_claims.len() > MAX_NOTIFICATION_CLAIMS_PER_NODE {
            let remove = replacement.notification_claims.len() - MAX_NOTIFICATION_CLAIMS_PER_NODE;
            replacement.notification_claims.drain(..remove);
        }
        let digest_accepted = digest.is_some_and(|digest| {
            if replacement.digest_claims.contains(digest) {
                false
            } else {
                replacement.digest_claims.push(digest.clone());
                true
            }
        });
        if replacement.digest_claims.len() > MAX_DIGEST_CLAIMS_PER_NODE {
            let remove = replacement.digest_claims.len() - MAX_DIGEST_CLAIMS_PER_NODE;
            replacement.digest_claims.drain(..remove);
        }
        if !accepted.iter().any(|accepted| *accepted) && !digest_accepted {
            return Ok(Some((accepted, false)));
        }
        let mut next = state.clone();
        next.nodes[index] = replacement;
        save(&self.path, &next)?;
        *state = next;
        Ok(Some((accepted, digest_accepted)))
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

    pub(crate) fn dismiss_shell(
        &self,
        registration: &NodeRegistrationSnapshot,
        shell_id: &str,
    ) -> io::Result<(NodeProjectionHealth, bool)> {
        validate_name(shell_id)?;
        let mut state = self.lock()?;
        let Some(index) = state.nodes.iter().position(|node| {
            node.node_id == registration.node_id
                && node.tombstone_epoch == registration.tombstone_epoch
        }) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Node has no cached projection",
            ));
        };
        let node = &state.nodes[index];
        if node.health == NodeProjectionHealthCode::Online {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "online Node shells must be closed by their owner",
            ));
        }
        if !node
            .projection
            .as_ref()
            .is_some_and(|projection| projection.shells.iter().any(|shell| shell.id == shell_id))
        {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "shell is not present in the cached Node projection",
            ));
        }
        if node.dismissed_shell_ids.iter().any(|id| id == shell_id) {
            return Ok((node.health_snapshot(), false));
        }
        if node.dismissed_shell_ids.len() >= MAX_DISMISSED_SHELLS_PER_NODE {
            return Err(invalid("dismissed Node shell limit reached"));
        }
        let mut next = state.clone();
        let node = &mut next.nodes[index];
        node.dismissed_shell_ids.push(shell_id.to_owned());
        node.dismissed_shell_ids.sort();
        save(&self.path, &next)?;
        let health = next.nodes[index].health_snapshot();
        *state = next;
        Ok((health, true))
    }

    pub(crate) fn restore_dismissed_shells(
        &self,
        registration: &NodeRegistrationSnapshot,
    ) -> io::Result<(NodeProjectionHealth, bool)> {
        let mut state = self.lock()?;
        let Some(index) = state.nodes.iter().position(|node| {
            node.node_id == registration.node_id
                && node.tombstone_epoch == registration.tombstone_epoch
        }) else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Node has no cached projection",
            ));
        };
        if state.nodes[index].dismissed_shell_ids.is_empty() {
            return Ok((state.nodes[index].health_snapshot(), false));
        }
        let mut next = state.clone();
        next.nodes[index].dismissed_shell_ids.clear();
        save(&self.path, &next)?;
        let health = next.nodes[index].health_snapshot();
        *state = next;
        Ok((health, true))
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
            observed_helper_version: self.observed_helper_version.clone(),
        }
    }

    fn visible_projection(&self) -> Option<NodeProjectionSnapshot> {
        let mut projection = self.projection.clone()?;
        if self.dismissed_shell_ids.is_empty() {
            return Some(projection);
        }
        let dismissed = self
            .dismissed_shell_ids
            .iter()
            .map(String::as_str)
            .collect::<HashSet<_>>();
        projection
            .shells
            .retain(|shell| !dismissed.contains(shell.id.as_str()));
        projection
            .agents
            .retain(|agent| !dismissed.contains(agent.shell_id.as_str()));
        for workspace in &mut projection.workspaces {
            let item_count = projection
                .shells
                .iter()
                .filter(|shell| shell.workspace_id == workspace.id)
                .count()
                + projection
                    .launchers
                    .iter()
                    .filter(|launcher| launcher.workspace_id == workspace.id)
                    .count()
                + projection
                    .schedules
                    .iter()
                    .filter(|schedule| schedule.workspace_id == workspace.id)
                    .count();
            let attention_count = projection
                .agents
                .iter()
                .filter(|agent| agent.workspace_id == workspace.id && agent.attention.is_some())
                .count();
            workspace.item_count = u32::try_from(item_count).unwrap_or(u32::MAX);
            workspace.attention_count = u32::try_from(attention_count).unwrap_or(u32::MAX);
        }
        Some(projection)
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
        observed_helper_version: None,
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

fn validate_helper_version(version: &str) -> io::Result<()> {
    if version.is_empty()
        || version.len() > MAX_HELPER_VERSION_BYTES
        || !version.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(invalid("Node helper version exceeds its bounds"));
    }
    Ok(())
}

fn load(path: &Path) -> io::Result<(CacheState, bool)> {
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
    let version = serde_json::from_slice::<serde_json::Value>(&bytes)
        .map_err(|error| invalid(error.to_string()))?
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| invalid("Node cache version is missing"))?;
    let (state, migrated) = match version {
        1 => {
            let old: VersionOneCacheState =
                serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
            if old.version != 1 {
                return Err(invalid("invalid Node cache version"));
            }
            (
                CacheState {
                    version: NODE_CACHE_VERSION,
                    nodes: old
                        .nodes
                        .into_iter()
                        .map(|node| CachedNode {
                            node_id: node.node_id,
                            registration_revision: node.registration_revision,
                            tombstone_epoch: node.tombstone_epoch,
                            generation: node.generation,
                            health: node.health,
                            last_attempt_at_ms: node.last_attempt_at_ms,
                            last_success_at_ms: node.last_success_at_ms,
                            retry_at_ms: node.retry_at_ms,
                            capabilities: node.capabilities,
                            observed_helper_version: None,
                            cursor: node.cursor,
                            projection: node.projection,
                            notification_claims: Vec::new(),
                            digest_claims: Vec::new(),
                            dismissed_shell_ids: Vec::new(),
                        })
                        .collect(),
                },
                true,
            )
        }
        2 => {
            let old: VersionTwoCacheState =
                serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
            if old.version != 2 {
                return Err(invalid("invalid Node cache version"));
            }
            (
                CacheState {
                    version: NODE_CACHE_VERSION,
                    nodes: old
                        .nodes
                        .into_iter()
                        .map(|node| CachedNode {
                            node_id: node.node_id,
                            registration_revision: node.registration_revision,
                            tombstone_epoch: node.tombstone_epoch,
                            generation: node.generation,
                            health: node.health,
                            last_attempt_at_ms: node.last_attempt_at_ms,
                            last_success_at_ms: node.last_success_at_ms,
                            retry_at_ms: node.retry_at_ms,
                            capabilities: node.capabilities,
                            observed_helper_version: None,
                            cursor: node.cursor,
                            projection: node.projection,
                            notification_claims: node.notification_claims,
                            digest_claims: node.digest_claims,
                            dismissed_shell_ids: Vec::new(),
                        })
                        .collect(),
                },
                true,
            )
        }
        3 => {
            let mut old: CacheState =
                serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?;
            if old.version != 3 {
                return Err(invalid("invalid Node cache version"));
            }
            old.version = NODE_CACHE_VERSION;
            (old, true)
        }
        4 => (
            serde_json::from_slice(&bytes).map_err(|error| invalid(error.to_string()))?,
            false,
        ),
        _ => return Err(invalid("unsupported Node cache version")),
    };
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
        if let Some(version) = &node.observed_helper_version {
            validate_helper_version(version)?;
        }
        if node.notification_claims.len() > MAX_NOTIFICATION_CLAIMS_PER_NODE
            || node.digest_claims.len() > MAX_DIGEST_CLAIMS_PER_NODE
            || node.dismissed_shell_ids.len() > MAX_DISMISSED_SHELLS_PER_NODE
        {
            return Err(invalid("Node cache metadata exceeds its bounds"));
        }
        let mut dismissed = HashSet::new();
        for shell_id in &node.dismissed_shell_ids {
            validate_name(shell_id)?;
            if !dismissed.insert(shell_id) {
                return Err(invalid("Node cache contains duplicate dismissed shells"));
            }
        }
        for claim in &node.notification_claims {
            validate_notification_claim(claim)?;
        }
        for claim in &node.digest_claims {
            validate_digest_claim(claim)?;
        }
        if let Some(projection) = &node.projection {
            if projection.node_id != node.node_id {
                return Err(invalid("Node cache projection identity mismatch"));
            }
            validate_projection(projection)?;
        }
    }
    Ok((state, migrated))
}

fn validate_notification_claim(claim: &RemoteNotificationClaim) -> io::Result<()> {
    if Uuid::parse_str(&claim.stream_id).is_err()
        || claim.entity_id.is_empty()
        || claim.entity_id.len() > MAX_NAME_BYTES
        || claim.revision == 0
        || claim.reason.is_empty()
        || claim.reason.len() > MAX_NOTIFICATION_REASON_BYTES
        || claim.reason.chars().any(char::is_control)
    {
        return Err(invalid("Node notification claim is invalid"));
    }
    Ok(())
}

fn validate_digest_claim(claim: &RemoteDigestClaim) -> io::Result<()> {
    let mut categories = claim.enabled_categories.clone();
    categories.sort_unstable();
    categories.dedup();
    if Uuid::parse_str(&claim.stream_id).is_err()
        || claim.prior_cursor > claim.through_cursor
        || categories.is_empty()
        || categories != claim.enabled_categories
    {
        return Err(invalid("Node notification digest claim is invalid"));
    }
    Ok(())
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
    use crate::protocol::{
        AgentAttentionReason, AgentState, NodeProjectionAgent, NodeProjectionAttention,
        NodeProjectionShell, NodeProjectionWorkspace, SchedulerHealth, SchedulerState, ShellOwner,
        ShellStatus,
    };

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

    fn projection_with_shell(node_id: String) -> NodeProjectionSnapshot {
        let mut projection = projection(node_id);
        projection.workspaces.push(NodeProjectionWorkspace {
            id: "workspace-1".into(),
            name: "project".into(),
            item_count: 1,
            attention_count: 1,
        });
        projection.shells.push(NodeProjectionShell {
            id: "shell-1".into(),
            workspace_id: "workspace-1".into(),
            name: "agent".into(),
            owner: ShellOwner::User,
            status: ShellStatus::Running,
            run_id: Some("run-1".into()),
            generation: Some(1),
            started_at_ms: Some(1),
            ended_at_ms: None,
            recovered_agent_id: None,
        });
        projection.agents.push(NodeProjectionAgent {
            id: "agent-1".into(),
            workspace_id: "workspace-1".into(),
            shell_id: "shell-1".into(),
            run_id: "run-1".into(),
            name: "agent".into(),
            integration: "opencode".into(),
            state: AgentState::Working,
            observation_revision: 1,
            observed_at_ms: 1,
            started_at_ms: 1,
            ended_at_ms: None,
            attention: Some(NodeProjectionAttention {
                reason: AgentAttentionReason::Blocked,
                observation_revision: 1,
                observed_at_ms: 1,
            }),
        });
        projection
    }

    #[test]
    fn dismissed_shells_are_persisted_hidden_restorable_and_owner_pruned() {
        let root = std::env::temp_dir().join(format!("boomux-node-dismiss-{}", Uuid::new_v4()));
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
        let cursor = EventCursor {
            stream_id: Uuid::new_v4().to_string(),
            event_id: 1,
        };
        let generation = cache
            .commit_projection(
                &registration,
                0,
                ProjectionObservation {
                    cursor: cursor.clone(),
                    projection: projection_with_shell(node_id.clone()),
                    capabilities: vec!["protocol_40".into()],
                    helper_version: "0.20.0".into(),
                    observed_at_ms: 1,
                },
            )
            .unwrap()
            .unwrap()
            .generation;
        assert!(cache.dismiss_shell(&registration, "shell-1").is_err());
        let generation = cache
            .mark_health(
                &registration,
                generation,
                NodeProjectionHealthCode::Unreachable,
                2,
                None,
            )
            .unwrap()
            .unwrap();
        let (health, changed) = cache.dismiss_shell(&registration, "shell-1").unwrap();
        assert!(changed);
        assert_eq!(health.cache_generation, generation);
        let view = cache.view(&registration).unwrap().projection.unwrap();
        assert!(view.shells.is_empty());
        assert!(view.agents.is_empty());
        assert_eq!(view.workspaces[0].item_count, 0);
        assert_eq!(view.workspaces[0].attention_count, 0);
        let claim = RemoteNotificationClaim {
            stream_id: cursor.stream_id.clone(),
            entity_id: "agent-1".into(),
            revision: 1,
            category: RemoteNotificationCategory::AgentBlocked,
            reason: "blocked".into(),
        };
        assert_eq!(
            cache
                .claim_notifications(&registration, &cursor, &[claim], None)
                .unwrap(),
            Some((vec![true], false))
        );

        drop(cache);
        let cache = NodeProjectionCache::load_at(path.clone());
        assert!(
            cache
                .view(&registration)
                .unwrap()
                .projection
                .unwrap()
                .shells
                .is_empty()
        );
        let generation = cache.health(&registration).unwrap().cache_generation;
        let generation = cache
            .commit_projection(
                &registration,
                generation,
                ProjectionObservation {
                    cursor: EventCursor {
                        stream_id: Uuid::new_v4().to_string(),
                        event_id: 2,
                    },
                    projection: projection_with_shell(node_id.clone()),
                    capabilities: vec!["protocol_40".into()],
                    helper_version: "0.20.0".into(),
                    observed_at_ms: 3,
                },
            )
            .unwrap()
            .unwrap()
            .generation;
        assert!(
            cache
                .view(&registration)
                .unwrap()
                .projection
                .unwrap()
                .shells
                .is_empty()
        );
        let (_, changed) = cache.restore_dismissed_shells(&registration).unwrap();
        assert!(changed);
        assert_eq!(
            cache.health(&registration).unwrap().cache_generation,
            generation
        );
        assert_eq!(
            cache
                .view(&registration)
                .unwrap()
                .projection
                .unwrap()
                .shells
                .len(),
            1
        );

        let generation = cache.health(&registration).unwrap().cache_generation;
        cache
            .mark_health(
                &registration,
                generation,
                NodeProjectionHealthCode::Stale,
                4,
                None,
            )
            .unwrap();
        cache.dismiss_shell(&registration, "shell-1").unwrap();
        let generation = cache.health(&registration).unwrap().cache_generation;
        cache
            .commit_projection(
                &registration,
                generation,
                ProjectionObservation {
                    cursor: EventCursor {
                        stream_id: Uuid::new_v4().to_string(),
                        event_id: 3,
                    },
                    projection: projection(node_id),
                    capabilities: vec!["protocol_40".into()],
                    helper_version: "0.20.0".into(),
                    observed_at_ms: 5,
                },
            )
            .unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            persisted["nodes"][0]["dismissed_shell_ids"],
            serde_json::json!([])
        );
        fs::remove_dir_all(root).unwrap();
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
                    ProjectionObservation {
                        cursor: EventCursor {
                            stream_id: Uuid::new_v4().to_string(),
                            event_id: 7,
                        },
                        projection: projection(node_id),
                        capabilities: vec!["protocol_32".into()],
                        helper_version: "0.41.0".into(),
                        observed_at_ms: 10,
                    },
                )
                .unwrap()
                .map(|commit| commit.generation),
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

    #[test]
    fn schema_two_migrates_to_empty_dismissed_shells() {
        let root = std::env::temp_dir().join(format!("boomux-node-v2-{}", Uuid::new_v4()));
        let path = root.join("boomux/node-cache.json");
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: Uuid::from_u128(2).to_string(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let cache = NodeProjectionCache::load_at(path.clone());
        cache
            .mark_health(&registration, 0, NodeProjectionHealthCode::Stale, 1, None)
            .unwrap();
        drop(cache);

        let mut old: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        old["version"] = serde_json::json!(2);
        old["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("dismissed_shell_ids");
        old["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("observed_helper_version");
        fs::write(&path, serde_json::to_vec_pretty(&old).unwrap()).unwrap();

        drop(NodeProjectionCache::load_at(path.clone()));
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["version"], 4);
        assert_eq!(
            migrated["nodes"][0]["dismissed_shell_ids"],
            serde_json::json!([])
        );
        assert_eq!(
            migrated["nodes"][0]["observed_helper_version"],
            serde_json::Value::Null
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_one_migrates_explicitly_and_notification_claims_are_at_most_once() {
        let root = std::env::temp_dir().join(format!("boomux-node-claims-{}", Uuid::new_v4()));
        let path = root.join("boomux/node-cache.json");
        let node_id = Uuid::from_u128(2).to_string();
        let stream_id = Uuid::from_u128(3).to_string();
        let registration = NodeRegistrationSnapshot {
            alias: "work".into(),
            target: "work.example".into(),
            node_id: node_id.clone(),
            revision: 1,
            tombstone_epoch: 0,
        };
        let cache = NodeProjectionCache::load_at(path.clone());
        let cursor = EventCursor {
            stream_id: stream_id.clone(),
            event_id: 7,
        };
        cache
            .commit_projection(
                &registration,
                0,
                ProjectionObservation {
                    cursor: EventCursor {
                        stream_id: stream_id.clone(),
                        event_id: 7,
                    },
                    projection: projection(node_id),
                    capabilities: vec!["protocol_32".into()],
                    helper_version: "0.41.0".into(),
                    observed_at_ms: 10,
                },
            )
            .unwrap()
            .unwrap();
        drop(cache);

        let mut version_one: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        version_one["version"] = serde_json::json!(1);
        version_one["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("notification_claims");
        version_one["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("digest_claims");
        version_one["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("dismissed_shell_ids");
        version_one["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("observed_helper_version");
        fs::write(&path, serde_json::to_vec_pretty(&version_one).unwrap()).unwrap();
        let cache = NodeProjectionCache::load_at(path.clone());
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated["version"], 4);
        assert_eq!(
            migrated["nodes"][0]["notification_claims"],
            serde_json::json!([])
        );
        assert_eq!(migrated["nodes"][0]["digest_claims"], serde_json::json!([]));
        assert_eq!(
            migrated["nodes"][0]["dismissed_shell_ids"],
            serde_json::json!([])
        );

        let claim = RemoteNotificationClaim {
            stream_id: stream_id.clone(),
            entity_id: "agent-1".into(),
            revision: 2,
            category: RemoteNotificationCategory::AgentBlocked,
            reason: "blocked".into(),
        };
        let digest = RemoteDigestClaim {
            stream_id: stream_id.clone(),
            prior_cursor: 7,
            through_cursor: 9,
            enabled_categories: vec![RemoteNotificationCategory::AgentBlocked],
        };
        assert_eq!(
            cache
                .claim_notifications(
                    &registration,
                    &cursor,
                    std::slice::from_ref(&claim),
                    Some(&digest),
                )
                .unwrap(),
            Some((vec![true], true))
        );
        assert_eq!(
            cache
                .claim_notifications(&registration, &cursor, &[claim], Some(&digest))
                .unwrap(),
            Some((vec![false], false))
        );
        drop(cache);
        let cache = NodeProjectionCache::load_at(path);
        assert_eq!(
            cache
                .claim_notifications(&registration, &cursor, &[], Some(&digest))
                .unwrap(),
            Some((Vec::new(), false))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_three_migrates_helper_version_as_unobserved_and_validates_bounds() {
        let root = std::env::temp_dir().join(format!("boomux-node-v3-{}", Uuid::new_v4()));
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
        cache
            .commit_projection(
                &registration,
                0,
                ProjectionObservation {
                    cursor: EventCursor {
                        stream_id: Uuid::from_u128(3).to_string(),
                        event_id: 7,
                    },
                    projection: projection(node_id),
                    capabilities: vec!["protocol_41".into()],
                    helper_version: "0.41.0".into(),
                    observed_at_ms: 10,
                },
            )
            .unwrap();
        drop(cache);

        let mut version_three: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        version_three["version"] = serde_json::json!(3);
        version_three["nodes"][0]
            .as_object_mut()
            .unwrap()
            .remove("observed_helper_version");
        fs::write(&path, serde_json::to_vec_pretty(&version_three).unwrap()).unwrap();

        let migrated = NodeProjectionCache::load_at(path.clone());
        assert_eq!(
            migrated
                .health(&registration)
                .unwrap()
                .observed_helper_version,
            None
        );
        let stored: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(stored["version"], 4);
        assert_eq!(
            stored["nodes"][0]["observed_helper_version"],
            serde_json::Value::Null
        );

        let generation = migrated.cursor_and_generation(&registration).unwrap().1;
        let error = migrated
            .commit_projection(
                &registration,
                generation,
                ProjectionObservation {
                    cursor: EventCursor {
                        stream_id: Uuid::from_u128(3).to_string(),
                        event_id: 8,
                    },
                    projection: projection(registration.node_id.clone()),
                    capabilities: vec!["protocol_41".into()],
                    helper_version: "x".repeat(MAX_HELPER_VERSION_BYTES + 1),
                    observed_at_ms: 11,
                },
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        fs::remove_dir_all(root).unwrap();
    }
}
