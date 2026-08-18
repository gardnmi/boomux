use std::collections::HashMap;
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderName, HeaderValue,
    REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::uri::Authority;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use boomux::client::{self, Client};
use boomux::protocol::{
    AgentAttentionReason, AgentState, CombinedNodeSnapshot, NodeProjectionHealthCode, ShellOwner,
};
use serde::Serialize;

const TERMINAL_OUTPUT_BYTES: usize = 256 * 1024;
const INDEX_HTML: &str = include_str!("../assets/mobile-web/index.html");
const APP_JS: &str = include_str!("../assets/mobile-web/app.js");
const MOBILE_MODEL_JS: &str = include_str!("../assets/mobile-web/mobile-model.js");
const STYLES_CSS: &str = include_str!("../assets/mobile-web/styles.css");
const MANIFEST: &str = include_str!("../assets/mobile-web/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../assets/mobile-web/service-worker.js");
const ICON: &str = include_str!("../assets/mobile-web/icon.svg");
const ICON_192: &[u8] = include_bytes!("../assets/mobile-web/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/mobile-web/icon-512.png");
const TAILSCALE_LOGIN: HeaderName = HeaderName::from_static("tailscale-user-login");

#[derive(Clone)]
struct AppState {
    client: Client,
    trusted_user: Option<Arc<str>>,
}

#[derive(Debug, Clone, Serialize)]
struct AttentionView {
    reason: AgentAttentionReason,
    observation_revision: u64,
    observed_at_ms: u64,
}

#[derive(Debug, Clone, Serialize)]
struct AgentCard {
    node_id: String,
    node_alias: String,
    node_local: bool,
    node_current: bool,
    node_stale: bool,
    node_health: NodeProjectionHealthCode,
    agent_id: String,
    workspace_id: String,
    workspace_name: String,
    shell_id: String,
    shell_name: String,
    run_id: String,
    run_current: bool,
    name: String,
    integration: String,
    state: AgentState,
    observation_revision: u64,
    observed_at_ms: u64,
    started_at_ms: u64,
    ended_at_ms: Option<u64>,
    attention: Option<AttentionView>,
    #[serde(skip)]
    schedule_owned: bool,
}

#[derive(Debug, Serialize)]
struct SnapshotCounts {
    agents: usize,
    attention: usize,
    active: usize,
    stale_nodes: usize,
}

#[derive(Debug, Serialize)]
struct MobileSnapshot {
    generated_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    viewer: Option<String>,
    counts: SnapshotCounts,
    agents: Vec<AgentCard>,
}

#[derive(Debug, Serialize)]
struct TimelineEntry {
    kind: &'static str,
    at_ms: u64,
    title: String,
    body: String,
    tone: &'static str,
}

#[derive(Debug, Serialize)]
struct AgentDetail {
    agent: AgentCard,
    timeline: Vec<TimelineEntry>,
    terminal_output: Option<String>,
    terminal_available: bool,
    notice: &'static str,
}

#[derive(Debug, Serialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    code: &'static str,
    message: &'static str,
}

struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
}

impl ApiError {
    fn daemon() -> Self {
        Self {
            status: StatusCode::BAD_GATEWAY,
            code: "daemon_unavailable",
            message: "Boomux could not refresh Agent state",
        }
    }

    fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "agent_not_found",
            message: "The qualified Agent is no longer available",
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: self.code,
                    message: self.message,
                },
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}

pub(crate) fn run(port: u16, trusted_user: Option<&str>) -> Result<(), Box<dyn Error>> {
    let trusted_user = trusted_user.map(str::trim);
    if trusted_user.is_some_and(|value| value.is_empty() || value.len() > 320 || !value.is_ascii())
    {
        return Err("--trusted-user must be a nonempty ASCII login of at most 320 bytes".into());
    }
    let client = client::connect_or_start()?;
    let state = AppState {
        client,
        trusted_user: trusted_user.map(Arc::from),
    };
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(serve(address, state))
}

async fn serve(address: SocketAddr, state: AppState) -> Result<(), Box<dyn Error>> {
    let app = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/mobile-model.js", get(mobile_model_js))
        .route("/styles.css", get(styles))
        .route("/manifest.webmanifest", get(manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/api/snapshot", get(snapshot))
        .route("/api/agents/{node_id}/{agent_id}", get(agent_detail))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            secure_request,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(address).await?;
    println!("Boomux mobile dashboard: http://{address}");
    println!("Tailnet proxy: tailscale serve --bg http://{address}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = interrupt => {},
        _ = terminate => {},
    }
}

async fn secure_request(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let mut response = if !authorized(request.headers(), state.trusted_user.as_deref()) {
        let mut response = (
            StatusCode::UNAUTHORIZED,
            Json(ErrorEnvelope {
                error: ErrorBody {
                    code: "unauthorized",
                    message: "This Tailscale identity is not authorized",
                },
            }),
        )
            .into_response();
        response
            .headers_mut()
            .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; img-src 'self' data:; manifest-src 'self'; object-src 'none'; script-src 'self'; style-src 'self'; frame-ancestors 'none'",
        ),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn authorized(headers: &HeaderMap, trusted_user: Option<&str>) -> bool {
    let Some(trusted_user) = trusted_user else {
        return true;
    };
    match headers
        .get(&TAILSCALE_LOGIN)
        .and_then(|value| value.to_str().ok())
    {
        Some(login) => login == trusted_user,
        None => loopback_host(headers),
    }
}

fn loopback_host(headers: &HeaderMap) -> bool {
    headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .is_some_and(|authority| matches!(authority.host(), "127.0.0.1" | "localhost"))
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    asset(
        APP_JS,
        "text/javascript; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn mobile_model_js() -> Response {
    asset(
        MOBILE_MODEL_JS,
        "text/javascript; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn styles() -> Response {
    asset(
        STYLES_CSS,
        "text/css; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn manifest() -> Response {
    asset(
        MANIFEST,
        "application/manifest+json; charset=utf-8",
        "public, max-age=3600",
    )
}

async fn service_worker() -> Response {
    asset(SERVICE_WORKER, "text/javascript; charset=utf-8", "no-cache")
}

async fn icon() -> Response {
    asset(ICON, "image/svg+xml", "public, max-age=86400")
}

async fn icon_192() -> Response {
    binary_asset(ICON_192, "image/png")
}

async fn icon_512() -> Response {
    binary_asset(ICON_512, "image/png")
}

fn asset(body: &'static str, content_type: &'static str, cache: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response()
}

fn binary_asset(body: &'static [u8], content_type: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=86400"),
            ),
        ],
        body,
    )
        .into_response()
}

async fn snapshot(State(state): State<AppState>, headers: HeaderMap) -> Result<Response, ApiError> {
    let viewer = headers
        .get(&TAILSCALE_LOGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let client = state.client.clone();
    let combined = tokio::task::spawn_blocking(move || client.combined_node_snapshot(None))
        .await
        .map_err(|_| ApiError::daemon())?
        .map_err(|_| ApiError::daemon())?;
    let mut response = Json(project_snapshot(combined, viewer)).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn agent_detail(
    State(state): State<AppState>,
    Path((node_id, agent_id)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    if uuid::Uuid::parse_str(&node_id).is_err() || uuid::Uuid::parse_str(&agent_id).is_err() {
        return Err(ApiError::not_found());
    }
    let client = state.client.clone();
    let detail = tokio::task::spawn_blocking(move || {
        let combined = client
            .combined_node_snapshot(None)
            .map_err(|_| ApiError::daemon())?;
        build_agent_detail(&client, &combined, &node_id, &agent_id)
    })
    .await
    .map_err(|_| ApiError::daemon())??;
    let mut response = Json(detail).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

fn project_snapshot(combined: CombinedNodeSnapshot, viewer: Option<String>) -> MobileSnapshot {
    let stale_nodes = combined.nodes.iter().filter(|node| node.stale).count();
    let mut agents = project_visible_agents(&combined);
    agents.sort_by(|left, right| {
        agent_priority(left)
            .cmp(&agent_priority(right))
            .then_with(|| right.observed_at_ms.cmp(&left.observed_at_ms))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.node_id.cmp(&right.node_id))
    });
    let counts = SnapshotCounts {
        agents: agents.len(),
        attention: agents
            .iter()
            .filter(|agent| agent.attention.is_some())
            .count(),
        active: agents
            .iter()
            .filter(|agent| {
                agent.run_current
                    && agent.node_current
                    && matches!(agent.state, AgentState::Working | AgentState::Blocked)
            })
            .count(),
        stale_nodes,
    };
    MobileSnapshot {
        generated_at_ms: unix_time_ms(),
        viewer,
        counts,
        agents,
    }
}

fn project_agents(combined: &CombinedNodeSnapshot) -> Vec<AgentCard> {
    let mut agents = Vec::new();
    for node in &combined.nodes {
        if let Some(snapshot) = &node.local_snapshot {
            for workspace in &snapshot.workspaces {
                let shell_names = workspace
                    .shells
                    .iter()
                    .map(|shell| (shell.id.as_str(), shell.name.as_str()))
                    .collect::<HashMap<_, _>>();
                let current_runs = workspace
                    .shells
                    .iter()
                    .filter_map(|shell| {
                        shell
                            .run
                            .as_ref()
                            .map(|run| (shell.id.as_str(), run.id.as_str()))
                    })
                    .collect::<HashMap<_, _>>();
                let schedule_owned = workspace
                    .shells
                    .iter()
                    .map(|shell| {
                        (
                            shell.id.as_str(),
                            matches!(shell.owner, ShellOwner::Schedule { .. }),
                        )
                    })
                    .collect::<HashMap<_, _>>();
                for agent in &workspace.agents {
                    agents.push(AgentCard {
                        node_id: node.node_id.clone(),
                        node_alias: node.alias.clone(),
                        node_local: true,
                        node_current: node.current,
                        node_stale: node.stale,
                        node_health: node.health,
                        agent_id: agent.id.clone(),
                        workspace_id: agent.workspace_id.clone(),
                        workspace_name: workspace.name.clone(),
                        shell_id: agent.shell_id.clone(),
                        shell_name: shell_names
                            .get(agent.shell_id.as_str())
                            .copied()
                            .unwrap_or("removed Shell")
                            .to_owned(),
                        run_id: agent.run_id.clone(),
                        run_current: current_runs.get(agent.shell_id.as_str()).copied()
                            == Some(agent.run_id.as_str()),
                        name: agent.name.clone(),
                        integration: agent.integration.clone(),
                        state: agent.observation.state,
                        observation_revision: agent.observation.revision,
                        observed_at_ms: agent.observation.observed_at_ms,
                        started_at_ms: agent.started_at_ms,
                        ended_at_ms: agent.ended_at_ms,
                        attention: agent.attention.as_ref().map(|attention| AttentionView {
                            reason: attention.reason,
                            observation_revision: attention.observation.revision,
                            observed_at_ms: attention.observation.observed_at_ms,
                        }),
                        schedule_owned: schedule_owned
                            .get(agent.shell_id.as_str())
                            .copied()
                            .unwrap_or(false),
                    });
                }
            }
        } else if let Some(projection) = &node.remote_projection {
            let workspace_names = projection
                .workspaces
                .iter()
                .map(|workspace| (workspace.id.as_str(), workspace.name.as_str()))
                .collect::<HashMap<_, _>>();
            let shell_names = projection
                .shells
                .iter()
                .map(|shell| (shell.id.as_str(), shell.name.as_str()))
                .collect::<HashMap<_, _>>();
            let current_runs = projection
                .shells
                .iter()
                .filter_map(|shell| {
                    shell
                        .run_id
                        .as_deref()
                        .map(|run_id| (shell.id.as_str(), run_id))
                })
                .collect::<HashMap<_, _>>();
            let schedule_owned = projection
                .shells
                .iter()
                .map(|shell| {
                    (
                        shell.id.as_str(),
                        matches!(shell.owner, ShellOwner::Schedule { .. }),
                    )
                })
                .collect::<HashMap<_, _>>();
            for agent in &projection.agents {
                agents.push(AgentCard {
                    node_id: node.node_id.clone(),
                    node_alias: node.alias.clone(),
                    node_local: false,
                    node_current: node.current,
                    node_stale: node.stale,
                    node_health: node.health,
                    agent_id: agent.id.clone(),
                    workspace_id: agent.workspace_id.clone(),
                    workspace_name: workspace_names
                        .get(agent.workspace_id.as_str())
                        .copied()
                        .unwrap_or("unknown Workspace")
                        .to_owned(),
                    shell_id: agent.shell_id.clone(),
                    shell_name: shell_names
                        .get(agent.shell_id.as_str())
                        .copied()
                        .unwrap_or("removed Shell")
                        .to_owned(),
                    run_id: agent.run_id.clone(),
                    run_current: current_runs.get(agent.shell_id.as_str()).copied()
                        == Some(agent.run_id.as_str()),
                    name: agent.name.clone(),
                    integration: agent.integration.clone(),
                    state: agent.state,
                    observation_revision: agent.observation_revision,
                    observed_at_ms: agent.observed_at_ms,
                    started_at_ms: agent.started_at_ms,
                    ended_at_ms: agent.ended_at_ms,
                    attention: agent.attention.as_ref().map(|attention| AttentionView {
                        reason: attention.reason,
                        observation_revision: attention.observation_revision,
                        observed_at_ms: attention.observed_at_ms,
                    }),
                    schedule_owned: schedule_owned
                        .get(agent.shell_id.as_str())
                        .copied()
                        .unwrap_or(false),
                });
            }
        }
    }
    agents
}

fn project_visible_agents(combined: &CombinedNodeSnapshot) -> Vec<AgentCard> {
    let agents = project_agents(combined);
    let mut winners = HashMap::<(String, String, String), (u64, u64, String)>::new();
    for agent in &agents {
        if agent.schedule_owned
            || !agent.run_current
            || matches!(agent.state, AgentState::Inactive | AgentState::Done)
        {
            continue;
        }
        let key = (
            agent.node_id.clone(),
            agent.shell_id.clone(),
            agent.run_id.clone(),
        );
        let rank = (
            agent.observed_at_ms,
            agent.started_at_ms,
            agent.agent_id.clone(),
        );
        winners
            .entry(key)
            .and_modify(|winner| {
                if rank > *winner {
                    *winner = rank.clone();
                }
            })
            .or_insert(rank);
    }

    agents
        .into_iter()
        .filter(|agent| {
            if agent.schedule_owned {
                return false;
            }
            if agent.attention.is_some() {
                return true;
            }
            let key = (
                agent.node_id.clone(),
                agent.shell_id.clone(),
                agent.run_id.clone(),
            );
            winners
                .get(&key)
                .is_some_and(|winner| winner.2 == agent.agent_id)
        })
        .collect()
}

fn build_agent_detail(
    client: &Client,
    combined: &CombinedNodeSnapshot,
    node_id: &str,
    agent_id: &str,
) -> Result<AgentDetail, ApiError> {
    let agent = project_visible_agents(combined)
        .into_iter()
        .find(|agent| agent.node_id == node_id && agent.agent_id == agent_id)
        .ok_or_else(ApiError::not_found)?;
    let mut timeline = Vec::new();
    let evidence = combined
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .and_then(|node| node.local_snapshot.as_ref())
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.agents)
                .find(|candidate| candidate.id == agent_id)
        })
        .map(|candidate| candidate.observation.evidence.clone());
    timeline.push(TimelineEntry {
        kind: "status",
        at_ms: agent.observed_at_ms,
        title: format!("Agent is {}", state_label(agent.state)),
        body: evidence.unwrap_or_else(|| {
            if agent.node_local {
                "Lifecycle observation has no displayable evidence.".into()
            } else {
                "Reduced remote projection; lifecycle evidence remains on the owning Node.".into()
            }
        }),
        tone: state_tone(agent.state),
    });
    if let Some(attention) = &agent.attention {
        timeline.push(TimelineEntry {
            kind: "attention",
            at_ms: attention.observed_at_ms,
            title: match attention.reason {
                AgentAttentionReason::Blocked => "Needs attention".into(),
                AgentAttentionReason::Completed => "Completed work".into(),
            },
            body: "This durable attention remains until it is explicitly acknowledged.".into(),
            tone: match attention.reason {
                AgentAttentionReason::Blocked => "blocked",
                AgentAttentionReason::Completed => "done",
            },
        });
    }
    timeline.sort_by_key(|entry| entry.at_ms);

    let current_run = combined
        .nodes
        .iter()
        .find(|node| node.node_id == node_id)
        .and_then(|node| node.local_snapshot.as_ref())
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.shells)
                .find(|shell| shell.id == agent.shell_id)
        })
        .and_then(|shell| shell.run.as_ref())
        .is_some_and(|run| run.id == agent.run_id);
    let (terminal_output, terminal_read_failed) = if agent.node_local && current_run {
        match client.read_shell_at(
            agent.shell_id.clone(),
            TERMINAL_OUTPUT_BYTES,
            Some(agent.run_id.clone()),
            Some(0),
            0,
        ) {
            Ok(output) => (
                Some(String::from_utf8_lossy(&output.bytes).into_owned()),
                false,
            ),
            Err(error) => {
                eprintln!(
                    "boomux: mobile terminal read failed for Agent {} on Shell {}: {error}",
                    agent.agent_id, agent.shell_id
                );
                (None, true)
            }
        }
    } else {
        (None, false)
    };
    let terminal_available = terminal_output.is_some();
    Ok(AgentDetail {
        agent,
        timeline,
        terminal_output,
        terminal_available,
        notice: if terminal_available {
            "Rendered Shell output is a bounded terminal view, not a structured conversation transcript."
        } else if terminal_read_failed {
            "The exact run-scoped terminal read failed. Refresh after checking the Boomux server log."
        } else if current_run {
            "Rendered terminal output is currently unavailable for this local Shell run."
        } else if combined
            .nodes
            .iter()
            .any(|node| node.node_id == node_id && node.local)
        {
            "The Agent's exact Shell run is no longer current, so Boomux will not substitute output from a later run."
        } else {
            "Remote cached projections do not contain terminal output; the owning Node remains authoritative."
        },
    })
}

fn agent_priority(agent: &AgentCard) -> u8 {
    match agent.attention.as_ref().map(|attention| attention.reason) {
        Some(AgentAttentionReason::Blocked) => 0,
        Some(AgentAttentionReason::Completed) => 1,
        None if !agent.run_current || !agent.node_current => 8,
        None => match agent.state {
            AgentState::Blocked => 2,
            AgentState::Working => 3,
            AgentState::Idle => 4,
            AgentState::Unknown => 5,
            AgentState::Inactive => 6,
            AgentState::Done => 7,
        },
    }
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Unknown => "unknown",
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Inactive => "inactive",
        AgentState::Done => "done",
    }
}

fn state_tone(state: AgentState) -> &'static str {
    match state {
        AgentState::Blocked => "attention",
        AgentState::Working => "active",
        AgentState::Done => "success",
        AgentState::Unknown | AgentState::Idle | AgentState::Inactive => "neutral",
    }
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use boomux::protocol::{
        AgentAttentionSnapshot, AgentAuthority, AgentInstanceSnapshot, AgentObservationSnapshot,
        CombinedNode, NodeProjectionAgent, NodeProjectionAttention, NodeProjectionShell,
        NodeProjectionSnapshot, SchedulerHealth, SchedulerState, ShellOwner, ShellRunSnapshot,
        ShellSnapshot, ShellStatus, Snapshot, WorkspaceSnapshot,
    };
    use std::path::PathBuf;

    fn scheduler() -> SchedulerHealth {
        SchedulerHealth {
            state: SchedulerState::Active,
            max_concurrent: 4,
            active_executions: 0,
        }
    }

    fn combined_snapshot() -> CombinedNodeSnapshot {
        let local_agent = AgentInstanceSnapshot {
            id: "00000000-0000-0000-0000-000000000003".into(),
            workspace_id: "00000000-0000-0000-0000-000000000002".into(),
            shell_id: "00000000-0000-0000-0000-000000000004".into(),
            run_id: "00000000-0000-0000-0000-000000000005".into(),
            name: "local-active".into(),
            integration: "opencode".into(),
            external_session_id: Some("session-local".into()),
            cwd: Some(PathBuf::from("/work")),
            started_at_ms: 10,
            ended_at_ms: None,
            observation: AgentObservationSnapshot {
                revision: 2,
                state: AgentState::Working,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "working".into(),
                confidence: 100,
                observed_at_ms: 20,
            },
            attention: None,
        };
        let mut historical_agent = local_agent.clone();
        historical_agent.id = "00000000-0000-0000-0000-000000000006".into();
        historical_agent.run_id = "00000000-0000-0000-0000-000000000007".into();
        historical_agent.name = "historical-blocked".into();
        historical_agent.observation.state = AgentState::Blocked;
        historical_agent.observation.observed_at_ms = 30;

        let local = CombinedNode {
            node_id: "00000000-0000-0000-0000-000000000001".into(),
            alias: "local".into(),
            local: true,
            route: None,
            registration_revision: None,
            health: NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 30,
            observed_protocol_version: Some(41),
            observed_capabilities: Vec::new(),
            observed_helper_version: Some("0.21.0".into()),
            workspace_owner_eligible: true,
            workspace_owner_unavailable_reason: None,
            scheduler: scheduler(),
            local_snapshot: Some(Snapshot {
                workspaces: vec![WorkspaceSnapshot {
                    id: "00000000-0000-0000-0000-000000000002".into(),
                    revision: 1,
                    name: "work".into(),
                    default_cwd: Some(PathBuf::from("/work")),
                    shells: vec![ShellSnapshot {
                        id: "00000000-0000-0000-0000-000000000004".into(),
                        revision: 1,
                        workspace_id: "00000000-0000-0000-0000-000000000002".into(),
                        name: "agent".into(),
                        cwd: PathBuf::from("/work"),
                        command: Vec::new(),
                        owner: ShellOwner::User,
                        status: ShellStatus::Running,
                        run: Some(ShellRunSnapshot {
                            id: "00000000-0000-0000-0000-000000000005".into(),
                            generation: 1,
                            started_at_ms: 10,
                            ended_at_ms: None,
                            exit_reason: None,
                            output_revision: 1,
                            environment_has_run_id: true,
                        }),
                        recovered_agent_id: None,
                        foreground_process: Some("opencode".into()),
                    }],
                    launchers: Vec::new(),
                    agents: vec![local_agent, historical_agent],
                    schedules: Vec::new(),
                }],
                focused_terminal: None,
                scheduler: Some(scheduler()),
            }),
            remote_projection: None,
        };

        let remote = CombinedNode {
            node_id: "00000000-0000-0000-0000-000000000008".into(),
            alias: "laptop".into(),
            local: false,
            route: Some("laptop".into()),
            registration_revision: Some(1),
            health: NodeProjectionHealthCode::Stale,
            current: false,
            stale: true,
            observed_at_ms: 40,
            observed_protocol_version: Some(41),
            observed_capabilities: Vec::new(),
            observed_helper_version: Some("0.21.0".into()),
            workspace_owner_eligible: false,
            workspace_owner_unavailable_reason: Some("stale".into()),
            scheduler: scheduler(),
            local_snapshot: None,
            remote_projection: Some(NodeProjectionSnapshot {
                node_id: "00000000-0000-0000-0000-000000000008".into(),
                workspaces: vec![boomux::protocol::NodeProjectionWorkspace {
                    id: "00000000-0000-0000-0000-000000000009".into(),
                    name: "remote-work".into(),
                    item_count: 1,
                    attention_count: 1,
                }],
                shells: vec![NodeProjectionShell {
                    id: "00000000-0000-0000-0000-000000000010".into(),
                    workspace_id: "00000000-0000-0000-0000-000000000009".into(),
                    name: "remote-agent".into(),
                    owner: ShellOwner::User,
                    status: ShellStatus::Running,
                    run_id: Some("00000000-0000-0000-0000-000000000011".into()),
                    generation: Some(1),
                    started_at_ms: Some(10),
                    ended_at_ms: None,
                    recovered_agent_id: None,
                }],
                launchers: Vec::new(),
                agents: vec![NodeProjectionAgent {
                    id: "00000000-0000-0000-0000-000000000012".into(),
                    workspace_id: "00000000-0000-0000-0000-000000000009".into(),
                    shell_id: "00000000-0000-0000-0000-000000000010".into(),
                    run_id: "00000000-0000-0000-0000-000000000011".into(),
                    name: "remote-blocked".into(),
                    integration: "pi".into(),
                    state: AgentState::Blocked,
                    observation_revision: 3,
                    observed_at_ms: 40,
                    started_at_ms: 10,
                    ended_at_ms: None,
                    attention: Some(NodeProjectionAttention {
                        reason: AgentAttentionReason::Blocked,
                        observation_revision: 3,
                        observed_at_ms: 40,
                    }),
                }],
                schedules: Vec::new(),
                executions: Vec::new(),
                executions_truncated: false,
                scheduler: scheduler(),
            }),
        };

        CombinedNodeSnapshot {
            nodes: vec![local, remote],
            workspaces: Vec::new(),
            external_workspaces: Vec::new(),
            focused_terminal: None,
        }
    }

    #[test]
    fn trusted_user_requires_the_exact_serve_identity() {
        let mut headers = HeaderMap::new();
        assert!(authorized(&headers, None));
        assert!(!authorized(&headers, Some("owner@example.com")));
        headers.insert(HOST, HeaderValue::from_static("127.0.0.1:3737"));
        assert!(authorized(&headers, Some("owner@example.com")));
        headers.insert(HOST, HeaderValue::from_static("desktop.tailnet.ts.net"));
        assert!(!authorized(&headers, Some("owner@example.com")));
        headers.insert(
            &TAILSCALE_LOGIN,
            HeaderValue::from_static("owner@example.com"),
        );
        assert!(authorized(&headers, Some("owner@example.com")));
        assert!(!authorized(&headers, Some("other@example.com")));
        headers.insert(
            &TAILSCALE_LOGIN,
            HeaderValue::from_static("other@example.com"),
        );
        headers.insert(HOST, HeaderValue::from_static("localhost:3737"));
        assert!(!authorized(&headers, Some("owner@example.com")));
    }

    #[test]
    fn app_shell_is_installable_and_has_no_inline_code() {
        assert!(INDEX_HTML.contains("rel=\"manifest\""));
        assert!(INDEX_HTML.contains("type=\"module\" src=\"app.js\""));
        assert!(!INDEX_HTML.contains("<script>"));
        assert!(MANIFEST.contains("\"display\": \"standalone\""));
        assert!(SERVICE_WORKER.contains("/api/"));
    }

    #[test]
    fn projection_keeps_qualified_authority_and_current_run_semantics() {
        let snapshot = project_snapshot(combined_snapshot(), None);

        assert_eq!(snapshot.counts.agents, 2);
        assert_eq!(snapshot.counts.attention, 1);
        assert_eq!(snapshot.counts.active, 1);
        assert_eq!(snapshot.counts.stale_nodes, 1);
        assert_eq!(snapshot.agents[0].name, "remote-blocked");
        assert!(snapshot.agents[0].attention.is_some());
        assert!(!snapshot.agents[0].node_current);
        let current = snapshot
            .agents
            .iter()
            .find(|agent| agent.name == "local-active")
            .unwrap();
        assert!(current.run_current);
        assert!(
            snapshot
                .agents
                .iter()
                .all(|agent| agent.name != "historical-blocked")
        );
        assert_ne!(current.node_id, snapshot.agents[0].node_id);
    }

    #[test]
    fn projection_matches_current_agent_and_attention_visibility() {
        let mut combined = combined_snapshot();
        let workspace = &mut combined.nodes[0]
            .local_snapshot
            .as_mut()
            .unwrap()
            .workspaces[0];
        let template = workspace.agents[0].clone();

        let mut newest = template.clone();
        newest.id = "00000000-0000-0000-0000-000000000013".into();
        newest.name = "newest-current".into();
        newest.started_at_ms = 40;
        newest.observation.revision = 4;
        newest.observation.state = AgentState::Idle;
        newest.observation.observed_at_ms = 50;
        workspace.agents.push(newest);

        let mut completed = template.clone();
        completed.id = "00000000-0000-0000-0000-000000000014".into();
        completed.name = "completed-attention".into();
        completed.run_id = "00000000-0000-0000-0000-000000000015".into();
        completed.ended_at_ms = Some(60);
        completed.observation = AgentObservationSnapshot {
            revision: 5,
            state: AgentState::Done,
            authority: AgentAuthority::LifecycleIntegration,
            evidence: "done".into(),
            confidence: 100,
            observed_at_ms: 60,
        };
        completed.attention = Some(AgentAttentionSnapshot {
            reason: AgentAttentionReason::Completed,
            observation: completed.observation.clone(),
        });
        workspace.agents.push(completed);

        let mut schedule_shell = workspace.shells[0].clone();
        schedule_shell.id = "00000000-0000-0000-0000-000000000016".into();
        schedule_shell.owner = ShellOwner::Schedule {
            schedule_id: "00000000-0000-0000-0000-000000000017".into(),
        };
        schedule_shell.run.as_mut().unwrap().id = "00000000-0000-0000-0000-000000000018".into();
        workspace.shells.push(schedule_shell);
        let mut scheduled = template;
        scheduled.id = "00000000-0000-0000-0000-000000000019".into();
        scheduled.name = "scheduled-attention".into();
        scheduled.shell_id = "00000000-0000-0000-0000-000000000016".into();
        scheduled.run_id = "00000000-0000-0000-0000-000000000018".into();
        scheduled.attention = Some(AgentAttentionSnapshot {
            reason: AgentAttentionReason::Blocked,
            observation: AgentObservationSnapshot {
                revision: 6,
                state: AgentState::Blocked,
                authority: AgentAuthority::LifecycleIntegration,
                evidence: "blocked".into(),
                confidence: 100,
                observed_at_ms: 70,
            },
        });
        workspace.agents.push(scheduled);

        let snapshot = project_snapshot(combined, None);
        let names = snapshot
            .agents
            .iter()
            .map(|agent| agent.name.as_str())
            .collect::<Vec<_>>();

        assert_eq!(snapshot.counts.agents, 3);
        assert_eq!(snapshot.counts.attention, 2);
        assert_eq!(snapshot.counts.active, 0);
        assert!(names.contains(&"newest-current"));
        assert!(names.contains(&"completed-attention"));
        assert!(names.contains(&"remote-blocked"));
        assert!(!names.contains(&"local-active"));
        assert!(!names.contains(&"historical-blocked"));
        assert!(!names.contains(&"scheduled-attention"));
    }
}
