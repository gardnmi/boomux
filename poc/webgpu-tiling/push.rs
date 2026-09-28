//! Phone Web Push delivery for the experimental gateway. No terminal text leaves Boomux.
use std::{
    collections::HashMap,
    error::Error,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use boomux::{
    client::Client,
    protocol::{
        AgentAttentionReason, AgentInstanceSnapshot, AgentState, DaemonEventKind, Snapshot,
    },
};
use jwt_simple::prelude::ES256KeyPair;
use serde::{Deserialize, Serialize};
use tokio::task::JoinSet;
use web_push::{
    ContentEncoding, HyperWebPushClient, SubscriptionInfo, Urgency, VapidSignatureBuilder,
    WebPushClient, WebPushError, WebPushMessageBuilder,
};

const MAX_SUBSCRIPTIONS: usize = 8;
const MAX_STATE_BYTES: u64 = 32 * 1024;
const MAX_ALERTS_PER_BATCH: usize = 32;
const MAX_TRACKED_AGENTS: usize = 4_096;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    version: u32,
    private_key_pem: String,
    subscriptions: Vec<SubscriptionInfo>,
}

pub(super) struct PushService {
    path: PathBuf,
    stored: Mutex<Stored>,
    public_key: String,
    contact: String,
    http: HyperWebPushClient,
}

#[derive(Clone)]
struct Alert {
    title: &'static str,
    body: String,
    tag: String,
}

fn state_path(port: u16, node_id: &str) -> io::Result<PathBuf> {
    if uuid::Uuid::parse_str(node_id).is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Invalid Boomux Node identity",
        ));
    }
    let root = std::env::var_os("BOOMUX_STATE_HOME")
        .or_else(|| std::env::var_os("XDG_STATE_HOME"))
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/state")))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "No state directory"))?;
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "State directory must be absolute",
        ));
    }
    let directory = root.join("boomux");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)?;
    if fs::symlink_metadata(&directory)?.permissions().mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Boomux state directory is not private",
        ));
    }
    Ok(directory.join(format!("web-push-{node_id}-{port}.json")))
}

fn read_stored(path: &PathBuf) -> io::Result<Stored> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file()
        || metadata.permissions().mode() & 0o077 != 0
        || metadata.len() > MAX_STATE_BYTES
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Invalid Web Push state file",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(MAX_STATE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Web Push state is too large",
        ));
    }
    let stored: Stored = serde_json::from_slice(&bytes)?;
    if stored.version != 1 || stored.subscriptions.len() > MAX_SUBSCRIPTIONS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Unsupported Web Push state",
        ));
    }
    for subscription in &stored.subscriptions {
        validate_subscription(subscription).map_err(io::Error::other)?;
    }
    Ok(stored)
}

fn write_stored(path: &PathBuf, stored: &Stored, new: bool) -> io::Result<()> {
    let bytes = serde_json::to_vec(stored)?;
    if bytes.len() as u64 > MAX_STATE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Web Push state is too large",
        ));
    }
    let destination = if new {
        path.clone()
    } else {
        path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()))
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&destination)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    if !new {
        fs::rename(&destination, path)?;
    }
    File::open(path.parent().unwrap())?.sync_all()?;
    Ok(())
}

fn validate_subscription(subscription: &SubscriptionInfo) -> Result<(), &'static str> {
    let endpoint = &subscription.endpoint;
    // Push endpoints arrive from the browser but must never become an arbitrary URL fetch.
    if endpoint.len() > 2048
        || ![
            "https://fcm.googleapis.com/",
            "https://updates.push.services.mozilla.com/",
            "https://web.push.apple.com/",
            "https://android.googleapis.com/gcm/send/",
        ]
        .iter()
        .any(|prefix| endpoint.starts_with(prefix))
    {
        return Err("Unsupported Push endpoint");
    }
    if URL_SAFE_NO_PAD
        .decode(&subscription.keys.p256dh)
        .map_err(|_| "Invalid Push key")?
        .len()
        != 65
        || URL_SAFE_NO_PAD
            .decode(&subscription.keys.auth)
            .map_err(|_| "Invalid Push secret")?
            .len()
            != 16
    {
        return Err("Invalid Push keys");
    }
    Ok(())
}

impl PushService {
    pub(super) fn open(
        port: u16,
        node_id: &str,
        contact: String,
    ) -> Result<Arc<Self>, Box<dyn Error>> {
        let path = state_path(port, node_id)?;
        let stored = match read_stored(&path) {
            Ok(stored) => stored,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let stored = Stored {
                    version: 1,
                    private_key_pem: ES256KeyPair::generate().to_pem()?,
                    subscriptions: Vec::new(),
                };
                match write_stored(&path, &stored, true) {
                    Ok(()) => stored,
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        read_stored(&path)?
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) => return Err(error.into()),
        };
        let key = VapidSignatureBuilder::from_pem_no_sub(stored.private_key_pem.as_bytes())?;
        Ok(Arc::new(Self {
            path,
            public_key: URL_SAFE_NO_PAD.encode(key.get_public_key()),
            stored: Mutex::new(stored),
            contact,
            http: HyperWebPushClient::new(),
        }))
    }

    pub(super) fn public_key(&self) -> &str {
        &self.public_key
    }

    pub(super) fn subscribe(&self, subscription: SubscriptionInfo) -> Result<(), String> {
        validate_subscription(&subscription).map_err(str::to_owned)?;
        let mut guard = self.stored.lock().map_err(|_| "Push state unavailable")?;
        let mut next = guard.clone();
        if let Some(existing) = next
            .subscriptions
            .iter_mut()
            .find(|item| item.endpoint == subscription.endpoint)
        {
            if *existing == subscription {
                return Ok(());
            }
            *existing = subscription;
        } else {
            if next.subscriptions.len() >= MAX_SUBSCRIPTIONS {
                return Err("Phone subscription limit reached".into());
            }
            next.subscriptions.push(subscription);
        }
        write_stored(&self.path, &next, false).map_err(|error| error.to_string())?;
        *guard = next;
        Ok(())
    }

    pub(super) fn unsubscribe(&self, endpoint: &str) -> Result<(), String> {
        let mut guard = self.stored.lock().map_err(|_| "Push state unavailable")?;
        let mut next = guard.clone();
        next.subscriptions.retain(|item| item.endpoint != endpoint);
        if next.subscriptions.len() == guard.subscriptions.len() {
            return Ok(());
        }
        write_stored(&self.path, &next, false).map_err(|error| error.to_string())?;
        *guard = next;
        Ok(())
    }

    async fn send(&self, subscription: SubscriptionInfo, alert: Alert) {
        let payload = serde_json::json!({"title":alert.title,"body":alert.body,"tag":alert.tag,"url":"/agents"}).to_string();
        let result = (|| -> Result<_, WebPushError> {
            let mut signature = VapidSignatureBuilder::from_pem(
                self.stored.lock().unwrap().private_key_pem.as_bytes(),
                &subscription,
            )?;
            signature.add_claim("sub", self.contact.as_str());
            let mut message = WebPushMessageBuilder::new(&subscription);
            message.set_payload(ContentEncoding::Aes128Gcm, payload.as_bytes());
            message.set_vapid_signature(signature.build()?);
            message.set_ttl(3600);
            message.set_urgency(Urgency::Normal);
            message.build()
        })();
        let outcome = match result {
            Ok(message) => {
                tokio::time::timeout(Duration::from_secs(10), self.http.send(message)).await
            }
            Err(error) => {
                eprintln!("Web Push encoding failed: {error}");
                return;
            }
        };
        match outcome {
            Ok(Err(WebPushError::EndpointNotValid(_) | WebPushError::EndpointNotFound(_))) => {
                let _ = self.unsubscribe(&subscription.endpoint);
            }
            Ok(Err(error)) => eprintln!("Web Push delivery failed: {error}"),
            Err(_) => eprintln!("Web Push delivery timed out"),
            Ok(Ok(())) => {}
        }
    }

    pub(super) async fn watch(self: Arc<Self>, client: Client, node_id: String) {
        let mut cursor = None;
        let mut states = HashMap::<String, AgentState>::new();
        loop {
            let reader = client.clone();
            let batch =
                tokio::task::spawn_blocking(move || reader.events(cursor, 256, 3_000)).await;
            let batch = match batch {
                Ok(Ok(batch)) => batch,
                _ => {
                    cursor = None;
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };
            cursor = Some(batch.cursor);
            if let Some(snapshot) = batch.snapshot {
                states = baseline_states(&snapshot);
                continue; // Baselines, including after reconnect, are never new notifications.
            }
            if client.node_identity().ok().as_deref() != Some(&node_id) {
                break;
            }
            let mut jobs = JoinSet::new();
            let mut alerts = 0;
            for event in batch.events {
                let Some(alert) = alert_for_event(&mut states, &event.kind) else {
                    continue;
                };
                if alerts >= MAX_ALERTS_PER_BATCH {
                    break;
                }
                alerts += 1;
                let subscriptions = match self.stored.lock() {
                    Ok(state) => state.subscriptions.clone(),
                    Err(_) => break,
                };
                for subscription in subscriptions {
                    if jobs.len() >= MAX_SUBSCRIPTIONS {
                        let _ = jobs.join_next().await;
                    }
                    let service = self.clone();
                    let alert = alert.clone();
                    jobs.spawn(async move { service.send(subscription, alert).await });
                }
            }
            while jobs.join_next().await.is_some() {}
        }
    }
}

fn baseline_states(snapshot: &Snapshot) -> HashMap<String, AgentState> {
    snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.agents)
        .take(MAX_TRACKED_AGENTS)
        .map(|agent| (agent.id.clone(), agent.observation.state))
        .collect()
}

fn alert_for_event(
    states: &mut HashMap<String, AgentState>,
    kind: &DaemonEventKind,
) -> Option<Alert> {
    let agent: &AgentInstanceSnapshot = match kind {
        DaemonEventKind::AgentRegistered { agent, .. }
        | DaemonEventKind::AgentStateChanged { agent, .. }
        | DaemonEventKind::AgentCompleted { agent, .. } => agent,
        _ => return None,
    };
    // Bound long-lived gateway state. If the Node exceeds this limit, suppress
    // alerts for newly observed Agents until the next baseline instead of
    // growing memory without limit or guessing whether an event is new.
    if !states.contains_key(&agent.id) && states.len() >= MAX_TRACKED_AGENTS {
        return None;
    }
    let previous = states.insert(agent.id.clone(), agent.observation.state);
    if previous == Some(agent.observation.state) {
        return None;
    }
    let attention = agent
        .attention
        .as_ref()
        .filter(|attention| attention.observation.revision == agent.observation.revision);
    let title = match (agent.observation.state, attention.map(|item| item.reason)) {
        (AgentState::Blocked, Some(AgentAttentionReason::Blocked)) => {
            "Boomux Agent needs attention"
        }
        (AgentState::Done, Some(AgentAttentionReason::Completed)) => "Boomux Agent completed",
        (AgentState::Idle, _) if previous == Some(AgentState::Working) => "Boomux Agent completed",
        _ => return None,
    };
    Some(Alert {
        title,
        body: agent.name.chars().take(80).collect(),
        tag: format!("{}-{}", agent.id, agent.observation.revision),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(
        state: AgentState,
        reason: Option<AgentAttentionReason>,
        revision: u64,
    ) -> DaemonEventKind {
        let observation = json!({"revision":revision,"state":state,"authority":"lifecycle_integration",
            "evidence":"test","confidence":100,"observed_at_ms":revision});
        let agent = serde_json::from_value(json!({
            "id":"agent","workspace_id":"workspace","shell_id":"shell","run_id":"run",
            "name":"Codex","integration":"codex","external_session_id":null,
            "started_at_ms":1,"ended_at_ms":null,"observation":observation,
            "attention":reason.map(|reason|json!({"reason":reason,"observation":observation})),
        }))
        .unwrap();
        DaemonEventKind::AgentStateChanged {
            workspace_id: "workspace".into(),
            shell_id: "shell".into(),
            agent,
        }
    }

    #[test]
    fn alerts_only_for_new_attention_or_working_to_idle_transitions() {
        let mut states = HashMap::new();
        assert!(alert_for_event(&mut states, &event(AgentState::Working, None, 1)).is_none());
        let blocked = event(AgentState::Blocked, Some(AgentAttentionReason::Blocked), 2);
        assert_eq!(
            alert_for_event(&mut states, &blocked).unwrap().title,
            "Boomux Agent needs attention"
        );
        assert!(alert_for_event(&mut states, &blocked).is_none());
        assert!(
            alert_for_event(
                &mut states,
                &event(AgentState::Blocked, Some(AgentAttentionReason::Blocked), 3)
            )
            .is_none()
        );
        assert!(alert_for_event(&mut states, &event(AgentState::Working, None, 4)).is_none());
        assert_eq!(
            alert_for_event(&mut states, &event(AgentState::Idle, None, 5))
                .unwrap()
                .title,
            "Boomux Agent completed"
        );
        assert!(alert_for_event(&mut states, &event(AgentState::Idle, None, 6)).is_none());
        assert_eq!(
            alert_for_event(
                &mut states,
                &event(AgentState::Done, Some(AgentAttentionReason::Completed), 7)
            )
            .unwrap()
            .title,
            "Boomux Agent completed"
        );
    }

    #[test]
    fn subscriptions_accept_only_known_https_push_services_and_valid_keys() {
        let key = URL_SAFE_NO_PAD.encode([4; 65]);
        let auth = URL_SAFE_NO_PAD.encode([7; 16]);
        let valid =
            SubscriptionInfo::new("https://fcm.googleapis.com/fcm/send/device", &key, &auth);
        assert!(validate_subscription(&valid).is_ok());
        assert!(
            validate_subscription(&SubscriptionInfo::new(
                "http://127.0.0.1/private",
                &key,
                &auth
            ))
            .is_err()
        );
        assert!(
            validate_subscription(&SubscriptionInfo::new(
                "https://fcm.googleapis.com.evil.test/",
                &key,
                &auth
            ))
            .is_err()
        );
        assert!(
            validate_subscription(&SubscriptionInfo::new(
                "https://fcm.googleapis.com/fcm/send/device",
                "bad",
                &auth
            ))
            .is_err()
        );
    }

    #[test]
    fn private_push_state_round_trips() {
        let path =
            std::env::temp_dir().join(format!("boomux-push-test-{}.json", uuid::Uuid::new_v4()));
        let state = Stored {
            version: 1,
            private_key_pem: "test".into(),
            subscriptions: Vec::new(),
        };
        write_stored(&path, &state, true).unwrap();
        assert_eq!(
            fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(read_stored(&path).unwrap().private_key_pem, "test");
        fs::remove_file(path).unwrap();
    }
}
