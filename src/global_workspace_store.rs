use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::{
    GlobalWorkspaceSnapshot, RoutedOperationResult, Snapshot, WorkspacePlacementSnapshot,
    WorkspacePlacementState, WorkspaceSnapshot,
};
use crate::state_store::{effective_uid, secure_state_dir, state_directory_from_environment};

const COORDINATOR_WORKSPACE_VERSION: u32 = 6;
const MAX_STORE_BYTES: u64 = 1024 * 1024;
const MAX_WORKSPACES: usize = 1_024;
const MAX_PLACEMENTS: usize = 128;
const MAX_PENDING_RESOURCES: usize = 1_024;
const MAX_COMPLETED_OPERATIONS: usize = 256;
const MAX_COMPLETION_RESERVATION_BYTES: usize = 768 * 1024;
const COMPLETION_RESERVATION_OVERHEAD: usize = 64 * 1024;

pub(crate) struct GlobalWorkspaceStore {
    path: PathBuf,
    state: Mutex<PersistedGlobalWorkspaces>,
    persist: bool,
}

pub(crate) struct GlobalWorkspaceTransaction<'a> {
    target: std::sync::MutexGuard<'a, PersistedGlobalWorkspaces>,
    staged: GlobalWorkspaceStore,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedGlobalWorkspaces {
    version: u32,
    local_migration_complete: bool,
    workspaces: Vec<GlobalWorkspaceSnapshot>,
    #[serde(default)]
    pending_resources: Vec<PendingWorkspaceResource>,
    #[serde(default)]
    completed_operations: Vec<CompletedWorkspaceOperation>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingResourceKind {
    Shell,
    Launcher,
    AgentSchedule,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingWorkspaceResource {
    #[serde(default)]
    pub(crate) operation_id: String,
    #[serde(default)]
    pub(crate) creates_workspace: bool,
    #[serde(default)]
    pub(crate) request_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) semantic_fingerprint: Option<String>,
    #[serde(default)]
    completion_reservation: String,
    #[serde(default)]
    pub(crate) owner_attempted: bool,
    pub(crate) global_workspace_id: String,
    pub(crate) expected_global_revision: u64,
    pub(crate) node_id: String,
    #[serde(default)]
    pub(crate) requested_owner_workspace_id: String,
    pub(crate) owner_workspace_id: String,
    pub(crate) owner_workspace_name: String,
    pub(crate) default_cwd: Option<PathBuf>,
    pub(crate) resource_id: String,
    pub(crate) kind: PendingResourceKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CompletedWorkspaceOperation {
    pub(crate) operation_id: String,
    pub(crate) request_fingerprint: String,
    #[serde(default)]
    pub(crate) semantic_fingerprint: Option<String>,
    #[serde(default)]
    pub(crate) creates_workspace: bool,
    pub(crate) workspace: GlobalWorkspaceSnapshot,
    pub(crate) resource: RoutedOperationResult,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreparedWorkspaceResource {
    Pending {
        pending: Box<PendingWorkspaceResource>,
        newly_prepared: bool,
    },
    Completed(Box<CompletedWorkspaceOperation>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PreparedWorkspaceShell {
    Pending {
        workspace: GlobalWorkspaceSnapshot,
        pending: Box<PendingWorkspaceResource>,
    },
    Completed(Box<CompletedWorkspaceOperation>),
}

impl GlobalWorkspaceStore {
    pub(crate) fn load_from_environment() -> io::Result<Self> {
        let path = state_directory_from_environment()?.join("global_workspaces.json");
        Self::load_at(path)
    }

    fn load_at(path: PathBuf) -> io::Result<Self> {
        let (state, migrated) = match load(&path) {
            Ok(mut state) if (1..COORDINATOR_WORKSPACE_VERSION).contains(&state.version) => {
                for pending in &mut state.pending_resources {
                    if pending.operation_id.is_empty() {
                        pending.operation_id.clone_from(&pending.resource_id);
                    }
                    if pending.requested_owner_workspace_id.is_empty() {
                        pending
                            .requested_owner_workspace_id
                            .clone_from(&pending.owner_workspace_id);
                    }
                    // Older schemas cannot prove whether the owner request crossed
                    // the transport boundary, so migration must retain recovery.
                    pending.owner_attempted = true;
                }
                state.version = COORDINATOR_WORKSPACE_VERSION;
                (state, true)
            }
            Ok(state) => (state, false),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (
                PersistedGlobalWorkspaces {
                    version: COORDINATOR_WORKSPACE_VERSION,
                    local_migration_complete: false,
                    workspaces: Vec::new(),
                    pending_resources: Vec::new(),
                    completed_operations: Vec::new(),
                },
                false,
            ),
            Err(error) => return Err(error),
        };
        validate(&state)?;
        if migrated {
            save(&path, &state)?;
        }
        Ok(Self {
            path,
            state: Mutex::new(state),
            persist: true,
        })
    }

    pub(crate) fn transaction(&self) -> io::Result<GlobalWorkspaceTransaction<'_>> {
        let target = self.lock()?;
        let staged = Self {
            path: self.path.clone(),
            state: Mutex::new(target.clone()),
            persist: false,
        };
        Ok(GlobalWorkspaceTransaction { target, staged })
    }

    pub(crate) fn checkpoint(&self) -> io::Result<()> {
        let state = self.lock()?;
        validate(&state)?;
        save(&self.path, &state)
    }

    pub(crate) fn initialize_local_once(
        &self,
        node_id: &str,
        snapshot: &Snapshot,
    ) -> io::Result<bool> {
        let mut state = self.lock()?;
        if state.local_migration_complete {
            return Ok(false);
        }
        let mut replacement = state.clone();
        for workspace in &snapshot.workspaces {
            if replacement.workspaces.len() >= MAX_WORKSPACES {
                return Err(invalid(
                    "coordinator Workspace limit reached during migration",
                ));
            }
            replacement.workspaces.push(GlobalWorkspaceSnapshot {
                id: workspace.id.clone(),
                revision: 1,
                name: workspace.name.clone(),
                closing: false,
                placements: vec![placement(
                    node_id,
                    workspace,
                    WorkspacePlacementState::Active,
                )],
            });
        }
        replacement.local_migration_complete = true;
        validate(&replacement)?;
        save(&self.path, &replacement)?;
        *state = replacement;
        Ok(true)
    }

    pub(crate) fn list(&self) -> io::Result<Vec<GlobalWorkspaceSnapshot>> {
        let mut workspaces = self.lock()?.workspaces.clone();
        workspaces.sort_by(|left, right| {
            left.name
                .cmp(&right.name)
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(workspaces)
    }

    pub(crate) fn pending_resources(&self) -> io::Result<Vec<PendingWorkspaceResource>> {
        Ok(self.lock()?.pending_resources.clone())
    }

    pub(crate) fn completed_operation(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
    ) -> io::Result<Option<CompletedWorkspaceOperation>> {
        let state = self.lock()?;
        match state
            .completed_operations
            .iter()
            .find(|completed| completed.operation_id == operation_id)
        {
            Some(completed) if completed.request_fingerprint == request_fingerprint => {
                Ok(Some(completed.clone()))
            }
            Some(_) => Err(operation_conflict()),
            None => Ok(None),
        }
    }

    pub(crate) fn get(&self, id: &str) -> io::Result<GlobalWorkspaceSnapshot> {
        self.lock()?
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .cloned()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "global Workspace not found"))
    }

    pub(crate) fn create(&self, name: String) -> io::Result<GlobalWorkspaceSnapshot> {
        validate_name(&name)?;
        self.mutate(|state| {
            if state
                .workspaces
                .iter()
                .any(|workspace| workspace.name == name)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "global Workspace name already exists",
                ));
            }
            if state.workspaces.len() >= MAX_WORKSPACES {
                return Err(invalid("coordinator Workspace limit reached"));
            }
            let workspace = GlobalWorkspaceSnapshot {
                id: Uuid::new_v4().to_string(),
                revision: 1,
                name,
                closing: false,
                placements: Vec::new(),
            };
            state.workspaces.push(workspace.clone());
            Ok(workspace)
        })
    }

    pub(crate) fn adopt(
        &self,
        node_id: &str,
        owner: &WorkspaceSnapshot,
        expected_revision: u64,
    ) -> io::Result<GlobalWorkspaceSnapshot> {
        require_revision(owner.revision, expected_revision, "owner Workspace")?;
        self.mutate(|state| {
            ensure_owner_unlinked(state, node_id, &owner.id)?;
            if state
                .workspaces
                .iter()
                .any(|workspace| workspace.name == owner.name)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "global Workspace name already exists; equal names are never merged",
                ));
            }
            let workspace = GlobalWorkspaceSnapshot {
                id: Uuid::new_v4().to_string(),
                revision: 1,
                name: owner.name.clone(),
                closing: false,
                placements: vec![placement(node_id, owner, WorkspacePlacementState::Active)],
            };
            state.workspaces.push(workspace.clone());
            Ok(workspace)
        })
    }

    pub(crate) fn link(
        &self,
        global_id: &str,
        expected_global_revision: u64,
        node_id: &str,
        owner: &WorkspaceSnapshot,
        expected_owner_revision: u64,
    ) -> io::Result<GlobalWorkspaceSnapshot> {
        require_revision(owner.revision, expected_owner_revision, "owner Workspace")?;
        self.mutate(|state| {
            ensure_owner_unlinked(state, node_id, &owner.id)?;
            ensure_no_pending_resource(state, global_id)?;
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == global_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            require_revision(
                workspace.revision,
                expected_global_revision,
                "global Workspace",
            )?;
            if workspace.closing {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "global Workspace close is in progress",
                ));
            }
            if workspace
                .placements
                .iter()
                .any(|placement| placement.node_id == node_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "Workspace already has a placement on this Node",
                ));
            }
            workspace.revision = next(workspace.revision)?;
            workspace
                .placements
                .push(placement(node_id, owner, WorkspacePlacementState::Active));
            workspace
                .placements
                .sort_by(|left, right| left.node_id.cmp(&right.node_id));
            Ok(workspace.clone())
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_resource(
        &self,
        global_id: &str,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        expected_global_revision: u64,
        node_id: &str,
        requested_owner_workspace_id: &str,
        owner_workspace_name: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
    ) -> io::Result<PreparedWorkspaceResource> {
        self.prepare_resource_inner(
            global_id,
            operation_id,
            request_fingerprint,
            request_bytes,
            expected_global_revision,
            node_id,
            requested_owner_workspace_id,
            owner_workspace_name,
            default_cwd,
            resource_id,
            kind,
            false,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_resource_for_attempt(
        &self,
        global_id: &str,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        expected_global_revision: u64,
        node_id: &str,
        requested_owner_workspace_id: &str,
        owner_workspace_name: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
    ) -> io::Result<PreparedWorkspaceResource> {
        self.prepare_resource_inner(
            global_id,
            operation_id,
            request_fingerprint,
            request_bytes,
            expected_global_revision,
            node_id,
            requested_owner_workspace_id,
            owner_workspace_name,
            default_cwd,
            resource_id,
            kind,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_resource_inner(
        &self,
        global_id: &str,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        expected_global_revision: u64,
        node_id: &str,
        requested_owner_workspace_id: &str,
        owner_workspace_name: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
        owner_attempted: bool,
    ) -> io::Result<PreparedWorkspaceResource> {
        validate_name(owner_workspace_name)?;
        self.mutate(|state| {
            if let Some(completed) = state
                .completed_operations
                .iter()
                .find(|completed| completed.operation_id == operation_id)
            {
                if completed.request_fingerprint != request_fingerprint {
                    return Err(operation_conflict());
                }
                return Ok(PreparedWorkspaceResource::Completed(Box::new(
                    completed.clone(),
                )));
            }
            if let Some(index) = state
                .pending_resources
                .iter()
                .position(|pending| pending.operation_id == operation_id)
            {
                let pending = &state.pending_resources[index];
                if pending.global_workspace_id != global_id
                    || pending.expected_global_revision != expected_global_revision
                    || pending.node_id != node_id
                    || pending.requested_owner_workspace_id != requested_owner_workspace_id
                    || pending.owner_workspace_name != owner_workspace_name
                    || pending.default_cwd != default_cwd
                    || pending.resource_id != resource_id
                    || pending.kind != kind
                    || pending
                        .request_fingerprint
                        .as_deref()
                        .is_some_and(|fingerprint| fingerprint != request_fingerprint)
                {
                    return Err(operation_conflict());
                }
                if state.pending_resources[index].request_fingerprint.is_none() {
                    state.pending_resources[index].request_fingerprint =
                        Some(request_fingerprint.to_owned());
                }
                if state.pending_resources[index]
                    .completion_reservation
                    .is_empty()
                {
                    let workspace = state
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == global_id)
                        .ok_or_else(|| invalid("prepared Workspace is missing"))?;
                    let projected = projected_workspace_with_pending(state, workspace, None)?;
                    let reservation = completion_reservation_bytes(&projected, request_bytes)?;
                    state.pending_resources[index].completion_reservation = " ".repeat(reservation);
                    compact_prepared_state(state)?;
                }
                if owner_attempted {
                    state.pending_resources[index].owner_attempted = true;
                }
                return Ok(PreparedWorkspaceResource::Pending {
                    pending: Box::new(state.pending_resources[index].clone()),
                    newly_prepared: false,
                });
            }
            if state
                .pending_resources
                .iter()
                .any(|pending| pending.node_id == node_id && pending.resource_id == resource_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "resource UUID is already reserved by another prepared operation",
                ));
            }
            if state.pending_resources.len() >= MAX_PENDING_RESOURCES {
                return Err(invalid("coordinator pending resource limit reached"));
            }
            let workspace = state
                .workspaces
                .iter()
                .find(|workspace| workspace.id == global_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            require_revision(
                workspace.revision,
                expected_global_revision,
                "global Workspace",
            )?;
            if workspace.closing {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "global Workspace close is in progress",
                ));
            }
            if let Some(placement) = workspace
                .placements
                .iter()
                .find(|placement| placement.node_id == node_id)
            {
                ensure_owner_unlinked_except(state, global_id, node_id, &placement.workspace_id)?;
            }
            let prepared_owner = state.pending_resources.iter().find(|pending| {
                pending.global_workspace_id == global_id && pending.node_id == node_id
            });
            let effective_owner_workspace_id = workspace
                .placements
                .iter()
                .find(|placement| placement.node_id == node_id)
                .map(|placement| placement.workspace_id.clone())
                .or_else(|| prepared_owner.map(|pending| pending.owner_workspace_id.clone()))
                .unwrap_or_else(|| requested_owner_workspace_id.to_owned());
            ensure_owner_unlinked_except(state, global_id, node_id, &effective_owner_workspace_id)?;
            let effective_owner_name = prepared_owner.map_or_else(
                || owner_workspace_name.to_owned(),
                |pending| pending.owner_workspace_name.clone(),
            );
            let effective_cwd =
                prepared_owner.map_or(default_cwd, |pending| pending.default_cwd.clone());
            let mut pending = PendingWorkspaceResource {
                operation_id: operation_id.to_owned(),
                creates_workspace: false,
                request_fingerprint: Some(request_fingerprint.to_owned()),
                semantic_fingerprint: None,
                completion_reservation: String::new(),
                owner_attempted,
                global_workspace_id: global_id.to_owned(),
                expected_global_revision,
                node_id: node_id.to_owned(),
                requested_owner_workspace_id: requested_owner_workspace_id.to_owned(),
                owner_workspace_id: effective_owner_workspace_id,
                owner_workspace_name: effective_owner_name,
                default_cwd: effective_cwd,
                resource_id: resource_id.to_owned(),
                kind,
            };
            let workspace = workspace.clone();
            reserve_new_pending_completion(state, &workspace, &mut pending, request_bytes)?;
            state.pending_resources.push(pending.clone());
            compact_prepared_state(state)?;
            Ok(PreparedWorkspaceResource::Pending {
                pending: Box::new(pending),
                newly_prepared: true,
            })
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_workspace_shell(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        semantic_fingerprint: &str,
        global_workspace_id: &str,
        name: &str,
        node_id: &str,
        requested_owner_workspace_id: &str,
        default_cwd: PathBuf,
        shell_id: &str,
    ) -> io::Result<PreparedWorkspaceShell> {
        validate_name(name)?;
        self.mutate(|state| {
            if let Some(completed) = state
                .completed_operations
                .iter()
                .find(|completed| completed.operation_id == operation_id)
            {
                if completed.request_fingerprint != request_fingerprint {
                    return Err(operation_conflict());
                }
                return Ok(PreparedWorkspaceShell::Completed(Box::new(
                    completed.clone(),
                )));
            }
            if let Some(index) = state
                .pending_resources
                .iter()
                .position(|pending| pending.operation_id == operation_id)
            {
                let pending = &state.pending_resources[index];
                if !pending.creates_workspace
                    || pending.global_workspace_id != global_workspace_id
                    || pending.node_id != node_id
                    || pending.requested_owner_workspace_id != requested_owner_workspace_id
                    || pending.owner_workspace_name != name
                    || pending.default_cwd.as_ref() != Some(&default_cwd)
                    || pending.resource_id != shell_id
                    || pending.kind != PendingResourceKind::Shell
                    || pending
                        .request_fingerprint
                        .as_deref()
                        .is_some_and(|fingerprint| fingerprint != request_fingerprint)
                {
                    return Err(operation_conflict());
                }
                if state.pending_resources[index].request_fingerprint.is_none() {
                    state.pending_resources[index].request_fingerprint =
                        Some(request_fingerprint.to_owned());
                    state.pending_resources[index].semantic_fingerprint =
                        Some(semantic_fingerprint.to_owned());
                }
                if state.pending_resources[index]
                    .completion_reservation
                    .is_empty()
                {
                    let workspace = state
                        .workspaces
                        .iter()
                        .find(|workspace| workspace.id == global_workspace_id)
                        .ok_or_else(|| invalid("prepared project Workspace is missing"))?;
                    let projected = projected_workspace_with_pending(state, workspace, None)?;
                    let reservation = completion_reservation_bytes(&projected, request_bytes)?;
                    state.pending_resources[index].completion_reservation = " ".repeat(reservation);
                    compact_prepared_state(state)?;
                }
                let pending = state.pending_resources[index].clone();
                let workspace = state
                    .workspaces
                    .iter()
                    .find(|workspace| workspace.id == pending.global_workspace_id)
                    .cloned()
                    .ok_or_else(|| invalid("prepared project Workspace is missing"))?;
                return Ok(PreparedWorkspaceShell::Pending {
                    workspace,
                    pending: Box::new(pending),
                });
            }
            if let Some(workspace) = state
                .workspaces
                .iter()
                .find(|workspace| workspace.name == name)
                .cloned()
            {
                if let Some(completed) = state.completed_operations.iter().rev().find(|completed| {
                    completed.creates_workspace
                        && completed.workspace.id == workspace.id
                        && completed.semantic_fingerprint.as_deref() == Some(semantic_fingerprint)
                }) {
                    let mut replay = completed.clone();
                    replay.operation_id = operation_id.to_owned();
                    replay.request_fingerprint = request_fingerprint.to_owned();
                    push_completed(state, replay.clone())?;
                    return Ok(PreparedWorkspaceShell::Completed(Box::new(replay)));
                }
                if let Some(existing_index) = state.pending_resources.iter().position(|pending| {
                    pending.creates_workspace
                        && pending.global_workspace_id == workspace.id
                        && (pending.semantic_fingerprint.as_deref() == Some(semantic_fingerprint)
                            || (pending.semantic_fingerprint.is_none()
                                && pending.node_id == node_id
                                && pending.owner_workspace_name == name
                                && pending.default_cwd.as_ref() == Some(&default_cwd)))
                        && pending.kind == PendingResourceKind::Shell
                }) {
                    if state.pending_resources.len() >= MAX_PENDING_RESOURCES {
                        return Err(invalid("coordinator pending resource limit reached"));
                    }
                    if state.pending_resources[existing_index]
                        .semantic_fingerprint
                        .is_none()
                    {
                        state.pending_resources[existing_index].semantic_fingerprint =
                            Some(semantic_fingerprint.to_owned());
                    }
                    let existing = state.pending_resources[existing_index].clone();
                    let mut pending = PendingWorkspaceResource {
                        operation_id: operation_id.to_owned(),
                        creates_workspace: true,
                        request_fingerprint: Some(request_fingerprint.to_owned()),
                        semantic_fingerprint: Some(semantic_fingerprint.to_owned()),
                        completion_reservation: String::new(),
                        owner_attempted: false,
                        global_workspace_id: existing.global_workspace_id,
                        expected_global_revision: existing.expected_global_revision,
                        node_id: existing.node_id,
                        requested_owner_workspace_id: requested_owner_workspace_id.to_owned(),
                        owner_workspace_id: existing.owner_workspace_id,
                        owner_workspace_name: existing.owner_workspace_name,
                        default_cwd: existing.default_cwd,
                        resource_id: existing.resource_id,
                        kind: PendingResourceKind::Shell,
                    };
                    reserve_new_pending_completion(state, &workspace, &mut pending, request_bytes)?;
                    state.pending_resources.push(pending.clone());
                    compact_prepared_state(state)?;
                    return Ok(PreparedWorkspaceShell::Pending {
                        workspace,
                        pending: Box::new(pending),
                    });
                }
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "global Workspace name already exists",
                ));
            }
            if state.workspaces.len() >= MAX_WORKSPACES
                || state.pending_resources.len() >= MAX_PENDING_RESOURCES
            {
                return Err(invalid("coordinator Workspace preparation limit reached"));
            }
            if state
                .workspaces
                .iter()
                .any(|workspace| workspace.id == global_workspace_id)
                || state
                    .pending_resources
                    .iter()
                    .any(|pending| pending.operation_id == operation_id)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "prepared project identity already exists",
                ));
            }
            let workspace = GlobalWorkspaceSnapshot {
                id: global_workspace_id.to_owned(),
                revision: 1,
                name: name.to_owned(),
                closing: false,
                placements: Vec::new(),
            };
            let mut pending = PendingWorkspaceResource {
                operation_id: operation_id.to_owned(),
                creates_workspace: true,
                request_fingerprint: Some(request_fingerprint.to_owned()),
                semantic_fingerprint: Some(semantic_fingerprint.to_owned()),
                completion_reservation: String::new(),
                owner_attempted: false,
                global_workspace_id: global_workspace_id.to_owned(),
                expected_global_revision: workspace.revision,
                node_id: node_id.to_owned(),
                requested_owner_workspace_id: requested_owner_workspace_id.to_owned(),
                owner_workspace_id: requested_owner_workspace_id.to_owned(),
                owner_workspace_name: name.to_owned(),
                default_cwd: Some(default_cwd),
                resource_id: shell_id.to_owned(),
                kind: PendingResourceKind::Shell,
            };
            reserve_new_pending_completion(state, &workspace, &mut pending, request_bytes)?;
            state.workspaces.push(workspace.clone());
            state.pending_resources.push(pending.clone());
            compact_prepared_state(state)?;
            Ok(PreparedWorkspaceShell::Pending {
                workspace,
                pending: Box::new(pending),
            })
        })
    }

    pub(crate) fn cancel_resource(&self, pending: &PendingWorkspaceResource) -> io::Result<()> {
        self.mutate(|state| {
            let Some(index) = state
                .pending_resources
                .iter()
                .position(|candidate| candidate.operation_id == pending.operation_id)
            else {
                if state.completed_operations.iter().any(|completed| {
                    completed.operation_id == pending.operation_id
                        && Some(completed.request_fingerprint.as_str())
                            == pending.request_fingerprint.as_deref()
                }) {
                    return Ok(());
                }
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "pending Workspace resource not found",
                ));
            };
            if state.pending_resources[index].request_fingerprint != pending.request_fingerprint {
                return Err(operation_conflict());
            }
            if state.pending_resources[index].creates_workspace
                && state.pending_resources[index].owner_attempted
            {
                return Ok(());
            }
            remove_pending_at(state, index);
            Ok(())
        })
    }

    pub(crate) fn cancel_pending_operation_if_never_attempted(
        &self,
        operation_id: &str,
        request_fingerprint: &str,
    ) -> io::Result<bool> {
        self.mutate(|state| {
            let Some(index) = state
                .pending_resources
                .iter()
                .position(|pending| pending.operation_id == operation_id)
            else {
                return Ok(false);
            };
            if state.pending_resources[index]
                .request_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| fingerprint != request_fingerprint)
            {
                return Err(operation_conflict());
            }
            if state.pending_resources[index].owner_attempted {
                return Ok(false);
            }
            remove_pending_at(state, index);
            Ok(true)
        })
    }

    pub(crate) fn complete_resource(
        &self,
        pending: &PendingWorkspaceResource,
        owner: &WorkspaceSnapshot,
        resource: &RoutedOperationResult,
    ) -> io::Result<CompletedWorkspaceOperation> {
        self.mutate(|state| {
            let Some(pending_index) = state
                .pending_resources
                .iter()
                .position(|candidate| candidate.operation_id == pending.operation_id)
            else {
                let completed = state
                    .completed_operations
                    .iter()
                    .find(|completed| {
                        completed.operation_id == pending.operation_id
                            && Some(completed.request_fingerprint.as_str())
                                == pending.request_fingerprint.as_deref()
                    })
                    .cloned()
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "pending Workspace resource not found",
                        )
                    })?;
                return Ok(completed);
            };
            if state.pending_resources[pending_index].request_fingerprint
                != pending.request_fingerprint
            {
                return Err(operation_conflict());
            }
            let pending = state.pending_resources[pending_index].clone();
            if owner.id != pending.owner_workspace_id
                || owner.name != pending.owner_workspace_name
                || owner.default_cwd != pending.default_cwd
            {
                return Err(invalid(
                    "owner Workspace metadata does not match the prepared operation",
                ));
            }
            ensure_owner_unlinked_except(
                state,
                &pending.global_workspace_id,
                &pending.node_id,
                &owner.id,
            )?;
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == pending.global_workspace_id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            if let Some(existing) = workspace
                .placements
                .iter_mut()
                .find(|placement| placement.node_id == pending.node_id)
            {
                if existing.workspace_id != owner.id {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "Workspace already has a different placement on this Node",
                    ));
                }
                existing.owner_workspace_name = Some(owner.name.clone());
                existing.owner_revision = owner.revision;
                existing.default_cwd.clone_from(&owner.default_cwd);
                existing.state = WorkspacePlacementState::Active;
            } else {
                workspace.revision = next(workspace.revision)?;
                workspace.placements.push(placement(
                    &pending.node_id,
                    owner,
                    WorkspacePlacementState::Active,
                ));
                workspace
                    .placements
                    .sort_by(|left, right| left.node_id.cmp(&right.node_id));
            }
            let result = workspace.clone();
            let completed = CompletedWorkspaceOperation {
                operation_id: pending.operation_id.clone(),
                request_fingerprint: pending.request_fingerprint.clone().ok_or_else(|| {
                    invalid("legacy prepared operation has no request fingerprint")
                })?,
                semantic_fingerprint: pending.semantic_fingerprint.clone(),
                creates_workspace: pending.creates_workspace,
                workspace: result,
                resource: resource.clone(),
            };
            let completed_bytes = serde_json::to_vec(&completed)
                .map_err(io::Error::other)?
                .len();
            if !pending.completion_reservation.is_empty()
                && completed_bytes > pending.completion_reservation.len()
            {
                return Err(invalid(
                    "completed Workspace operation exceeded its prepared capacity reservation",
                ));
            }
            state.pending_resources.remove(pending_index);
            push_completed(state, completed.clone())?;
            Ok(completed)
        })
    }

    pub(crate) fn mark_owner_attempted(
        &self,
        pending: &PendingWorkspaceResource,
    ) -> io::Result<PendingWorkspaceResource> {
        self.mutate(|state| {
            let index = state
                .pending_resources
                .iter()
                .position(|candidate| candidate.operation_id == pending.operation_id)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "pending Workspace resource not found",
                    )
                })?;
            if state.pending_resources[index].request_fingerprint != pending.request_fingerprint {
                return Err(operation_conflict());
            }
            state.pending_resources[index].owner_attempted = true;
            Ok(state.pending_resources[index].clone())
        })
    }

    pub(crate) fn reconcile_resource(
        &self,
        pending: &PendingWorkspaceResource,
        observed: Option<(&WorkspaceSnapshot, &RoutedOperationResult)>,
    ) -> io::Result<Option<CompletedWorkspaceOperation>> {
        match observed {
            Some((owner, resource)) => self.complete_resource(pending, owner, resource).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn rename(
        &self,
        id: &str,
        expected_revision: u64,
        name: String,
    ) -> io::Result<GlobalWorkspaceSnapshot> {
        validate_name(&name)?;
        self.mutate(|state| {
            ensure_no_pending_resource(state, id)?;
            if state
                .workspaces
                .iter()
                .any(|workspace| workspace.id != id && workspace.name == name)
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "global Workspace name already exists",
                ));
            }
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            require_revision(workspace.revision, expected_revision, "global Workspace")?;
            if workspace.closing {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "global Workspace close is in progress",
                ));
            }
            if workspace.name != name {
                workspace.name = name;
                workspace.revision = next(workspace.revision)?;
            }
            Ok(workspace.clone())
        })
    }

    pub(crate) fn begin_close(
        &self,
        id: &str,
        expected_revision: u64,
    ) -> io::Result<GlobalWorkspaceSnapshot> {
        self.mutate(|state| {
            ensure_no_pending_resource(state, id)?;
            let workspace = state
                .workspaces
                .iter_mut()
                .find(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            require_revision(workspace.revision, expected_revision, "global Workspace")?;
            if !workspace.closing {
                workspace.revision = next(workspace.revision)?;
                workspace.closing = true;
                for placement in &mut workspace.placements {
                    placement.state = WorkspacePlacementState::ClosePending;
                }
            }
            Ok(workspace.clone())
        })
    }

    pub(crate) fn confirm_closed(
        &self,
        id: &str,
        node_id: &str,
        owner_id: &str,
    ) -> io::Result<Option<GlobalWorkspaceSnapshot>> {
        self.mutate(|state| {
            let index = state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            let workspace = &mut state.workspaces[index];
            if !workspace.closing {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "global Workspace is not closing",
                ));
            }
            workspace.placements.retain(|placement| {
                !(placement.node_id == node_id && placement.workspace_id == owner_id)
            });
            if workspace.placements.is_empty() {
                state.workspaces.remove(index);
                return Ok(None);
            }
            workspace.revision = next(workspace.revision)?;
            Ok(Some(workspace.clone()))
        })
    }

    pub(crate) fn confirm_empty_closed(&self, id: &str) -> io::Result<()> {
        self.mutate(|state| {
            let index = state
                .workspaces
                .iter()
                .position(|workspace| workspace.id == id)
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "global Workspace not found")
                })?;
            let workspace = &state.workspaces[index];
            if !workspace.closing || !workspace.placements.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "global Workspace still has unresolved placements",
                ));
            }
            state.workspaces.remove(index);
            Ok(())
        })
    }

    fn mutate<T>(
        &self,
        operation: impl FnOnce(&mut PersistedGlobalWorkspaces) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut state = self.lock()?;
        let mut replacement = state.clone();
        let result = operation(&mut replacement)?;
        validate(&replacement)?;
        if self.persist {
            save(&self.path, &replacement)?;
        }
        *state = replacement;
        Ok(result)
    }

    fn lock(&self) -> io::Result<std::sync::MutexGuard<'_, PersistedGlobalWorkspaces>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("global Workspace store lock is poisoned"))
    }
}

impl GlobalWorkspaceTransaction<'_> {
    pub(crate) fn get(&self, id: &str) -> io::Result<GlobalWorkspaceSnapshot> {
        self.staged.get(id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_resource_for_attempt(
        &self,
        global_id: &str,
        operation_id: &str,
        request_fingerprint: &str,
        request_bytes: usize,
        expected_global_revision: u64,
        node_id: &str,
        requested_owner_workspace_id: &str,
        owner_workspace_name: &str,
        default_cwd: Option<PathBuf>,
        resource_id: &str,
        kind: PendingResourceKind,
    ) -> io::Result<PreparedWorkspaceResource> {
        self.staged.prepare_resource_for_attempt(
            global_id,
            operation_id,
            request_fingerprint,
            request_bytes,
            expected_global_revision,
            node_id,
            requested_owner_workspace_id,
            owner_workspace_name,
            default_cwd,
            resource_id,
            kind,
        )
    }

    pub(crate) fn complete_resource(
        &self,
        pending: &PendingWorkspaceResource,
        owner: &WorkspaceSnapshot,
        resource: &RoutedOperationResult,
    ) -> io::Result<CompletedWorkspaceOperation> {
        self.staged.complete_resource(pending, owner, resource)
    }

    pub(crate) fn commit(mut self) -> io::Result<()> {
        let replacement = self
            .staged
            .state
            .into_inner()
            .map_err(|_| io::Error::other("staged global Workspace lock is poisoned"))?;
        validate(&replacement)?;
        *self.target = replacement;
        Ok(())
    }
}

fn placement(
    node_id: &str,
    workspace: &WorkspaceSnapshot,
    state: WorkspacePlacementState,
) -> WorkspacePlacementSnapshot {
    WorkspacePlacementSnapshot {
        node_id: node_id.to_owned(),
        workspace_id: workspace.id.clone(),
        owner_workspace_name: Some(workspace.name.clone()),
        owner_revision: workspace.revision,
        default_cwd: workspace.default_cwd.clone(),
        state,
    }
}

fn ensure_owner_unlinked(
    state: &PersistedGlobalWorkspaces,
    node_id: &str,
    owner_id: &str,
) -> io::Result<()> {
    if state
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.placements)
        .any(|placement| placement.node_id == node_id && placement.workspace_id == owner_id)
    {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Node-local Workspace is already linked",
        ))
    } else {
        Ok(())
    }
}

fn ensure_owner_unlinked_except(
    state: &PersistedGlobalWorkspaces,
    global_id: &str,
    node_id: &str,
    owner_id: &str,
) -> io::Result<()> {
    if state
        .workspaces
        .iter()
        .filter(|workspace| workspace.id != global_id)
        .flat_map(|workspace| &workspace.placements)
        .any(|placement| placement.node_id == node_id && placement.workspace_id == owner_id)
    {
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Node-local Workspace is already linked",
        ))
    } else {
        Ok(())
    }
}

fn ensure_no_pending_resource(
    state: &PersistedGlobalWorkspaces,
    global_id: &str,
) -> io::Result<()> {
    if state
        .pending_resources
        .iter()
        .any(|pending| pending.global_workspace_id == global_id)
    {
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "global Workspace resource recovery is pending",
        ))
    } else {
        Ok(())
    }
}

fn remove_pending_at(state: &mut PersistedGlobalWorkspaces, index: usize) {
    let pending = state.pending_resources.remove(index);
    if pending.creates_workspace
        && let Some(workspace_index) = state.workspaces.iter().position(|workspace| {
            workspace.id == pending.global_workspace_id && workspace.placements.is_empty()
        })
        && !state
            .pending_resources
            .iter()
            .any(|candidate| candidate.global_workspace_id == pending.global_workspace_id)
    {
        state.workspaces.remove(workspace_index);
    }
}

fn push_completed(
    state: &mut PersistedGlobalWorkspaces,
    completed: CompletedWorkspaceOperation,
) -> io::Result<()> {
    if let Some(existing) = state
        .completed_operations
        .iter()
        .find(|existing| existing.operation_id == completed.operation_id)
    {
        return if existing == &completed {
            Ok(())
        } else {
            Err(operation_conflict())
        };
    }
    state.completed_operations.push(completed);
    while state.completed_operations.len() > MAX_COMPLETED_OPERATIONS
        || serde_json::to_vec_pretty(state).is_ok_and(|bytes| bytes.len() as u64 > MAX_STORE_BYTES)
    {
        if state.completed_operations.len() == 1 {
            return Err(invalid(
                "completed Workspace operation exceeds the coordinator store limit",
            ));
        }
        state.completed_operations.remove(0);
    }
    Ok(())
}

fn completion_reservation_bytes(
    workspace: &GlobalWorkspaceSnapshot,
    request_bytes: usize,
) -> io::Result<usize> {
    let workspace_bytes = serde_json::to_vec(workspace)
        .map_err(io::Error::other)?
        .len();
    let reservation = request_bytes
        .checked_mul(2)
        .and_then(|bytes| bytes.checked_add(workspace_bytes.saturating_mul(2)))
        .and_then(|bytes| bytes.checked_add(COMPLETION_RESERVATION_OVERHEAD))
        .ok_or_else(|| invalid("completed Workspace operation capacity overflow"))?;
    if reservation > MAX_COMPLETION_RESERVATION_BYTES {
        return Err(invalid(
            "Workspace resource request is too large for durable completion replay",
        ));
    }
    Ok(reservation)
}

fn projected_completed_workspace(
    workspace: &GlobalWorkspaceSnapshot,
    pending: &PendingWorkspaceResource,
) -> io::Result<GlobalWorkspaceSnapshot> {
    let mut projected = workspace.clone();
    if let Some(placement) = projected
        .placements
        .iter_mut()
        .find(|placement| placement.node_id == pending.node_id)
    {
        placement
            .workspace_id
            .clone_from(&pending.owner_workspace_id);
        placement.owner_workspace_name = Some(pending.owner_workspace_name.clone());
        placement.owner_revision = u64::MAX;
        placement.default_cwd.clone_from(&pending.default_cwd);
        placement.state = WorkspacePlacementState::Active;
    } else {
        projected.revision = next(projected.revision)?;
        projected.placements.push(WorkspacePlacementSnapshot {
            node_id: pending.node_id.clone(),
            workspace_id: pending.owner_workspace_id.clone(),
            owner_workspace_name: Some(pending.owner_workspace_name.clone()),
            owner_revision: u64::MAX,
            default_cwd: pending.default_cwd.clone(),
            state: WorkspacePlacementState::Active,
        });
        projected
            .placements
            .sort_by(|left, right| left.node_id.cmp(&right.node_id));
    }
    Ok(projected)
}

fn projected_workspace_with_pending(
    state: &PersistedGlobalWorkspaces,
    workspace: &GlobalWorkspaceSnapshot,
    additional: Option<&PendingWorkspaceResource>,
) -> io::Result<GlobalWorkspaceSnapshot> {
    let mut projected = workspace.clone();
    for pending in state
        .pending_resources
        .iter()
        .filter(|pending| pending.global_workspace_id == workspace.id)
        .chain(additional)
    {
        projected = projected_completed_workspace(&projected, pending)?;
    }
    Ok(projected)
}

fn reserve_new_pending_completion(
    state: &mut PersistedGlobalWorkspaces,
    workspace: &GlobalWorkspaceSnapshot,
    pending: &mut PendingWorkspaceResource,
    request_bytes: usize,
) -> io::Result<()> {
    let before = projected_workspace_with_pending(state, workspace, None)?;
    let after = projected_completed_workspace(&before, pending)?;
    let before_bytes = serde_json::to_vec(&before).map_err(io::Error::other)?.len();
    let after_bytes = serde_json::to_vec(&after).map_err(io::Error::other)?.len();
    let growth = after_bytes.saturating_sub(before_bytes).saturating_mul(2);
    if growth > 0 {
        for existing in state
            .pending_resources
            .iter_mut()
            .filter(|existing| existing.global_workspace_id == workspace.id)
        {
            if existing.completion_reservation.is_empty() {
                return Err(invalid(
                    "legacy pending Workspace operation must reserve completion capacity before another placement is prepared",
                ));
            }
            let expanded = existing
                .completion_reservation
                .len()
                .checked_add(growth)
                .ok_or_else(|| invalid("completed Workspace operation capacity overflow"))?;
            if expanded > MAX_COMPLETION_RESERVATION_BYTES {
                return Err(invalid(
                    "Workspace placement growth exceeds durable completion replay capacity",
                ));
            }
            existing
                .completion_reservation
                .push_str(&" ".repeat(growth));
        }
    }
    pending.completion_reservation =
        " ".repeat(completion_reservation_bytes(&after, request_bytes)?);
    Ok(())
}

fn compact_prepared_state(state: &mut PersistedGlobalWorkspaces) -> io::Result<()> {
    while serde_json::to_vec_pretty(state)
        .map_err(io::Error::other)?
        .len() as u64
        > MAX_STORE_BYTES
    {
        if state.completed_operations.is_empty() {
            return Err(invalid(
                "coordinator store has insufficient capacity to reserve the completed Workspace operation",
            ));
        }
        state.completed_operations.remove(0);
    }
    Ok(())
}

fn operation_conflict() -> io::Error {
    io::Error::new(
        io::ErrorKind::AlreadyExists,
        "prepared operation identity exists with a different request fingerprint",
    )
}

fn require_revision(actual: u64, expected: u64, label: &str) -> io::Result<()> {
    if actual == expected && expected > 0 {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{label} revision changed: expected {expected}, current {actual}"),
        ))
    }
}

fn next(revision: u64) -> io::Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| io::Error::other("global Workspace revision exhausted"))
}

fn validate_name(name: &str) -> io::Result<()> {
    if name.is_empty() || name.len() > 256 || name.chars().any(char::is_control) {
        Err(invalid("global Workspace name is invalid"))
    } else {
        Ok(())
    }
}

fn validate(state: &PersistedGlobalWorkspaces) -> io::Result<()> {
    if state.version != COORDINATOR_WORKSPACE_VERSION
        || state.workspaces.len() > MAX_WORKSPACES
        || state.pending_resources.len() > MAX_PENDING_RESOURCES
        || state.completed_operations.len() > MAX_COMPLETED_OPERATIONS
    {
        return Err(invalid("unsupported or oversized global Workspace store"));
    }
    let mut ids = HashSet::new();
    let mut names = HashSet::new();
    let mut owners = HashSet::new();
    for workspace in &state.workspaces {
        validate_name(&workspace.name)?;
        if Uuid::parse_str(&workspace.id).is_err()
            || workspace.revision == 0
            || workspace.placements.len() > MAX_PLACEMENTS
            || !ids.insert(&workspace.id)
            || !names.insert(&workspace.name)
        {
            return Err(invalid(
                "global Workspace store contains invalid or duplicate metadata",
            ));
        }
        for placement in &workspace.placements {
            if Uuid::parse_str(&placement.node_id).is_err()
                || placement.workspace_id.is_empty()
                || !owners.insert((&placement.node_id, &placement.workspace_id))
            {
                return Err(invalid(
                    "global Workspace store contains an invalid or duplicate placement",
                ));
            }
            if workspace.closing != (placement.state == WorkspacePlacementState::ClosePending) {
                return Err(invalid("global Workspace close progress is inconsistent"));
            }
        }
    }
    let mut pending = HashSet::new();
    for operation in &state.pending_resources {
        if Uuid::parse_str(&operation.global_workspace_id).is_err()
            || Uuid::parse_str(&operation.operation_id).is_err()
            || Uuid::parse_str(&operation.node_id).is_err()
            || Uuid::parse_str(&operation.requested_owner_workspace_id).is_err()
            || Uuid::parse_str(&operation.owner_workspace_id).is_err()
            || Uuid::parse_str(&operation.resource_id).is_err()
            || operation.expected_global_revision == 0
            || !ids.contains(&operation.global_workspace_id)
            || !pending.insert(&operation.operation_id)
            || operation
                .request_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| !valid_fingerprint(fingerprint))
            || operation
                .semantic_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| !valid_fingerprint(fingerprint))
            || operation.completion_reservation.len() > MAX_COMPLETION_RESERVATION_BYTES
            || operation
                .completion_reservation
                .bytes()
                .any(|byte| byte != b' ')
            || (!operation.completion_reservation.is_empty()
                && operation.request_fingerprint.is_none())
        {
            return Err(invalid("global Workspace pending resource is invalid"));
        }
        validate_name(&operation.owner_workspace_name)?;
    }
    let mut completed = HashSet::new();
    for operation in &state.completed_operations {
        let resource_workspace_id = match &operation.resource {
            RoutedOperationResult::Shell { shell } => &shell.workspace_id,
            RoutedOperationResult::Launcher { launcher } => &launcher.workspace_id,
            RoutedOperationResult::AgentSchedule { schedule } => &schedule.workspace_id,
            _ => {
                return Err(invalid(
                    "completed Workspace operation has an invalid result",
                ));
            }
        };
        if Uuid::parse_str(&operation.operation_id).is_err()
            || !valid_fingerprint(&operation.request_fingerprint)
            || operation
                .semantic_fingerprint
                .as_deref()
                .is_some_and(|fingerprint| !valid_fingerprint(fingerprint))
            || Uuid::parse_str(&operation.workspace.id).is_err()
            || operation.workspace.revision == 0
            || operation.workspace.placements.len() > MAX_PLACEMENTS
            || !operation
                .workspace
                .placements
                .iter()
                .any(|placement| placement.workspace_id == *resource_workspace_id)
            || !completed.insert(&operation.operation_id)
            || pending.contains(&operation.operation_id)
        {
            return Err(invalid("completed Workspace operation is invalid"));
        }
    }
    Ok(())
}

fn valid_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn load(path: &Path) -> io::Result<PersistedGlobalWorkspaces> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "global Workspace store is not an owner-only regular file",
        ));
    }
    if metadata.len() > MAX_STORE_BYTES {
        return Err(invalid("global Workspace store exceeds the size limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| invalid(format!("could not parse global Workspace store: {error}")))
}

fn save(path: &Path, state: &PersistedGlobalWorkspaces) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("global Workspace path has no parent"))?;
    secure_state_dir(parent)?;
    let bytes = serde_json::to_vec_pretty(state).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_STORE_BYTES {
        return Err(invalid("global Workspace store exceeds the size limit"));
    }
    let temporary = parent.join(format!(".global-workspaces-{}.tmp", Uuid::new_v4()));
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

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{
        SchedulerHealth, SchedulerState, ShellOwner, ShellSnapshot, ShellStatus,
    };
    use std::os::unix::fs::PermissionsExt;

    fn snapshot(id: &str, name: &str, revision: u64) -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            id: id.into(),
            revision,
            name: name.into(),
            default_cwd: Some("/owner/project".into()),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        }
    }

    fn daemon_snapshot(workspaces: Vec<WorkspaceSnapshot>) -> Snapshot {
        Snapshot {
            workspaces,
            focused_terminal: None,
            scheduler: Some(SchedulerHealth {
                state: SchedulerState::Active,
                max_concurrent: 4,
                active_executions: 0,
            }),
        }
    }

    fn fingerprint(value: u128) -> String {
        format!("{value:064x}")
    }

    fn shell_result(id: &str, workspace_id: &str) -> RoutedOperationResult {
        RoutedOperationResult::Shell {
            shell: ShellSnapshot {
                id: id.into(),
                revision: 1,
                workspace_id: workspace_id.into(),
                name: "shell".into(),
                cwd: "/owner/project".into(),
                command: Vec::new(),
                owner: ShellOwner::User,
                status: ShellStatus::Pending,
                run: None,
                recovered_agent_id: None,
                foreground_process: None,
            },
        }
    }

    #[test]
    fn legacy_local_workspaces_initialize_once_with_local_placements() {
        let root =
            std::env::temp_dir().join(format!("boomux-global-workspaces-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let node = Uuid::from_u128(1).to_string();
        let owner = Uuid::from_u128(2).to_string();
        assert!(
            store
                .initialize_local_once(&node, &daemon_snapshot(vec![snapshot(&owner, "local", 7)]))
                .unwrap()
        );
        assert!(
            !store
                .initialize_local_once(&node, &daemon_snapshot(vec![snapshot("later", "later", 1)]))
                .unwrap()
        );
        let workspace = &store.list().unwrap()[0];
        assert_eq!(workspace.id, owner);
        assert_eq!(workspace.placements[0].node_id, node);
        drop(store);
        assert_eq!(
            GlobalWorkspaceStore::load_at(path)
                .unwrap()
                .list()
                .unwrap()
                .len(),
            1
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn equal_names_never_merge_and_link_guards_both_authorities() {
        let root = std::env::temp_dir().join(format!("boomux-global-link-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        let local = Uuid::from_u128(1).to_string();
        let remote = Uuid::from_u128(2).to_string();
        let local_owner = snapshot(&Uuid::from_u128(3).to_string(), "same", 3);
        store
            .initialize_local_once(&local, &daemon_snapshot(vec![local_owner]))
            .unwrap();
        let remote_owner = snapshot(&Uuid::from_u128(4).to_string(), "same", 5);
        assert_eq!(
            store.adopt(&remote, &remote_owner, 5).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        let global = store.list().unwrap().remove(0);
        assert_eq!(
            store
                .link(&global.id, global.revision + 1, &remote, &remote_owner, 5)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let linked = store
            .link(&global.id, global.revision, &remote, &remote_owner, 5)
            .unwrap();
        assert_eq!(linked.placements.len(), 2);
        assert!(
            store
                .link(&global.id, linked.revision, &remote, &remote_owner, 5)
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn close_progress_survives_restart_until_every_owner_confirms() {
        let root = std::env::temp_dir().join(format!("boomux-global-close-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let local = Uuid::from_u128(1).to_string();
        let remote = Uuid::from_u128(2).to_string();
        let first = snapshot(&Uuid::from_u128(3).to_string(), "work", 1);
        let second = snapshot(&Uuid::from_u128(4).to_string(), "external", 1);
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(&local, &daemon_snapshot(vec![first.clone()]))
            .unwrap();
        let global = store.list().unwrap().remove(0);
        let linked = store
            .link(&global.id, global.revision, &remote, &second, 1)
            .unwrap();
        let _closing = store.begin_close(&global.id, linked.revision).unwrap();
        store.confirm_closed(&global.id, &local, &first.id).unwrap();
        drop(store);
        let restored = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let unresolved = restored.get(&global.id).unwrap();
        assert!(unresolved.closing);
        assert_eq!(unresolved.placements.len(), 1);
        assert!(
            restored
                .confirm_closed(&global.id, &remote, &second.id)
                .unwrap()
                .is_none()
        );
        assert!(restored.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_close_is_removed_only_after_close_begins() {
        let root =
            std::env::temp_dir().join(format!("boomux-global-empty-close-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        let global = store.create("empty".into()).unwrap();
        assert!(store.confirm_empty_closed(&global.id).is_err());
        let closing = store.begin_close(&global.id, global.revision).unwrap();
        assert!(closing.closing);
        store.confirm_empty_closed(&global.id).unwrap();
        assert!(store.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_one_is_migrated_explicitly_and_owner_name_is_learned_later() {
        let root = std::env::temp_dir().join(format!("boomux-global-v1-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        secure_state_dir(path.parent().unwrap()).unwrap();
        let global_id = Uuid::from_u128(1).to_string();
        let node_id = Uuid::from_u128(2).to_string();
        let owner_id = Uuid::from_u128(3).to_string();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "local_migration_complete": true,
                "workspaces": [{
                    "id": global_id,
                    "revision": 1,
                    "name": "global",
                    "closing": false,
                    "placements": [{
                        "node_id": node_id,
                        "workspace_id": owner_id,
                        "owner_revision": 4,
                        "default_cwd": "/owner/project",
                        "state": "active"
                    }]
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let workspace = store.get(&global_id).unwrap();
        assert_eq!(workspace.placements[0].owner_workspace_name, None);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 6);
        assert_eq!(persisted["pending_resources"], serde_json::json!([]));
        assert_eq!(persisted["completed_operations"], serde_json::json!([]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_two_pending_resource_migrates_to_exact_operation_identity() {
        let root = std::env::temp_dir().join(format!("boomux-global-v2-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        secure_state_dir(path.parent().unwrap()).unwrap();
        let global_id = Uuid::from_u128(1).to_string();
        let node_id = Uuid::from_u128(2).to_string();
        let owner_id = Uuid::from_u128(3).to_string();
        let resource_id = Uuid::from_u128(4).to_string();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "local_migration_complete": true,
                "workspaces": [{
                    "id": global_id,
                    "revision": 1,
                    "name": "pending",
                    "closing": false,
                    "placements": []
                }],
                "pending_resources": [{
                    "global_workspace_id": global_id,
                    "expected_global_revision": 1,
                    "node_id": node_id,
                    "owner_workspace_id": owner_id,
                    "owner_workspace_name": "pending",
                    "default_cwd": "/owner/project",
                    "resource_id": resource_id,
                    "kind": "shell"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let pending = store.pending_resources().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].operation_id, resource_id);
        assert!(!pending[0].creates_workspace);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 6);
        assert_eq!(
            persisted["pending_resources"][0]["operation_id"],
            resource_id
        );
        assert_eq!(pending[0].requested_owner_workspace_id, owner_id);
        assert!(pending[0].owner_attempted);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_three_pending_resource_migrates_requested_owner_and_empty_ledger() {
        let root = std::env::temp_dir().join(format!("boomux-global-v3-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        secure_state_dir(path.parent().unwrap()).unwrap();
        let global_id = Uuid::from_u128(1).to_string();
        let node_id = Uuid::from_u128(2).to_string();
        let owner_id = Uuid::from_u128(3).to_string();
        let resource_id = Uuid::from_u128(4).to_string();
        let operation_id = Uuid::from_u128(5).to_string();
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "version": 3,
                "local_migration_complete": true,
                "workspaces": [{
                    "id": global_id,
                    "revision": 1,
                    "name": "pending",
                    "closing": false,
                    "placements": []
                }],
                "pending_resources": [{
                    "operation_id": operation_id,
                    "creates_workspace": false,
                    "global_workspace_id": global_id,
                    "expected_global_revision": 1,
                    "node_id": node_id,
                    "owner_workspace_id": owner_id,
                    "owner_workspace_name": "pending",
                    "default_cwd": "/owner/project",
                    "resource_id": resource_id,
                    "kind": "shell"
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let pending = store.pending_resources().unwrap().remove(0);
        assert_eq!(pending.requested_owner_workspace_id, owner_id);
        assert_eq!(pending.request_fingerprint, None);
        assert!(pending.owner_attempted);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 6);
        assert_eq!(persisted["completed_operations"], serde_json::json!([]));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn prepared_resource_survives_restart_and_global_rename_keeps_owner_name() {
        let root = std::env::temp_dir().join(format!("boomux-global-pending-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let node = Uuid::from_u128(1).to_string();
        let owner = snapshot(&Uuid::from_u128(2).to_string(), "owner-local", 3);
        let resource = Uuid::from_u128(3).to_string();
        let operation = Uuid::from_u128(4).to_string();
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(&node, &daemon_snapshot(vec![owner.clone()]))
            .unwrap();
        let global = store.list().unwrap().remove(0);
        let renamed = store
            .rename(&global.id, global.revision, "coordinator-name".into())
            .unwrap();
        let prepared = store
            .prepare_resource(
                &renamed.id,
                &operation,
                &fingerprint(1),
                1024,
                renamed.revision,
                &node,
                &owner.id,
                &owner.name,
                owner.default_cwd.clone(),
                &resource,
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending,
            newly_prepared,
        } = prepared
        else {
            panic!("expected pending operation");
        };
        assert!(newly_prepared);
        assert_eq!(
            store
                .rename(&renamed.id, renamed.revision, "blocked".into())
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            store
                .begin_close(&renamed.id, renamed.revision)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        drop(store);
        let restored = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        let replay = restored
            .prepare_resource(
                &renamed.id,
                &operation,
                &fingerprint(1),
                1024,
                renamed.revision,
                &node,
                &owner.id,
                &owner.name,
                owner.default_cwd.clone(),
                &resource,
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: replay,
            newly_prepared,
        } = replay
        else {
            panic!("expected pending replay");
        };
        assert!(!newly_prepared);
        assert_eq!(replay, pending);
        let backup = path.with_extension("backup");
        fs::rename(&path, &backup).unwrap();
        fs::create_dir(&path).unwrap();
        assert!(
            restored
                .complete_resource(&replay, &owner, &shell_result(&resource, &owner.id))
                .is_err()
        );
        fs::remove_dir(&path).unwrap();
        fs::rename(&backup, &path).unwrap();
        drop(restored);
        let recovered = GlobalWorkspaceStore::load_at(path).unwrap();
        let replay = recovered
            .prepare_resource(
                &renamed.id,
                &operation,
                &fingerprint(1),
                1024,
                renamed.revision,
                &node,
                &owner.id,
                &owner.name,
                owner.default_cwd.clone(),
                &resource,
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: replay,
            newly_prepared,
        } = replay
        else {
            panic!("expected pending replay");
        };
        assert!(!newly_prepared);
        assert_eq!(replay, pending);
        let completed = recovered
            .complete_resource(&replay, &owner, &shell_result(&resource, &owner.id))
            .unwrap();
        assert_eq!(completed.workspace.name, "coordinator-name");
        assert_eq!(
            completed.workspace.placements[0]
                .owner_workspace_name
                .as_deref(),
            Some("owner-local")
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn preparation_for_dispatch_persists_attempted_boundary_once() {
        let root = std::env::temp_dir().join(format!("boomux-global-attempt-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let node = Uuid::from_u128(1).to_string();
        let owner = snapshot(&Uuid::from_u128(2).to_string(), "owner", 1);
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(&node, &daemon_snapshot(vec![owner.clone()]))
            .unwrap();
        let global = store.list().unwrap().remove(0);
        let prepared = store
            .prepare_resource_for_attempt(
                &global.id,
                &Uuid::from_u128(3).to_string(),
                &fingerprint(4),
                1024,
                global.revision,
                &node,
                &owner.id,
                &owner.name,
                owner.default_cwd.clone(),
                &Uuid::from_u128(5).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending { pending, .. } = prepared else {
            panic!("expected pending operation");
        };
        assert!(pending.owner_attempted);
        drop(store);
        assert!(
            GlobalWorkspaceStore::load_at(path)
                .unwrap()
                .pending_resources()
                .unwrap()[0]
                .owner_attempted
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compound_alias_retry_uses_its_original_guard_after_revision_advances() {
        let root = std::env::temp_dir().join(format!("boomux-global-alias-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let node = Uuid::from_u128(2).to_string();
        let operation_a = Uuid::from_u128(3).to_string();
        let global_a = Uuid::from_u128(4).to_string();
        let owner_a = Uuid::from_u128(5).to_string();
        let shell_a = Uuid::from_u128(6).to_string();
        let semantic = fingerprint(7);
        let prepared_a = store
            .prepare_workspace_shell(
                &operation_a,
                &fingerprint(8),
                1024,
                &semantic,
                &global_a,
                "project",
                &node,
                &owner_a,
                "/owner/project".into(),
                &shell_a,
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending {
            pending: pending_a, ..
        } = prepared_a
        else {
            panic!("expected first pending operation");
        };
        let operation_b = Uuid::from_u128(9).to_string();
        let prepared_b = store
            .prepare_workspace_shell(
                &operation_b,
                &fingerprint(10),
                1024,
                &semantic,
                &Uuid::from_u128(11).to_string(),
                "project",
                &node,
                &Uuid::from_u128(12).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(13).to_string(),
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending {
            workspace,
            pending: pending_b,
        } = prepared_b
        else {
            panic!("expected alias pending operation");
        };
        let attempted_b = store
            .prepare_resource_for_attempt(
                &pending_b.global_workspace_id,
                &pending_b.operation_id,
                pending_b.request_fingerprint.as_deref().unwrap(),
                1024,
                pending_b.expected_global_revision,
                &pending_b.node_id,
                &pending_b.requested_owner_workspace_id,
                &pending_b.owner_workspace_name,
                pending_b.default_cwd.clone(),
                &pending_b.resource_id,
                pending_b.kind,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: attempted_b,
            ..
        } = attempted_b
        else {
            panic!("expected attempted alias");
        };
        let owner = snapshot(&attempted_b.owner_workspace_id, "project", 1);
        let resource = shell_result(&attempted_b.resource_id, &owner.id);
        let completed_b = store
            .complete_resource(&attempted_b, &owner, &resource)
            .unwrap();
        assert!(completed_b.workspace.revision > workspace.revision);

        let retried_a = store
            .prepare_workspace_shell(
                &operation_a,
                &fingerprint(8),
                1024,
                &semantic,
                &global_a,
                "project",
                &node,
                &owner_a,
                "/owner/project".into(),
                &shell_a,
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending {
            workspace,
            pending: retried_a,
        } = retried_a
        else {
            panic!("expected exact pending retry");
        };
        assert_eq!(
            retried_a.expected_global_revision,
            pending_a.expected_global_revision
        );
        assert!(workspace.revision > retried_a.expected_global_revision);
        let attempted_a = store
            .prepare_resource_for_attempt(
                &retried_a.global_workspace_id,
                &retried_a.operation_id,
                retried_a.request_fingerprint.as_deref().unwrap(),
                1024,
                retried_a.expected_global_revision,
                &retried_a.node_id,
                &retried_a.requested_owner_workspace_id,
                &retried_a.owner_workspace_name,
                retried_a.default_cwd.clone(),
                &retried_a.resource_id,
                retried_a.kind,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: attempted_a,
            ..
        } = attempted_a
        else {
            panic!("expected attempted exact retry");
        };
        assert!(attempted_a.owner_attempted);
        assert_eq!(
            store
                .complete_resource(&attempted_a, &owner, &resource)
                .unwrap()
                .resource,
            resource
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn malformed_future_and_insecure_stores_fail_closed_without_replacement() {
        for (label, bytes, mode) in [
            ("malformed", b"not-json".to_vec(), 0o600),
            (
                "future",
                br#"{"version":99,"local_migration_complete":true,"workspaces":[],"pending_resources":[]}"#.to_vec(),
                0o600,
            ),
            (
                "insecure",
                br#"{"version":2,"local_migration_complete":true,"workspaces":[],"pending_resources":[]}"#.to_vec(),
                0o644,
            ),
        ] {
            let root = std::env::temp_dir().join(format!("boomux-global-{label}-{}", Uuid::new_v4()));
            let path = root.join("boomux/global_workspaces.json");
            secure_state_dir(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert!(GlobalWorkspaceStore::load_at(path.clone()).is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn concurrent_same_kind_operations_keep_exact_records_and_cancel_in_isolation() {
        use std::sync::{Arc, Barrier};

        let root =
            std::env::temp_dir().join(format!("boomux-global-concurrent-{}", Uuid::new_v4()));
        let store = Arc::new(
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap(),
        );
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("concurrent".into()).unwrap();
        let node = Uuid::from_u128(2).to_string();
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for offset in 0..2_u128 {
            let store = Arc::clone(&store);
            let barrier = Arc::clone(&barrier);
            let global = global.clone();
            let node = node.clone();
            handles.push(std::thread::spawn(move || {
                let operation = Uuid::from_u128(10 + offset).to_string();
                let resource = Uuid::from_u128(20 + offset).to_string();
                let requested_owner = Uuid::from_u128(30 + offset).to_string();
                barrier.wait();
                let prepared = store
                    .prepare_resource(
                        &global.id,
                        &operation,
                        &fingerprint(10 + offset),
                        1024,
                        global.revision,
                        &node,
                        &requested_owner,
                        "concurrent",
                        Some("/owner/project".into()),
                        &resource,
                        PendingResourceKind::Shell,
                    )
                    .unwrap();
                let PreparedWorkspaceResource::Pending { pending, .. } = prepared else {
                    panic!("expected pending operation");
                };
                pending
            }));
        }
        barrier.wait();
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();
        assert_ne!(first.operation_id, second.operation_id);
        assert_ne!(first.resource_id, second.resource_id);
        assert_ne!(
            first.requested_owner_workspace_id,
            second.requested_owner_workspace_id
        );
        assert_eq!(first.owner_workspace_id, second.owner_workspace_id);
        store.cancel_resource(&first).unwrap();
        assert_eq!(store.pending_resources().unwrap(), vec![(*second).clone()]);
        assert!(
            store
                .prepare_resource(
                    &global.id,
                    &second.operation_id,
                    second.request_fingerprint.as_deref().unwrap(),
                    1024,
                    global.revision,
                    &node,
                    &second.requested_owner_workspace_id,
                    "concurrent",
                    Some("/owner/project".into()),
                    &Uuid::new_v4().to_string(),
                    PendingResourceKind::Shell,
                )
                .is_err()
        );
        let completed_owner = snapshot(&second.owner_workspace_id, "concurrent", 2);
        store
            .complete_resource(
                &second,
                &completed_owner,
                &shell_result(&second.resource_id, &completed_owner.id),
            )
            .unwrap();
        assert!(store.pending_resources().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn reconcile_absence_retains_then_exact_observation_completes_and_replays() {
        let root = std::env::temp_dir().join(format!("boomux-global-reconcile-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("reconcile".into()).unwrap();
        let operation_id = Uuid::from_u128(2).to_string();
        let node_id = Uuid::from_u128(3).to_string();
        let owner_id = Uuid::from_u128(4).to_string();
        let resource_id = Uuid::from_u128(5).to_string();
        let request_fingerprint = fingerprint(6);
        let prepared = store
            .prepare_resource(
                &global.id,
                &operation_id,
                &request_fingerprint,
                1024,
                global.revision,
                &node_id,
                &owner_id,
                "reconcile",
                Some("/owner/project".into()),
                &resource_id,
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending { pending, .. } = prepared else {
            panic!("expected pending operation");
        };

        assert_eq!(store.reconcile_resource(&pending, None).unwrap(), None);
        assert_eq!(store.pending_resources().unwrap(), vec![(*pending).clone()]);

        let owner = snapshot(&pending.owner_workspace_id, "reconcile", 2);
        let resource = shell_result(&pending.resource_id, &pending.owner_workspace_id);
        let completed = store
            .reconcile_resource(&pending, Some((&owner, &resource)))
            .unwrap()
            .unwrap();
        assert!(store.pending_resources().unwrap().is_empty());
        assert_eq!(completed.workspace.revision, global.revision + 1);

        drop(store);
        let restored = GlobalWorkspaceStore::load_at(path).unwrap();
        let replay = restored
            .prepare_resource(
                &global.id,
                &operation_id,
                &request_fingerprint,
                1024,
                global.revision,
                &node_id,
                &owner_id,
                "reconcile",
                Some("/owner/project".into()),
                &resource_id,
                PendingResourceKind::Shell,
            )
            .unwrap();
        assert_eq!(
            replay,
            PreparedWorkspaceResource::Completed(Box::new(completed))
        );
        assert!(
            restored
                .completed_operation(&operation_id, &fingerprint(7))
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn concurrent_exact_completion_and_late_cancel_share_one_terminal_success() {
        use std::sync::{Arc, Barrier};

        let root = std::env::temp_dir().join(format!("boomux-global-cas-{}", Uuid::new_v4()));
        let store = Arc::new(
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap(),
        );
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("cas".into()).unwrap();
        let prepared = store
            .prepare_resource(
                &global.id,
                &Uuid::from_u128(2).to_string(),
                &fingerprint(3),
                1024,
                global.revision,
                &Uuid::from_u128(4).to_string(),
                &Uuid::from_u128(5).to_string(),
                "cas",
                Some("/owner/project".into()),
                &Uuid::from_u128(6).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending { pending, .. } = prepared else {
            panic!("expected pending operation");
        };
        let owner = snapshot(&pending.owner_workspace_id, "cas", 2);
        let resource = shell_result(&pending.resource_id, &pending.owner_workspace_id);
        let barrier = Arc::new(Barrier::new(3));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = Arc::clone(&store);
            let pending = pending.clone();
            let owner = owner.clone();
            let resource = resource.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                store
                    .complete_resource(&pending, &owner, &resource)
                    .unwrap()
            }));
        }
        barrier.wait();
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();
        assert_eq!(first, second);
        store.cancel_resource(&pending).unwrap();
        assert_eq!(
            store
                .completed_operation(
                    &pending.operation_id,
                    pending.request_fingerprint.as_deref().unwrap()
                )
                .unwrap(),
            Some(first)
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_workspace_preparation_resumes_by_name_and_cancels_without_empty_metadata() {
        let root = std::env::temp_dir().join(format!("boomux-global-project-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let node = Uuid::from_u128(2).to_string();
        let prepared = store
            .prepare_workspace_shell(
                &Uuid::from_u128(3).to_string(),
                &fingerprint(1),
                1024,
                &fingerprint(2),
                &Uuid::from_u128(4).to_string(),
                "project",
                &node,
                &Uuid::from_u128(5).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(6).to_string(),
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending { workspace, pending } = prepared else {
            panic!("expected prepared project");
        };
        drop(store);
        let restored = GlobalWorkspaceStore::load_at(path).unwrap();
        let resumed = restored
            .prepare_workspace_shell(
                &Uuid::new_v4().to_string(),
                &fingerprint(3),
                1024,
                &fingerprint(2),
                &Uuid::new_v4().to_string(),
                "project",
                &node,
                &Uuid::new_v4().to_string(),
                "/owner/project".into(),
                &Uuid::new_v4().to_string(),
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending {
            workspace: resumed_workspace,
            pending: resumed,
        } = resumed
        else {
            panic!("expected resumed project");
        };
        assert_eq!(resumed_workspace.id, workspace.id);
        assert_ne!(resumed.operation_id, pending.operation_id);
        assert_eq!(resumed.owner_workspace_id, pending.owner_workspace_id);
        assert_eq!(resumed.resource_id, pending.resource_id);
        restored.cancel_resource(&resumed).unwrap();
        restored.cancel_resource(&pending).unwrap();
        assert!(restored.list().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_project_replays_by_exact_operation_and_matching_name_semantics() {
        let root =
            std::env::temp_dir().join(format!("boomux-global-project-replay-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let operation_id = Uuid::from_u128(2).to_string();
        let global_id = Uuid::from_u128(3).to_string();
        let node_id = Uuid::from_u128(4).to_string();
        let owner_id = Uuid::from_u128(5).to_string();
        let shell_id = Uuid::from_u128(6).to_string();
        let exact = fingerprint(7);
        let semantic = fingerprint(8);
        let prepared = store
            .prepare_workspace_shell(
                &operation_id,
                &exact,
                1024,
                &semantic,
                &global_id,
                "project",
                &node_id,
                &owner_id,
                "/owner/project".into(),
                &shell_id,
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending { pending, .. } = prepared else {
            panic!("expected pending project");
        };
        let owner = snapshot(&pending.owner_workspace_id, "project", 2);
        let completed = store
            .complete_resource(
                &pending,
                &owner,
                &shell_result(&pending.resource_id, &pending.owner_workspace_id),
            )
            .unwrap();
        assert_eq!(
            store
                .prepare_workspace_shell(
                    &operation_id,
                    &exact,
                    1024,
                    &semantic,
                    &global_id,
                    "project",
                    &node_id,
                    &owner_id,
                    "/owner/project".into(),
                    &shell_id,
                )
                .unwrap(),
            PreparedWorkspaceShell::Completed(Box::new(completed.clone()))
        );
        let alias_operation = Uuid::from_u128(9).to_string();
        let alias = store
            .prepare_workspace_shell(
                &alias_operation,
                &fingerprint(10),
                1024,
                &semantic,
                &Uuid::from_u128(11).to_string(),
                "project",
                &node_id,
                &Uuid::from_u128(12).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(13).to_string(),
            )
            .unwrap();
        let PreparedWorkspaceShell::Completed(alias) = alias else {
            panic!("expected completed project alias");
        };
        assert_eq!(alias.operation_id, alias_operation);
        assert_eq!(alias.workspace, completed.workspace);
        assert_eq!(alias.resource, completed.resource);
        assert!(
            store
                .prepare_workspace_shell(
                    &Uuid::from_u128(14).to_string(),
                    &fingerprint(15),
                    1024,
                    &fingerprint(16),
                    &Uuid::from_u128(17).to_string(),
                    "project",
                    &node_id,
                    &Uuid::from_u128(18).to_string(),
                    "/owner/project".into(),
                    &Uuid::from_u128(19).to_string(),
                )
                .is_err()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn definitive_project_preflight_cancels_exact_empty_metadata_for_new_semantics() {
        let root = std::env::temp_dir().join(format!("boomux-global-preflight-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let operation_id = Uuid::from_u128(2).to_string();
        let exact = fingerprint(3);
        store
            .prepare_workspace_shell(
                &operation_id,
                &exact,
                1024,
                &fingerprint(4),
                &Uuid::from_u128(5).to_string(),
                "preflight-project",
                &Uuid::from_u128(6).to_string(),
                &Uuid::from_u128(7).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(8).to_string(),
            )
            .unwrap();
        assert!(
            store
                .cancel_pending_operation_if_never_attempted(&operation_id, &exact)
                .unwrap()
        );
        assert!(store.list().unwrap().is_empty());
        let replacement = store
            .prepare_workspace_shell(
                &Uuid::from_u128(9).to_string(),
                &fingerprint(10),
                1024,
                &fingerprint(11),
                &Uuid::from_u128(12).to_string(),
                "preflight-project",
                &Uuid::from_u128(6).to_string(),
                &Uuid::from_u128(13).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(14).to_string(),
            )
            .unwrap();
        assert!(matches!(
            replacement,
            PreparedWorkspaceShell::Pending { .. }
        ));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn attempted_project_retains_pending_name_across_later_definitive_failures() {
        let root = std::env::temp_dir().join(format!("boomux-global-attempted-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let operation_id = Uuid::from_u128(2).to_string();
        let exact = fingerprint(3);
        let prepared = store
            .prepare_workspace_shell(
                &operation_id,
                &exact,
                1024,
                &fingerprint(4),
                &Uuid::from_u128(5).to_string(),
                "attempted-project",
                &Uuid::from_u128(6).to_string(),
                &Uuid::from_u128(7).to_string(),
                "/owner/project".into(),
                &Uuid::from_u128(8).to_string(),
            )
            .unwrap();
        let PreparedWorkspaceShell::Pending { pending, .. } = prepared else {
            panic!("expected pending project");
        };
        let attempted = store
            .prepare_resource_for_attempt(
                &pending.global_workspace_id,
                &pending.operation_id,
                pending.request_fingerprint.as_deref().unwrap(),
                1024,
                pending.expected_global_revision,
                &pending.node_id,
                &pending.requested_owner_workspace_id,
                &pending.owner_workspace_name,
                pending.default_cwd.clone(),
                &pending.resource_id,
                pending.kind,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: attempted, ..
        } = attempted
        else {
            panic!("expected pending attempted project");
        };
        let attempted = *attempted;
        assert!(attempted.owner_attempted);

        // A later capability, admission, identity, or not-found failure cannot
        // prove what happened after this durable dispatch boundary.
        store.cancel_resource(&attempted).unwrap();
        assert!(
            !store
                .cancel_pending_operation_if_never_attempted(&operation_id, &exact)
                .unwrap()
        );
        assert_eq!(store.pending_resources().unwrap(), vec![attempted]);
        assert_eq!(store.list().unwrap()[0].name, "attempted-project");
        assert!(
            store
                .prepare_workspace_shell(
                    &Uuid::from_u128(9).to_string(),
                    &fingerprint(10),
                    1024,
                    &fingerprint(11),
                    &Uuid::from_u128(12).to_string(),
                    "attempted-project",
                    &Uuid::from_u128(13).to_string(),
                    &Uuid::from_u128(14).to_string(),
                    "/different/project".into(),
                    &Uuid::from_u128(15).to_string(),
                )
                .is_err()
        );
        assert_eq!(store.list().unwrap()[0].name, "attempted-project");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_four_pending_operation_reserves_completion_capacity_on_retry() {
        let root = std::env::temp_dir().join(format!("boomux-global-v4-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("v4-pending".into()).unwrap();
        let operation_id = Uuid::from_u128(2).to_string();
        let request_fingerprint = fingerprint(3);
        store
            .prepare_resource(
                &global.id,
                &operation_id,
                &request_fingerprint,
                1024,
                global.revision,
                &Uuid::from_u128(4).to_string(),
                &Uuid::from_u128(5).to_string(),
                "v4-pending",
                Some("/owner/project".into()),
                &Uuid::from_u128(6).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        drop(store);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        legacy["version"] = 4.into();
        legacy["pending_resources"][0]
            .as_object_mut()
            .unwrap()
            .remove("completion_reservation");
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let migrated = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        assert!(
            migrated.pending_resources().unwrap()[0]
                .completion_reservation
                .is_empty()
        );
        assert!(migrated.pending_resources().unwrap()[0].owner_attempted);
        migrated
            .prepare_resource(
                &global.id,
                &operation_id,
                &request_fingerprint,
                1024,
                global.revision,
                &Uuid::from_u128(4).to_string(),
                &Uuid::from_u128(5).to_string(),
                "v4-pending",
                Some("/owner/project".into()),
                &Uuid::from_u128(6).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 6);
        assert!(
            persisted["pending_resources"][0]["completion_reservation"]
                .as_str()
                .is_some_and(|reservation| !reservation.is_empty())
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_five_pending_operation_migrates_as_conservatively_attempted() {
        let root = std::env::temp_dir().join(format!("boomux-global-v5-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("v5-pending".into()).unwrap();
        store
            .prepare_resource(
                &global.id,
                &Uuid::from_u128(2).to_string(),
                &fingerprint(3),
                1024,
                global.revision,
                &Uuid::from_u128(4).to_string(),
                &Uuid::from_u128(5).to_string(),
                "v5-pending",
                Some("/owner/project".into()),
                &Uuid::from_u128(6).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        drop(store);
        let mut legacy: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        legacy["version"] = 5.into();
        legacy["pending_resources"][0]
            .as_object_mut()
            .unwrap()
            .remove("owner_attempted");
        fs::write(&path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();

        let migrated = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        assert!(migrated.pending_resources().unwrap()[0].owner_attempted);
        let persisted: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["version"], 6);
        assert_eq!(persisted["pending_resources"][0]["owner_attempted"], true);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn near_limit_store_reserves_completion_before_owner_mutation() {
        let root = std::env::temp_dir().join(format!("boomux-global-capacity-{}", Uuid::new_v4()));
        let path = root.join("boomux/global_workspaces.json");
        let store = GlobalWorkspaceStore::load_at(path.clone()).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("capacity".into()).unwrap();
        let owner = snapshot(&Uuid::from_u128(2).to_string(), "historical", 1);
        let historical = GlobalWorkspaceSnapshot {
            id: Uuid::from_u128(3).to_string(),
            revision: 2,
            name: "historical".into(),
            closing: false,
            placements: vec![placement(
                &Uuid::from_u128(4).to_string(),
                &owner,
                WorkspacePlacementState::Active,
            )],
        };
        store
            .mutate(|state| {
                for offset in 0..100_u128 {
                    let mut resource =
                        shell_result(&Uuid::from_u128(1_000 + offset).to_string(), &owner.id);
                    let RoutedOperationResult::Shell { shell } = &mut resource else {
                        unreachable!();
                    };
                    shell.command = vec!["x".repeat(20_000)];
                    push_completed(
                        state,
                        CompletedWorkspaceOperation {
                            operation_id: Uuid::from_u128(100 + offset).to_string(),
                            request_fingerprint: fingerprint(100 + offset),
                            semantic_fingerprint: None,
                            creates_workspace: false,
                            workspace: historical.clone(),
                            resource,
                        },
                    )?;
                }
                Ok(())
            })
            .unwrap();
        let before_bytes = fs::metadata(&path).unwrap().len();
        let before_outcomes = store.lock().unwrap().completed_operations.len();
        assert!(before_bytes > 850 * 1024);

        let operation_id = Uuid::from_u128(10).to_string();
        let node_id = Uuid::from_u128(11).to_string();
        let owner_id = Uuid::from_u128(12).to_string();
        let resource_id = Uuid::from_u128(13).to_string();
        let prepared = store
            .prepare_resource(
                &global.id,
                &operation_id,
                &fingerprint(14),
                50_000,
                global.revision,
                &node_id,
                &owner_id,
                "capacity",
                Some("/owner/project".into()),
                &resource_id,
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending { pending, .. } = prepared else {
            panic!("expected capacity-reserved pending operation");
        };
        assert!(!pending.completion_reservation.is_empty());
        assert!(store.lock().unwrap().completed_operations.len() < before_outcomes);
        let completed_owner = snapshot(&pending.owner_workspace_id, "capacity", 2);
        let mut resource = shell_result(&pending.resource_id, &pending.owner_workspace_id);
        let RoutedOperationResult::Shell { shell } = &mut resource else {
            unreachable!();
        };
        shell.command = vec!["y".repeat(50_000)];
        store
            .complete_resource(&pending, &completed_owner, &resource)
            .unwrap();
        assert!(fs::metadata(&path).unwrap().len() <= MAX_STORE_BYTES);

        let error = store
            .prepare_resource(
                &global.id,
                &Uuid::from_u128(20).to_string(),
                &fingerprint(21),
                MAX_COMPLETION_RESERVATION_BYTES,
                global.revision + 1,
                &node_id,
                &Uuid::from_u128(22).to_string(),
                "capacity",
                Some("/owner/project".into()),
                &Uuid::from_u128(23).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap_err();
        assert!(error.to_string().contains("too large"));
        assert!(store.pending_resources().unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn distinct_pending_placements_expand_each_others_completion_reservations() {
        let root = std::env::temp_dir().join(format!("boomux-global-growth-{}", Uuid::new_v4()));
        let store =
            GlobalWorkspaceStore::load_at(root.join("boomux/global_workspaces.json")).unwrap();
        store
            .initialize_local_once(
                &Uuid::from_u128(1).to_string(),
                &daemon_snapshot(Vec::new()),
            )
            .unwrap();
        let global = store.create("growth".into()).unwrap();
        let first = store
            .prepare_resource(
                &global.id,
                &Uuid::from_u128(2).to_string(),
                &fingerprint(3),
                1024,
                global.revision,
                &Uuid::from_u128(4).to_string(),
                &Uuid::from_u128(5).to_string(),
                "growth",
                Some("/owner/one".into()),
                &Uuid::from_u128(6).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending { pending: first, .. } = first else {
            panic!("expected first pending placement");
        };
        let first_reservation = first.completion_reservation.len();
        let second = store
            .prepare_resource(
                &global.id,
                &Uuid::from_u128(7).to_string(),
                &fingerprint(8),
                1024,
                global.revision,
                &Uuid::from_u128(9).to_string(),
                &Uuid::from_u128(10).to_string(),
                "growth",
                Some("/owner/two".into()),
                &Uuid::from_u128(11).to_string(),
                PendingResourceKind::Shell,
            )
            .unwrap();
        let PreparedWorkspaceResource::Pending {
            pending: second, ..
        } = second
        else {
            panic!("expected second pending placement");
        };
        let pending = store.pending_resources().unwrap();
        let expanded_first = pending
            .iter()
            .find(|pending| pending.operation_id == first.operation_id)
            .unwrap();
        assert!(expanded_first.completion_reservation.len() > first_reservation);

        let second_owner = WorkspaceSnapshot {
            id: second.owner_workspace_id.clone(),
            revision: 1,
            name: "growth".into(),
            default_cwd: Some("/owner/two".into()),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        };
        store
            .complete_resource(
                &second,
                &second_owner,
                &shell_result(&second.resource_id, &second.owner_workspace_id),
            )
            .unwrap();
        let first = store
            .pending_resources()
            .unwrap()
            .into_iter()
            .find(|pending| pending.operation_id == first.operation_id)
            .unwrap();
        let first_owner = WorkspaceSnapshot {
            id: first.owner_workspace_id.clone(),
            revision: 1,
            name: "growth".into(),
            default_cwd: Some("/owner/one".into()),
            shells: Vec::new(),
            launchers: Vec::new(),
            agents: Vec::new(),
            schedules: Vec::new(),
        };
        let completed = store
            .complete_resource(
                &first,
                &first_owner,
                &shell_result(&first.resource_id, &first.owner_workspace_id),
            )
            .unwrap();
        assert_eq!(completed.workspace.placements.len(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_operation_ledger_keeps_only_the_newest_bounded_outcomes() {
        let node_id = Uuid::from_u128(1).to_string();
        let owner = snapshot(&Uuid::from_u128(2).to_string(), "bounded", 1);
        let workspace = GlobalWorkspaceSnapshot {
            id: Uuid::from_u128(3).to_string(),
            revision: 2,
            name: "bounded".into(),
            closing: false,
            placements: vec![placement(&node_id, &owner, WorkspacePlacementState::Active)],
        };
        let mut state = PersistedGlobalWorkspaces {
            version: COORDINATOR_WORKSPACE_VERSION,
            local_migration_complete: true,
            workspaces: vec![workspace.clone()],
            pending_resources: Vec::new(),
            completed_operations: Vec::new(),
        };
        for offset in 0..=MAX_COMPLETED_OPERATIONS {
            push_completed(
                &mut state,
                CompletedWorkspaceOperation {
                    operation_id: Uuid::from_u128(100 + offset as u128).to_string(),
                    request_fingerprint: fingerprint(100 + offset as u128),
                    semantic_fingerprint: None,
                    creates_workspace: false,
                    workspace: workspace.clone(),
                    resource: shell_result(
                        &Uuid::from_u128(1_000 + offset as u128).to_string(),
                        &owner.id,
                    ),
                },
            )
            .unwrap();
        }
        assert_eq!(state.completed_operations.len(), MAX_COMPLETED_OPERATIONS);
        assert_eq!(
            state.completed_operations[0].operation_id,
            Uuid::from_u128(101).to_string()
        );
        assert!(serde_json::to_vec(&state).unwrap().len() as u64 <= MAX_STORE_BYTES);
        validate(&state).unwrap();
    }
}
