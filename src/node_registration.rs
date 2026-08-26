use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
#[cfg(test)]
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::protocol::NodeRegistrationSnapshot;
use crate::ssh_bootstrap::SshTarget;
use crate::state_store::{effective_uid, secure_state_dir, state_directory_from_environment};

const REGISTRATION_VERSION: u32 = 1;
const MAX_REGISTRATION_BYTES: u64 = 128 * 1024;
const MAX_REGISTRATIONS: usize = 128;
const MAX_ALIAS_BYTES: usize = 128;

pub(crate) struct NodeRegistrationManager {
    path: PathBuf,
    state: Mutex<ManagerState>,
    changed: Condvar,
}

enum ManagerState {
    Available(RegistrationState),
    Unavailable(String),
}

#[derive(Clone)]
struct RegistrationState {
    revision: u64,
    tombstone_epoch: u64,
    registrations: Vec<Registration>,
}

#[derive(Clone)]
struct Registration {
    snapshot: NodeRegistrationSnapshot,
    admission_open: bool,
    admission_epoch: u64,
    admitted: usize,
    maintenance: Option<MaintenanceLease>,
}

#[derive(Clone)]
struct MaintenanceLease {
    token: String,
    deadline: Instant,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PersistedRegistrations {
    version: u32,
    revision: u64,
    tombstone_epoch: u64,
    registrations: Vec<NodeRegistrationSnapshot>,
}

impl NodeRegistrationManager {
    pub(crate) fn load_from_environment() -> Self {
        match state_directory_from_environment() {
            Ok(directory) => Self::load_at(directory.join("node_registrations.json")),
            Err(error) => Self::unavailable(PathBuf::new(), error),
        }
    }

    fn load_at(path: PathBuf) -> Self {
        match load(&path) {
            Ok(state) => Self {
                path,
                state: Mutex::new(ManagerState::Available(state)),
                changed: Condvar::new(),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self {
                path,
                state: Mutex::new(ManagerState::Available(RegistrationState {
                    revision: 0,
                    tombstone_epoch: 0,
                    registrations: Vec::new(),
                })),
                changed: Condvar::new(),
            },
            Err(error) => Self::unavailable(path, error),
        }
    }

    fn unavailable(path: PathBuf, error: io::Error) -> Self {
        Self {
            path,
            state: Mutex::new(ManagerState::Unavailable(error.to_string())),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn unavailable_reason(&self) -> io::Result<Option<String>> {
        let state = self.lock_state()?;
        Ok(match &*state {
            ManagerState::Available(_) => None,
            ManagerState::Unavailable(reason) => Some(reason.clone()),
        })
    }

    pub(crate) fn list(&self) -> io::Result<Vec<NodeRegistrationSnapshot>> {
        let state = self.lock_state()?;
        let state = available(&state)?;
        let mut registrations = state
            .registrations
            .iter()
            .map(|registration| registration.snapshot.clone())
            .collect::<Vec<_>>();
        registrations.sort_by(|left, right| {
            left.alias
                .cmp(&right.alias)
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
        Ok(registrations)
    }

    pub(crate) fn inspect(&self, selector: &str) -> io::Result<NodeRegistrationSnapshot> {
        let state = self.lock_state()?;
        let state = available(&state)?;
        Ok(find(state, selector)?.snapshot.clone())
    }

    pub(crate) fn with_current<T>(
        &self,
        expected: &NodeRegistrationSnapshot,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let mut state = self.lock_state()?;
        let state = available_mut(&mut state)?;
        expire_maintenance(state);
        let Some(registration) = state
            .registrations
            .iter()
            .find(|registration| registration.snapshot.node_id == expected.node_id)
        else {
            return Ok(None);
        };
        if registration.snapshot.revision != expected.revision
            || registration.snapshot.tombstone_epoch != expected.tombstone_epoch
            || registration.snapshot.target != expected.target
            || !registration.admission_open
        {
            return Ok(None);
        }
        operation().map(Some)
    }

    pub(crate) fn observe(&self, expected: &NodeRegistrationSnapshot) -> io::Result<Option<u64>> {
        let mut state = self.lock_state()?;
        let state = available_mut(&mut state)?;
        expire_maintenance(state);
        let Some(registration) = state
            .registrations
            .iter()
            .find(|registration| registration.snapshot.node_id == expected.node_id)
        else {
            return Ok(None);
        };
        if registration.snapshot.revision != expected.revision
            || registration.snapshot.tombstone_epoch != expected.tombstone_epoch
            || registration.snapshot.target != expected.target
            || !registration.admission_open
        {
            return Ok(None);
        }
        Ok(Some(registration.admission_epoch))
    }

    pub(crate) fn with_observation<T>(
        &self,
        expected: &NodeRegistrationSnapshot,
        admission_epoch: u64,
        operation: impl FnOnce() -> io::Result<T>,
    ) -> io::Result<Option<T>> {
        let mut state = self.lock_state()?;
        let state = available_mut(&mut state)?;
        expire_maintenance(state);
        let Some(registration) = state
            .registrations
            .iter()
            .find(|registration| registration.snapshot.node_id == expected.node_id)
        else {
            return Ok(None);
        };
        if registration.snapshot.revision != expected.revision
            || registration.snapshot.tombstone_epoch != expected.tombstone_epoch
            || registration.snapshot.target != expected.target
            || !registration.admission_open
            || registration.admission_epoch != admission_epoch
        {
            return Ok(None);
        }
        operation().map(Some)
    }

    pub(crate) fn admit(&self, expected: &NodeRegistrationSnapshot) -> io::Result<bool> {
        let mut state = self.lock_state()?;
        let state = available_mut(&mut state)?;
        expire_maintenance(state);
        let Some(registration) = state
            .registrations
            .iter_mut()
            .find(|registration| registration.snapshot.node_id == expected.node_id)
        else {
            return Ok(false);
        };
        if registration.snapshot.revision != expected.revision
            || registration.snapshot.tombstone_epoch != expected.tombstone_epoch
            || !registration.admission_open
        {
            return Ok(false);
        }
        registration.admitted = registration
            .admitted
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Node registration admission exhausted"))?;
        Ok(true)
    }

    pub(crate) fn release(&self, expected: &NodeRegistrationSnapshot) {
        if let Ok(mut state) = self.state.lock()
            && let Ok(state) = available_mut(&mut state)
            && let Some(registration) = state
                .registrations
                .iter_mut()
                .find(|registration| registration.snapshot.node_id == expected.node_id)
            && registration.snapshot.revision == expected.revision
        {
            registration.admitted = registration.admitted.saturating_sub(1);
            self.changed.notify_all();
        }
    }

    pub(crate) fn add(
        &self,
        alias: String,
        target: String,
        node_id: String,
        local_node_id: &str,
    ) -> io::Result<NodeRegistrationSnapshot> {
        validate_alias(&alias)?;
        validate_target(&target)?;
        validate_node_id(&node_id)?;
        if node_id == local_node_id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a Boomux Node cannot register itself",
            ));
        }
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        if let Some(existing) = current.registrations.iter().find(|registration| {
            registration.snapshot.alias == alias
                || registration.snapshot.target == target
                || registration.snapshot.node_id == node_id
        }) {
            if existing.snapshot.alias == alias
                && existing.snapshot.target == target
                && existing.snapshot.node_id == node_id
            {
                return Ok(existing.snapshot.clone());
            }
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Node alias, target, or Node ID is already registered",
            ));
        }
        if current.registrations.len() >= MAX_REGISTRATIONS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "registered Node limit reached",
            ));
        }
        let revision = next(current.revision, "registration revision")?;
        let snapshot = NodeRegistrationSnapshot {
            alias,
            target,
            node_id,
            revision,
            tombstone_epoch: current.tombstone_epoch,
        };
        let mut replacement = current.clone();
        replacement.revision = revision;
        replacement.registrations.push(Registration {
            snapshot: snapshot.clone(),
            admission_open: true,
            admission_epoch: 0,
            admitted: 0,
            maintenance: None,
        });
        save(&self.path, &replacement)?;
        *current = replacement;
        Ok(snapshot)
    }

    pub(crate) fn rename(
        &self,
        selector: &str,
        alias: String,
        expected_revision: u64,
    ) -> io::Result<NodeRegistrationSnapshot> {
        validate_alias(&alias)?;
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        let index = find_index(current, selector)?;
        require_revision(&current.registrations[index], expected_revision)?;
        if !current.registrations[index].admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Node registration maintenance is in progress",
            ));
        }
        if current.registrations[index].snapshot.alias == alias {
            return Ok(current.registrations[index].snapshot.clone());
        }
        if current
            .registrations
            .iter()
            .enumerate()
            .any(|(other, registration)| other != index && registration.snapshot.alias == alias)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Node alias is already registered",
            ));
        }
        let revision = next(current.revision, "registration revision")?;
        let mut replacement = current.clone();
        replacement.revision = revision;
        replacement.registrations[index].snapshot.alias = alias;
        replacement.registrations[index].snapshot.revision = revision;
        save(&self.path, &replacement)?;
        let snapshot = replacement.registrations[index].snapshot.clone();
        *current = replacement;
        Ok(snapshot)
    }

    pub(crate) fn retarget(
        &self,
        selector: &str,
        target: String,
        verified_node_id: &str,
        expected_revision: u64,
        timeout: Duration,
    ) -> io::Result<NodeRegistrationSnapshot> {
        validate_target(&target)?;
        validate_node_id(verified_node_id)?;
        self.prepare_drain_commit(selector, expected_revision, timeout, |state, index| {
            if state.registrations[index].snapshot.node_id != verified_node_id {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "new SSH target resolved to a different Boomux Node identity",
                ));
            }
            if state.registrations[index].snapshot.target == target {
                return Ok(state.registrations[index].snapshot.clone());
            }
            if state
                .registrations
                .iter()
                .enumerate()
                .any(|(other, registration)| {
                    other != index && registration.snapshot.target == target
                })
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "SSH target is already registered",
                ));
            }
            let revision = next(state.revision, "registration revision")?;
            state.revision = revision;
            state.registrations[index].snapshot.target = target;
            state.registrations[index].snapshot.revision = revision;
            Ok(state.registrations[index].snapshot.clone())
        })
    }

    pub(crate) fn forget(
        &self,
        selector: &str,
        timeout: Duration,
    ) -> io::Result<NodeRegistrationSnapshot> {
        let expected_revision = self.inspect(selector)?.revision;
        self.prepare_drain_commit(selector, expected_revision, timeout, |state, index| {
            let tombstone_epoch = next(state.tombstone_epoch, "registration tombstone epoch")?;
            state.tombstone_epoch = tombstone_epoch;
            let mut removed = state.registrations.remove(index).snapshot;
            removed.tombstone_epoch = tombstone_epoch;
            Ok(removed)
        })
    }

    pub(crate) fn begin_upgrade_maintenance_if(
        &self,
        selector: &str,
        expected_revision: u64,
        drain_timeout: Duration,
        lease_duration: Duration,
        transition_idle: impl FnOnce() -> bool,
    ) -> io::Result<(NodeRegistrationSnapshot, String)> {
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        if !transition_idle() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "a daemon transition is already in progress",
            ));
        }
        let index = find_index(current, selector)?;
        require_revision(&current.registrations[index], expected_revision)?;
        if !current.registrations[index].admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Node registration change is already in progress",
            ));
        }
        current.registrations[index].admission_epoch = next(
            current.registrations[index].admission_epoch,
            "registration admission epoch",
        )?;
        current.registrations[index].admission_open = false;
        let token = Uuid::new_v4().to_string();
        current.registrations[index].maintenance = Some(MaintenanceLease {
            token: token.clone(),
            deadline: Instant::now() + lease_duration,
        });
        let deadline = Instant::now() + drain_timeout;
        loop {
            let current = available_mut(&mut state)?;
            let index = find_index(current, selector)?;
            require_revision(&current.registrations[index], expected_revision)?;
            if current.registrations[index].admitted == 0 {
                return Ok((current.registrations[index].snapshot.clone(), token));
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                current.registrations[index].admission_open = true;
                current.registrations[index].maintenance = None;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active Node registration operations",
                ));
            };
            let (next_state, wait) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("Node registration lock is poisoned"))?;
            state = next_state;
            if wait.timed_out() {
                let current = available_mut(&mut state)?;
                let index = find_index(current, selector)?;
                current.registrations[index].admission_open = true;
                current.registrations[index].maintenance = None;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active Node registration operations",
                ));
            }
        }
    }

    pub(crate) fn finish_upgrade_maintenance(&self, node_id: &str, token: &str) -> io::Result<()> {
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        let registration = current
            .registrations
            .iter_mut()
            .find(|registration| registration.snapshot.node_id == node_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Node registration not found")
            })?;
        if registration
            .maintenance
            .as_ref()
            .is_none_or(|maintenance| maintenance.token != token)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Node upgrade maintenance lease is not current",
            ));
        }
        registration.maintenance = None;
        registration.admission_open = true;
        self.changed.notify_all();
        Ok(())
    }

    pub(crate) fn finish_uninstall_maintenance(
        &self,
        node_id: &str,
        token: &str,
    ) -> io::Result<NodeRegistrationSnapshot> {
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        let index = current
            .registrations
            .iter()
            .position(|registration| registration.snapshot.node_id == node_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Node registration not found")
            })?;
        if current.registrations[index]
            .maintenance
            .as_ref()
            .is_none_or(|maintenance| maintenance.token != token)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Node uninstall maintenance lease is not current",
            ));
        }
        if current.registrations[index].admitted != 0 {
            return Err(io::Error::new(
                io::ErrorKind::ResourceBusy,
                "Node uninstall maintenance still has admitted operations",
            ));
        }
        let mut replacement = current.clone();
        let tombstone_epoch = next(replacement.tombstone_epoch, "registration tombstone epoch")?;
        replacement.tombstone_epoch = tombstone_epoch;
        let mut removed = replacement.registrations.remove(index).snapshot;
        removed.tombstone_epoch = tombstone_epoch;
        save(&self.path, &replacement)?;
        *current = replacement;
        self.changed.notify_all();
        Ok(removed)
    }

    pub(crate) fn renew_upgrade_maintenance(
        &self,
        node_id: &str,
        token: &str,
        lease_duration: Duration,
    ) -> io::Result<()> {
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        let registration = current
            .registrations
            .iter_mut()
            .find(|registration| registration.snapshot.node_id == node_id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "Node registration not found")
            })?;
        let maintenance = registration.maintenance.as_mut().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Node upgrade maintenance lease is not current",
            )
        })?;
        if maintenance.token != token {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Node upgrade maintenance lease is not current",
            ));
        }
        maintenance.deadline = Instant::now() + lease_duration;
        Ok(())
    }

    pub(crate) fn has_active_upgrade_maintenance(&self) -> io::Result<bool> {
        let mut state = self.lock_state()?;
        let ManagerState::Available(current) = &mut *state else {
            return Ok(false);
        };
        expire_maintenance(current);
        Ok(current
            .registrations
            .iter()
            .any(|registration| registration.maintenance.is_some()))
    }

    fn prepare_drain_commit(
        &self,
        selector: &str,
        expected_revision: u64,
        timeout: Duration,
        mutate: impl FnOnce(&mut RegistrationState, usize) -> io::Result<NodeRegistrationSnapshot>,
    ) -> io::Result<NodeRegistrationSnapshot> {
        let mut state = self.lock_state()?;
        let current = available_mut(&mut state)?;
        expire_maintenance(current);
        let index = find_index(current, selector)?;
        require_revision(&current.registrations[index], expected_revision)?;
        if !current.registrations[index].admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Node registration change is already in progress",
            ));
        }
        current.registrations[index].admission_epoch = next(
            current.registrations[index].admission_epoch,
            "registration admission epoch",
        )?;
        current.registrations[index].admission_open = false;
        let deadline = Instant::now() + timeout;
        loop {
            let current = available_mut(&mut state)?;
            let index = find_index(current, selector)?;
            require_revision(&current.registrations[index], expected_revision)?;
            if current.registrations[index].admitted == 0 {
                break;
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                current.registrations[index].admission_open = true;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active Node registration operations",
                ));
            };
            let (next_state, wait) = self
                .changed
                .wait_timeout(state, remaining)
                .map_err(|_| io::Error::other("Node registration lock is poisoned"))?;
            state = next_state;
            if wait.timed_out() {
                let current = available_mut(&mut state)?;
                let index = find_index(current, selector)?;
                current.registrations[index].admission_open = true;
                self.changed.notify_all();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out draining active Node registration operations",
                ));
            }
        }

        let current = available_mut(&mut state)?;
        let index = find_index(current, selector)?;
        let mut replacement = current.clone();
        let result = match mutate(&mut replacement, index) {
            Ok(result) => result,
            Err(error) => {
                current.registrations[index].admission_open = true;
                self.changed.notify_all();
                return Err(error);
            }
        };
        if let Some(registration) = replacement.registrations.get_mut(index) {
            registration.admission_open = true;
        }
        if let Err(error) = save(&self.path, &replacement) {
            current.registrations[index].admission_open = true;
            self.changed.notify_all();
            return Err(error);
        }
        *current = replacement;
        self.changed.notify_all();
        Ok(result)
    }

    fn lock_state(&self) -> io::Result<std::sync::MutexGuard<'_, ManagerState>> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("Node registration lock is poisoned"))
    }

    #[cfg(test)]
    fn admit_for_test(&self, selector: &str) -> io::Result<()> {
        let mut state = self.lock_state()?;
        let registration = find_mut(available_mut(&mut state)?, selector)?;
        if !registration.admission_open {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "admission closed",
            ));
        }
        registration.admitted += 1;
        Ok(())
    }

    #[cfg(test)]
    fn release_for_test(&self, selector: &str) {
        if let Ok(mut state) = self.state.lock()
            && let Ok(registration) =
                available_mut(&mut state).and_then(|state| find_mut(state, selector))
        {
            registration.admitted = registration.admitted.saturating_sub(1);
            self.changed.notify_all();
        }
    }
}

fn available(state: &ManagerState) -> io::Result<&RegistrationState> {
    match state {
        ManagerState::Available(state) => Ok(state),
        ManagerState::Unavailable(reason) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Node registration routing is disabled: {reason}"),
        )),
    }
}

fn available_mut(state: &mut ManagerState) -> io::Result<&mut RegistrationState> {
    match state {
        ManagerState::Available(state) => Ok(state),
        ManagerState::Unavailable(reason) => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("Node registration routing is disabled: {reason}"),
        )),
    }
}

fn expire_maintenance(state: &mut RegistrationState) {
    let now = Instant::now();
    for registration in &mut state.registrations {
        if registration
            .maintenance
            .as_ref()
            .is_some_and(|maintenance| maintenance.deadline <= now)
        {
            registration.maintenance = None;
            registration.admission_open = true;
        }
    }
}

fn find<'a>(state: &'a RegistrationState, selector: &str) -> io::Result<&'a Registration> {
    state
        .registrations
        .iter()
        .find(|registration| {
            registration.snapshot.alias == selector || registration.snapshot.node_id == selector
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Node registration not found"))
}

#[cfg(test)]
fn find_mut<'a>(
    state: &'a mut RegistrationState,
    selector: &str,
) -> io::Result<&'a mut Registration> {
    state
        .registrations
        .iter_mut()
        .find(|registration| {
            registration.snapshot.alias == selector || registration.snapshot.node_id == selector
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Node registration not found"))
}

fn find_index(state: &RegistrationState, selector: &str) -> io::Result<usize> {
    state
        .registrations
        .iter()
        .position(|registration| {
            registration.snapshot.alias == selector || registration.snapshot.node_id == selector
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "Node registration not found"))
}

fn require_revision(registration: &Registration, expected: u64) -> io::Result<()> {
    if registration.snapshot.revision == expected {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "Node registration revision changed: expected {expected}, current {}",
                registration.snapshot.revision
            ),
        ))
    }
}

fn validate_alias(alias: &str) -> io::Result<()> {
    if alias.is_empty()
        || alias.len() > MAX_ALIAS_BYTES
        || alias.chars().any(char::is_control)
        || alias.chars().any(char::is_whitespace)
        || Uuid::parse_str(alias).is_ok_and(|id| id.to_string() == alias)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node alias must be bounded, nonempty, whitespace-free, and not a canonical Node ID",
        ));
    }
    Ok(())
}

fn validate_target(target: &str) -> io::Result<()> {
    SshTarget::parse(target.to_owned()).map(|_| ())
}

fn validate_node_id(node_id: &str) -> io::Result<()> {
    let parsed = Uuid::parse_str(node_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid Node ID"))?;
    if parsed.to_string() != node_id {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Node ID must use canonical UUID syntax",
        ));
    }
    Ok(())
}

fn next(value: u64, label: &str) -> io::Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| io::Error::other(format!("{label} exhausted")))
}

fn load(path: &Path) -> io::Result<RegistrationState> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != effective_uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux Node registration store is not an owned regular file",
        ));
    }
    if metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux Node registration store is not owner-only",
        ));
    }
    if metadata.len() > MAX_REGISTRATION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node registration store exceeds the size limit",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut bytes)?;
    let persisted: PersistedRegistrations = serde_json::from_slice(&bytes).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("could not parse Boomux Node registrations: {error}"),
        )
    })?;
    if persisted.version != REGISTRATION_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported Boomux Node registration version {}; expected {REGISTRATION_VERSION}",
                persisted.version
            ),
        ));
    }
    validate_persisted(persisted)
}

fn validate_persisted(persisted: PersistedRegistrations) -> io::Result<RegistrationState> {
    if persisted.registrations.len() > MAX_REGISTRATIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node registration store contains too many registrations",
        ));
    }
    let mut aliases = HashSet::new();
    let mut targets = HashSet::new();
    let mut node_ids = HashSet::new();
    let mut registrations = Vec::with_capacity(persisted.registrations.len());
    for snapshot in persisted.registrations {
        validate_alias(&snapshot.alias).map_err(invalid_persisted)?;
        validate_target(&snapshot.target).map_err(invalid_persisted)?;
        validate_node_id(&snapshot.node_id).map_err(invalid_persisted)?;
        if snapshot.revision == 0
            || snapshot.revision > persisted.revision
            || snapshot.tombstone_epoch > persisted.tombstone_epoch
            || !aliases.insert(snapshot.alias.clone())
            || !targets.insert(snapshot.target.clone())
            || !node_ids.insert(snapshot.node_id.clone())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Boomux Node registration store contains invalid or duplicate data",
            ));
        }
        registrations.push(Registration {
            snapshot,
            admission_open: true,
            admission_epoch: 0,
            admitted: 0,
            maintenance: None,
        });
    }
    Ok(RegistrationState {
        revision: persisted.revision,
        tombstone_epoch: persisted.tombstone_epoch,
        registrations,
    })
}

fn invalid_persisted(error: io::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}

fn save(path: &Path, state: &RegistrationState) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Node registration path has no parent"))?;
    secure_state_dir(parent)?;
    let persisted = PersistedRegistrations {
        version: REGISTRATION_VERSION,
        revision: state.revision,
        tombstone_epoch: state.tombstone_epoch,
        registrations: state
            .registrations
            .iter()
            .map(|registration| registration.snapshot.clone())
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&persisted).map_err(io::Error::other)?;
    if bytes.len() as u64 > MAX_REGISTRATION_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Boomux Node registrations exceed the size limit",
        ));
    }
    let temporary = parent.join(format!(".node-registrations-{}.tmp", Uuid::new_v4()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    fn path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("boomux-registration-{}", Uuid::new_v4()))
            .join("boomux/node_registrations.json")
    }

    fn node_id(value: u128) -> String {
        Uuid::from_u128(value).to_string()
    }

    #[test]
    fn registration_lifecycle_is_bounded_deterministic_and_persistent() {
        let path = path();
        let manager = NodeRegistrationManager::load_at(path.clone());
        let local = node_id(1);
        let second = manager
            .add("zeta".into(), "z.example".into(), node_id(2), &local)
            .unwrap();
        let first = manager
            .add("alpha".into(), "a.example".into(), node_id(3), &local)
            .unwrap();
        assert_eq!(manager.list().unwrap()[0].alias, "alpha");
        assert_eq!(
            manager
                .add("alpha".into(), "a.example".into(), node_id(3), &local)
                .unwrap(),
            first
        );
        assert_eq!(
            manager
                .add("other".into(), "z.example".into(), node_id(4), &local)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            manager
                .add("self".into(), "self.example".into(), local.clone(), &local)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        let renamed = manager
            .rename("zeta", "beta".into(), second.revision)
            .unwrap();
        assert!(renamed.revision > second.revision);
        drop(manager);

        let restored = NodeRegistrationManager::load_at(path.clone());
        assert_eq!(restored.inspect("beta").unwrap(), renamed);
        let removed = restored.forget("beta", Duration::from_millis(10)).unwrap();
        assert!(removed.tombstone_epoch > second.tombstone_epoch);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn retarget_requires_the_pinned_identity_and_rolls_back_failed_drain() {
        let path = path();
        let manager = std::sync::Arc::new(NodeRegistrationManager::load_at(path.clone()));
        let local = node_id(1);
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &local)
            .unwrap();
        assert_eq!(
            manager
                .retarget(
                    "work",
                    "new".into(),
                    &node_id(3),
                    registration.revision,
                    Duration::from_millis(10),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(manager.inspect("work").unwrap().target, "old");

        manager.admit_for_test("work").unwrap();
        let error = manager
            .retarget(
                "work",
                "new".into(),
                &node_id(2),
                registration.revision,
                Duration::from_millis(1),
            )
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        manager.release_for_test("work");
        let changed = manager
            .retarget(
                "work",
                "new".into(),
                &node_id(2),
                registration.revision,
                Duration::from_secs(1),
            )
            .unwrap();
        assert_eq!(changed.target, "new");
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn malformed_future_and_insecure_files_are_preserved_and_disable_routing() {
        for (bytes, mode) in [
            (b"not-json".to_vec(), 0o600),
            (
                serde_json::to_vec(&serde_json::json!({
                    "version": 99,
                    "revision": 0,
                    "tombstone_epoch": 0,
                    "registrations": []
                }))
                .unwrap(),
                0o600,
            ),
            (
                serde_json::to_vec(&serde_json::json!({
                    "version": 1,
                    "revision": 0,
                    "tombstone_epoch": 0,
                    "registrations": []
                }))
                .unwrap(),
                0o644,
            ),
        ] {
            let path = path();
            secure_state_dir(path.parent().unwrap()).unwrap();
            fs::write(&path, &bytes).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            let manager = NodeRegistrationManager::load_at(path.clone());
            assert!(manager.list().is_err());
            assert_eq!(fs::read(&path).unwrap(), bytes);
            fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn drain_can_complete_after_an_admitted_operation_releases() {
        let path = path();
        let manager = std::sync::Arc::new(NodeRegistrationManager::load_at(path.clone()));
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &node_id(1))
            .unwrap();
        manager.admit_for_test("work").unwrap();
        let released = std::sync::Arc::clone(&manager);
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            released.release_for_test("work");
        });
        let changed = manager
            .retarget(
                "work",
                "new".into(),
                &node_id(2),
                registration.revision,
                Duration::from_secs(1),
            )
            .unwrap();
        worker.join().unwrap();
        assert_eq!(changed.target, "new");
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn upgrade_maintenance_is_active_while_admitted_operations_drain() {
        let path = path();
        let manager = std::sync::Arc::new(NodeRegistrationManager::load_at(path.clone()));
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &node_id(1))
            .unwrap();
        manager.admit_for_test("work").unwrap();
        let beginning = std::sync::Arc::clone(&manager);
        let revision = registration.revision;
        let worker = thread::spawn(move || {
            beginning.begin_upgrade_maintenance_if(
                "work",
                revision,
                Duration::from_secs(1),
                Duration::from_secs(1),
                || true,
            )
        });
        let deadline = Instant::now() + Duration::from_secs(1);
        while !manager.has_active_upgrade_maintenance().unwrap() {
            assert!(
                Instant::now() < deadline,
                "maintenance did not become active"
            );
            thread::sleep(Duration::from_millis(1));
        }
        manager.release_for_test("work");
        let (_, token) = worker.join().unwrap().unwrap();
        manager
            .finish_upgrade_maintenance(&registration.node_id, &token)
            .unwrap();
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn maintenance_invalidates_observations_after_admission_reopens() {
        let path = path();
        let manager = NodeRegistrationManager::load_at(path.clone());
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &node_id(1))
            .unwrap();
        let stale_epoch = manager.observe(&registration).unwrap().unwrap();
        let (_, token) = manager
            .begin_upgrade_maintenance_if(
                "work",
                registration.revision,
                Duration::from_secs(1),
                Duration::from_secs(1),
                || true,
            )
            .unwrap();
        manager
            .finish_upgrade_maintenance(&registration.node_id, &token)
            .unwrap();

        assert!(
            manager
                .with_observation(&registration, stale_epoch, || Ok(()))
                .unwrap()
                .is_none()
        );
        let current_epoch = manager.observe(&registration).unwrap().unwrap();
        assert!(
            manager
                .with_observation(&registration, current_epoch, || Ok(()))
                .unwrap()
                .is_some()
        );
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn upgrade_maintenance_blocks_registration_changes_and_expires_safely() {
        let path = path();
        let manager = NodeRegistrationManager::load_at(path.clone());
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &node_id(1))
            .unwrap();
        let (leased, token) = manager
            .begin_upgrade_maintenance_if(
                "work",
                registration.revision,
                Duration::from_secs(1),
                Duration::from_millis(1),
                || true,
            )
            .unwrap();
        assert_eq!(leased, registration);
        manager
            .renew_upgrade_maintenance(&registration.node_id, &token, Duration::from_secs(1))
            .unwrap();
        thread::sleep(Duration::from_millis(2));
        assert!(!manager.admit(&registration).unwrap());
        assert_eq!(
            manager
                .rename("work", "renamed".into(), registration.revision)
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        assert_eq!(
            manager
                .retarget(
                    "work",
                    "new".into(),
                    &node_id(2),
                    registration.revision,
                    Duration::from_millis(10),
                )
                .unwrap_err()
                .kind(),
            io::ErrorKind::WouldBlock
        );
        manager
            .finish_upgrade_maintenance(&registration.node_id, &token)
            .unwrap();
        assert!(manager.admit(&registration).unwrap());
        manager.release(&registration);

        let (_, _token) = manager
            .begin_upgrade_maintenance_if(
                "work",
                registration.revision,
                Duration::from_secs(1),
                Duration::from_millis(1),
                || true,
            )
            .unwrap();
        thread::sleep(Duration::from_millis(2));
        assert!(manager.admit(&registration).unwrap());
        manager.release(&registration);
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }

    #[test]
    fn uninstall_maintenance_atomically_persists_registration_removal() {
        let path = path();
        let manager = NodeRegistrationManager::load_at(path.clone());
        let registration = manager
            .add("work".into(), "old".into(), node_id(2), &node_id(1))
            .unwrap();
        let (_, token) = manager
            .begin_upgrade_maintenance_if(
                "work",
                registration.revision,
                Duration::from_secs(1),
                Duration::from_secs(1),
                || true,
            )
            .unwrap();

        let removed = manager
            .finish_uninstall_maintenance(&registration.node_id, &token)
            .unwrap();
        assert_eq!(removed.node_id, registration.node_id);
        assert!(removed.tombstone_epoch > registration.tombstone_epoch);
        assert!(manager.list().unwrap().is_empty());

        let restored = NodeRegistrationManager::load_at(path.clone());
        assert!(restored.list().unwrap().is_empty());
        fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
    }
}
