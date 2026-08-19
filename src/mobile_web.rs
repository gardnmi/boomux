use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::header::{
    CACHE_CONTROL, CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, HeaderValue, ORIGIN,
    REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, Request, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use boomux::client::{self, Client};
use boomux::protocol::{
    AgentAttentionReason, AgentInstanceSnapshot, AgentState, AttachFrame, CombinedNodeSnapshot,
    NodeProjectionHealthCode, OpenCodeSessionClaimSnapshot, OpenCodeSharedRuntimeSnapshot,
    ProtocolFeature, ShellOwner, ShellStatus, TerminalProfile,
};
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc as async_mpsc;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

const PROJECTION_REFRESH_INTERVAL: Duration = Duration::from_secs(1);
const PROJECTION_STOP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const INDEX_HTML: &str = include_str!("../assets/mobile-web/index.html");
const APP_JS: &str = include_str!("../assets/mobile-web/app.js");
const STYLES_CSS: &str = include_str!("../assets/mobile-web/styles.css");
const MANIFEST: &str = include_str!("../assets/mobile-web/manifest.webmanifest");
const SERVICE_WORKER: &str = include_str!("../assets/mobile-web/service-worker.js");
const ICON: &str = include_str!("../assets/mobile-web/icon.svg");
const ICON_192: &[u8] = include_bytes!("../assets/mobile-web/icon-192.png");
const ICON_512: &[u8] = include_bytes!("../assets/mobile-web/icon-512.png");
const TERMINAL_JS: &str = include_str!("../assets/mobile-web/terminal.js");
const GHOSTTY_WASM: &[u8] = include_bytes!("../assets/mobile-web/ghostty-vt.wasm");
const TERMINAL_GRANT_LIFETIME: Duration = Duration::from_secs(30);
const TERMINAL_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const TERMINAL_MAX_GRANTS: usize = 128;
const TERMINAL_MAX_MESSAGE_BYTES: usize = 64 * 1024;
const TERMINAL_QUEUE_DEPTH: usize = 32;
const TERMINAL_SOCKET_WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const TERMINAL_MAX_CONNECTIONS: usize = 64;
#[derive(Clone)]
struct AppState {
    client: Client,
    presentation: Arc<Mutex<PresentationState>>,
    opencode_web_url: Option<Arc<str>>,
    opencode_runtime_hint: Option<OpenCodeRuntimeHint>,
    terminal_grants: Arc<Mutex<HashMap<String, TerminalGrant>>>,
    terminal_origins: Arc<HashSet<String>>,
    terminal_connections: Arc<Semaphore>,
}

#[derive(Clone)]
struct TerminalGrant {
    shell_id: String,
    run_id: String,
    profile: TerminalProfile,
    expires_at: Instant,
}

#[derive(Clone)]
struct OpenCodeRuntimeHint {
    generation_id: Arc<str>,
    port: u16,
}

struct OpenCodeWebConfiguration {
    public_url: Option<String>,
    runtime_port: Option<u16>,
}

#[derive(Default)]
struct PresentationState {
    snapshot: Option<MobileSnapshot>,
    previous_states: HashMap<(String, String), AgentState>,
    completed_agents: HashSet<(String, String)>,
    baseline_ready: bool,
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
    just_completed: bool,
    #[serde(skip)]
    schedule_owned: bool,
}

#[derive(Debug, Clone, Serialize)]
struct SnapshotCounts {
    agents: usize,
    attention: usize,
    active: usize,
    stale_nodes: usize,
}

#[derive(Debug, Clone, Serialize)]
struct MobileSnapshot {
    generated_at_ms: u64,
    daemon_connected: bool,
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
    native_web: Option<NativeWebHandoff>,
    native_web_notice: String,
    terminal_available: bool,
    terminal_control_available: bool,
    notice: &'static str,
}

#[derive(Debug, Serialize)]
struct NativeWebHandoff {
    integration: &'static str,
    label: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    port: Option<u16>,
    path: String,
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

#[derive(Debug, Serialize)]
struct TerminalGrantResponse {
    websocket_url: String,
    rows: u16,
    cols: u16,
}

#[derive(Default, Deserialize)]
struct TerminalDimensions {
    rows: Option<u16>,
    cols: Option<u16>,
}

#[derive(Deserialize)]
struct TerminalResize {
    #[serde(rename = "type")]
    kind: String,
    rows: u16,
    cols: u16,
    #[serde(default)]
    pixel_width: u16,
    #[serde(default)]
    pixel_height: u16,
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

    fn terminal_unavailable() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "terminal_unavailable",
            message: "Only a current, running, user-owned local Shell can be controlled",
        }
    }

    fn invalid_origin() -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            code: "invalid_origin",
            message: "Terminal control requires a same-origin request",
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

pub(crate) fn run(
    port: u16,
    public_url: Option<&str>,
    opencode_web_url: Option<&str>,
    opencode_web_port: u16,
    no_opencode_web: bool,
) -> Result<(), Box<dyn Error>> {
    let configuration =
        opencode_web_configuration(port, opencode_web_url, opencode_web_port, no_opencode_web)?;
    let terminal_origins = terminal_origins(port, public_url)?;
    let client = client::connect_or_start()?;
    let opencode_runtime = configuration
        .runtime_port
        .map(|port| client.ensure_opencode_shared_runtime(port))
        .transpose()?;
    let mut presentation = PresentationState::default();
    presentation.update(client.combined_node_snapshot(None)?);
    let state = AppState {
        client,
        presentation: Arc::new(Mutex::new(presentation)),
        opencode_web_url: configuration.public_url.map(Arc::from),
        opencode_runtime_hint: opencode_runtime.map(|runtime| OpenCodeRuntimeHint {
            generation_id: Arc::from(runtime.generation_id),
            port: runtime.port,
        }),
        terminal_grants: Arc::new(Mutex::new(HashMap::new())),
        terminal_origins: Arc::new(terminal_origins),
        terminal_connections: Arc::new(Semaphore::new(TERMINAL_MAX_CONNECTIONS)),
    };
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(serve(address, state))
}

fn opencode_web_configuration(
    dashboard_port: u16,
    public_url: Option<&str>,
    runtime_port: u16,
    disabled: bool,
) -> Result<OpenCodeWebConfiguration, Box<dyn Error>> {
    let public_url = public_url.map(normalize_native_web_url).transpose()?;
    if !disabled && dashboard_port == runtime_port {
        return Err("--port and --opencode-web-port must be different".into());
    }
    Ok(OpenCodeWebConfiguration {
        public_url,
        runtime_port: (!disabled).then_some(runtime_port),
    })
}

fn terminal_origins(
    port: u16,
    public_url: Option<&str>,
) -> Result<HashSet<String>, Box<dyn Error>> {
    let mut origins = HashSet::from([
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]);
    if let Some(public_url) = public_url {
        origins.insert(normalize_public_origin(public_url, "--public-url")?);
    }
    Ok(origins)
}

async fn serve(address: SocketAddr, state: AppState) -> Result<(), Box<dyn Error>> {
    let listener = tokio::net::TcpListener::bind(address).await?;
    let stopping = Arc::new(AtomicBool::new(false));
    let worker = spawn_projection_worker(
        state.client.clone(),
        Arc::clone(&state.presentation),
        Arc::clone(&stopping),
    )?;
    println!("Boomux mobile dashboard: http://{address}");
    if let Some(url) = state.opencode_web_url.as_deref() {
        println!("OpenCode shared runtime public handoff: {url}");
    } else if let Some(runtime) = &state.opencode_runtime_hint {
        println!(
            "OpenCode shared runtime handoff: http://127.0.0.1:{}",
            runtime.port
        );
    }
    let app = Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/app.js", get(app_js))
        .route("/terminal.js", get(terminal_js))
        .route("/ghostty-vt.wasm", get(ghostty_wasm))
        .route("/styles.css", get(styles))
        .route("/manifest.webmanifest", get(manifest))
        .route("/service-worker.js", get(service_worker))
        .route("/icon.svg", get(icon))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/api/snapshot", get(snapshot))
        .route("/api/agents/{node_id}/{agent_id}", get(agent_detail))
        .route(
            "/api/agents/{node_id}/{agent_id}/terminal-grant",
            post(terminal_grant),
        )
        .route("/api/terminal/{token}", get(terminal_socket))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            security_headers,
        ))
        .with_state(state);
    let result = axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await;
    stopping.store(true, Ordering::Release);
    worker
        .join()
        .map_err(|_| "mobile projection worker panicked")?;
    result?;
    Ok(())
}

fn spawn_projection_worker(
    client: Client,
    presentation: Arc<Mutex<PresentationState>>,
    stopping: Arc<AtomicBool>,
) -> Result<thread::JoinHandle<()>, Box<dyn Error>> {
    Ok(thread::Builder::new()
        .name("boomux-mobile-projection".into())
        .spawn(move || {
            while !stopping.load(Ordering::Acquire) {
                match client.combined_node_snapshot(None) {
                    Ok(combined) => match presentation.lock() {
                        Ok(mut state) => state.update(combined),
                        Err(_) => return,
                    },
                    Err(error) => match presentation.lock() {
                        Ok(mut state) => {
                            if state.mark_disconnected() {
                                eprintln!(
                                    "boomux: mobile Agent projection refresh failed: {error}"
                                );
                            }
                        }
                        Err(_) => return,
                    },
                }
                let started = std::time::Instant::now();
                while started.elapsed() < PROJECTION_REFRESH_INTERVAL {
                    if stopping.load(Ordering::Acquire) {
                        return;
                    }
                    thread::sleep(PROJECTION_STOP_POLL_INTERVAL);
                }
            }
        })?)
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

async fn security_headers(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; base-uri 'none'; connect-src 'self'; img-src 'self' data:; manifest-src 'self'; object-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; frame-ancestors 'none'",
        ),
    );
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    response
}

fn normalize_native_web_url(value: &str) -> Result<String, Box<dyn Error>> {
    normalize_public_origin(value, "--opencode-web-url")
}

fn normalize_public_origin(value: &str, option: &str) -> Result<String, Box<dyn Error>> {
    let value = value.trim();
    let uri = value
        .parse::<axum::http::Uri>()
        .map_err(|_| format!("{option} must be an absolute HTTP or HTTPS URL"))?;
    let scheme = uri
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"))
        .ok_or_else(|| format!("{option} must use http or https"))?;
    let authority = uri
        .authority()
        .ok_or_else(|| format!("{option} must include a host"))?;
    if authority.as_str().contains('@') {
        return Err(format!("{option} must not include credentials").into());
    }
    if !matches!(uri.path(), "" | "/") || uri.query().is_some() {
        return Err(format!("{option} must not include a path or query").into());
    }
    if scheme == "http" && !matches!(authority.host(), "127.0.0.1" | "localhost" | "::1") {
        return Err(format!("{option} requires https except for a loopback origin").into());
    }
    let host = if authority.host().contains(':') {
        format!("[{}]", authority.host())
    } else {
        authority.host().to_owned()
    };
    let port = authority
        .port_u16()
        .filter(|port| !matches!((scheme, *port), ("http", 80) | ("https", 443)));
    Ok(match port {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

fn opencode_handoff(
    base_url: Option<&str>,
    port: Option<u16>,
    directory: &str,
    session_id: &str,
) -> NativeWebHandoff {
    let path = format!(
        "/{}/session/{}",
        base64_url(directory.as_bytes()),
        percent_encode_path_segment(session_id)
    );
    NativeWebHandoff {
        integration: "opencode",
        label: "Open in OpenCode",
        url: base_url.map(|base_url| format!("{base_url}{path}")),
        port,
        path,
    }
}

fn base64_url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        encoded.push(ALPHABET[(chunk[0] >> 2) as usize] as char);
        encoded.push(
            ALPHABET
                [(((chunk[0] & 0x03) << 4) | (chunk.get(1).copied().unwrap_or(0) >> 4)) as usize]
                as char,
        );
        if let Some(second) = chunk.get(1) {
            encoded.push(
                ALPHABET
                    [(((second & 0x0f) << 2) | (chunk.get(2).copied().unwrap_or(0) >> 6)) as usize]
                    as char,
            );
        }
        if let Some(third) = chunk.get(2) {
            encoded.push(ALPHABET[(third & 0x3f) as usize] as char);
        }
    }
    encoded
}

fn percent_encode_path_segment(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push(HEX[(byte >> 4) as usize] as char);
            encoded.push(HEX[(byte & 0x0f) as usize] as char);
        }
    }
    encoded
}

async fn index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8", "no-cache")
}

async fn terminal_js() -> Response {
    asset(TERMINAL_JS, "text/javascript; charset=utf-8", "no-cache")
}

async fn ghostty_wasm() -> Response {
    binary_asset(GHOSTTY_WASM, "application/wasm", "no-cache")
}

async fn styles() -> Response {
    asset(STYLES_CSS, "text/css; charset=utf-8", "no-cache")
}

async fn manifest() -> Response {
    asset(
        MANIFEST,
        "application/manifest+json; charset=utf-8",
        "no-cache",
    )
}

async fn service_worker() -> Response {
    asset(SERVICE_WORKER, "text/javascript; charset=utf-8", "no-cache")
}

async fn icon() -> Response {
    asset(ICON, "image/svg+xml", "public, max-age=86400")
}

async fn icon_192() -> Response {
    binary_asset(ICON_192, "image/png", "public, max-age=86400")
}

async fn icon_512() -> Response {
    binary_asset(ICON_512, "image/png", "public, max-age=86400")
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

fn binary_asset(body: &'static [u8], content_type: &'static str, cache: &'static str) -> Response {
    (
        [
            (CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (CACHE_CONTROL, HeaderValue::from_static(cache)),
        ],
        body,
    )
        .into_response()
}

async fn snapshot(State(state): State<AppState>) -> Result<Response, ApiError> {
    let snapshot = state
        .presentation
        .lock()
        .map_err(|_| ApiError::daemon())?
        .snapshot
        .clone()
        .ok_or_else(ApiError::daemon)?;
    let mut response = Json(snapshot).into_response();
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
    let opencode_web_url = state.opencode_web_url.clone();
    let opencode_runtime_hint = state.opencode_runtime_hint.clone();
    let mut detail = tokio::task::spawn_blocking(move || {
        let combined = client
            .combined_node_snapshot(None)
            .map_err(|_| ApiError::daemon())?;
        build_agent_detail(
            &client,
            &combined,
            &node_id,
            &agent_id,
            opencode_web_url.as_deref(),
            opencode_runtime_hint.as_ref(),
        )
    })
    .await
    .map_err(|_| ApiError::daemon())??;
    detail.agent.just_completed = state
        .presentation
        .lock()
        .map_err(|_| ApiError::daemon())?
        .completed_agents
        .contains(&(detail.agent.node_id.clone(), detail.agent.agent_id.clone()));
    let mut response = Json(detail).into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn terminal_grant(
    State(state): State<AppState>,
    Path((node_id, agent_id)): Path<(String, String)>,
    Query(dimensions): Query<TerminalDimensions>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    require_same_origin(&headers, &state.terminal_origins)?;
    if Uuid::parse_str(&node_id).is_err() || Uuid::parse_str(&agent_id).is_err() {
        return Err(ApiError::not_found());
    }
    let client = state.client.clone();
    let target = tokio::task::spawn_blocking(move || terminal_target(&client, &node_id, &agent_id))
        .await
        .map_err(|_| ApiError::daemon())??;
    let (shell_id, run_id, source_rows, source_cols) = target;
    let (rows, cols) = requested_terminal_dimensions(dimensions, source_rows, source_cols)?;
    let token = Uuid::new_v4().simple().to_string();
    let expires_at = Instant::now() + TERMINAL_GRANT_LIFETIME;
    let mut grants = state
        .terminal_grants
        .lock()
        .map_err(|_| ApiError::daemon())?;
    grants.retain(|_, grant| {
        grant.expires_at > Instant::now() && (grant.shell_id != shell_id || grant.run_id != run_id)
    });
    if grants.len() >= TERMINAL_MAX_GRANTS {
        return Err(ApiError::terminal_unavailable());
    }
    grants.insert(
        token.clone(),
        TerminalGrant {
            shell_id,
            run_id,
            profile: browser_terminal_profile(rows, cols),
            expires_at,
        },
    );
    let mut response = Json(TerminalGrantResponse {
        websocket_url: format!("/api/terminal/{token}"),
        rows,
        cols,
    })
    .into_response();
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn terminal_socket(
    State(state): State<AppState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    require_same_origin(&headers, &state.terminal_origins)?;
    let grant = state
        .terminal_grants
        .lock()
        .map_err(|_| ApiError::daemon())?
        .remove(&token)
        .filter(|grant| grant.expires_at > Instant::now())
        .ok_or_else(ApiError::not_found)?;
    let permit = Arc::clone(&state.terminal_connections)
        .try_acquire_owned()
        .map_err(|_| ApiError::terminal_unavailable())?;
    let client = state.client.clone();
    let attachment = tokio::task::spawn_blocking(move || {
        validate_terminal_target(&client, &grant.shell_id, &grant.run_id)?;
        client
            .attach_exact_run_from_web(grant.shell_id, grant.run_id, grant.profile)
            .map_err(|_| ApiError::terminal_unavailable())
    })
    .await
    .map_err(|_| ApiError::daemon())??;
    Ok(upgrade
        .max_message_size(TERMINAL_MAX_MESSAGE_BYTES)
        .max_frame_size(TERMINAL_MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| bridge_terminal(socket, attachment, permit))
        .into_response())
}

fn require_same_origin(
    headers: &HeaderMap,
    allowed_origins: &HashSet<String>,
) -> Result<(), ApiError> {
    let host = headers
        .get(HOST)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::invalid_origin)?;
    let origin = headers
        .get(ORIGIN)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<axum::http::Uri>().ok())
        .ok_or_else(ApiError::invalid_origin)?;
    let Some(scheme) = origin
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"))
    else {
        return Err(ApiError::invalid_origin());
    };
    let Some(authority) = origin.authority().map(|value| value.as_str()) else {
        return Err(ApiError::invalid_origin());
    };
    let canonical = format!("{scheme}://{authority}");
    if authority != host || !allowed_origins.contains(&canonical) {
        return Err(ApiError::invalid_origin());
    }
    Ok(())
}

fn terminal_target(
    client: &Client,
    node_id: &str,
    agent_id: &str,
) -> Result<(String, String, u16, u16), ApiError> {
    let combined = client
        .combined_node_snapshot(None)
        .map_err(|_| ApiError::daemon())?;
    let agent = project_visible_agents(&combined)
        .into_iter()
        .find(|agent| agent.node_id == node_id && agent.agent_id == agent_id)
        .ok_or_else(ApiError::not_found)?;
    if !agent.node_local || !agent.node_current || !agent.run_current || agent.schedule_owned {
        return Err(ApiError::terminal_unavailable());
    }
    validate_terminal_target(client, &agent.shell_id, &agent.run_id)?;
    let (_, _, rows, cols) = client
        .read_terminal_reconstruction(&agent.shell_id, &agent.run_id)
        .map_err(|_| ApiError::terminal_unavailable())?;
    Ok((agent.shell_id, agent.run_id, rows, cols))
}

fn validate_terminal_target(client: &Client, shell_id: &str, run_id: &str) -> Result<(), ApiError> {
    let snapshot = client.snapshot().map_err(|_| ApiError::daemon())?;
    let shell = snapshot
        .workspaces
        .iter()
        .flat_map(|workspace| &workspace.shells)
        .find(|shell| shell.id == shell_id)
        .ok_or_else(ApiError::terminal_unavailable)?;
    if shell.owner != ShellOwner::User
        || shell.status != ShellStatus::Running
        || shell.run.as_ref().map(|run| run.id.as_str()) != Some(run_id)
    {
        return Err(ApiError::terminal_unavailable());
    }
    Ok(())
}

fn browser_terminal_profile(rows: u16, cols: u16) -> TerminalProfile {
    TerminalProfile {
        term: Some("xterm-256color".into()),
        colorterm: Some("truecolor".into()),
        term_program: Some("boomux-web".into()),
        term_program_version: Some(env!("CARGO_PKG_VERSION").into()),
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn requested_terminal_dimensions(
    dimensions: TerminalDimensions,
    source_rows: u16,
    source_cols: u16,
) -> Result<(u16, u16), ApiError> {
    let rows = dimensions.rows.unwrap_or(source_rows);
    let cols = dimensions.cols.unwrap_or(source_cols);
    valid_terminal_dimensions(rows, cols)
        .then_some((rows, cols))
        .ok_or_else(ApiError::terminal_unavailable)
}

fn valid_terminal_dimensions(rows: u16, cols: u16) -> bool {
    rows > 0 && cols > 0 && rows <= 1_000 && cols <= 1_000
}

async fn bridge_terminal(
    socket: WebSocket,
    attachment: client::Attachment,
    _permit: OwnedSemaphorePermit,
) {
    let (mut websocket_output, mut websocket_input) = socket.split();
    if !send_terminal_bytes(&mut websocket_output, &attachment.reconstruction).await {
        return;
    }
    if let Some(warning) = &attachment.warning
        && !send_terminal_message(&mut websocket_output, terminal_status("warning", warning)).await
    {
        return;
    }

    let (daemon_output, mut output) = async_mpsc::channel(TERMINAL_QUEUE_DEPTH);
    let (input, daemon_input) = mpsc::sync_channel::<AttachFrame>(TERMINAL_QUEUE_DEPTH);
    let reconnect_input = input.clone();
    let _ = attachment
        .stream
        .set_write_timeout(Some(TERMINAL_SOCKET_WRITE_TIMEOUT));
    let mut read_stream = match attachment.stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let shutdown_stream = match attachment.stream.try_clone() {
        Ok(stream) => stream,
        Err(_) => return,
    };
    let mut write_stream = attachment.stream;
    let reader = thread::spawn(move || {
        loop {
            match AttachFrame::read_from(&mut read_stream) {
                Ok(AttachFrame::Output(bytes)) => {
                    if daemon_output.blocking_send(Ok(bytes)).is_err() {
                        break;
                    }
                }
                Ok(AttachFrame::Reconnect) => {
                    let _ = reconnect_input.send(AttachFrame::ReconnectAck);
                    let _ = daemon_output
                        .blocking_send(Err("The daemon restarted; reconnect the terminal."));
                    break;
                }
                Ok(AttachFrame::Detached) | Err(_) => break,
                Ok(AttachFrame::Suspended) => {
                    let _ = daemon_output
                        .blocking_send(Err("Web terminal control was suspended unexpectedly."));
                    break;
                }
                Ok(_) => {
                    let _ = daemon_output
                        .blocking_send(Err("The daemon sent an invalid terminal frame."));
                    break;
                }
            }
        }
    });
    let writer = thread::spawn(move || {
        for frame in daemon_input {
            let detached = frame == AttachFrame::Detached;
            if frame.write_to(&mut write_stream).is_err() || detached {
                break;
            }
        }
        let _ = write_stream.shutdown(std::net::Shutdown::Both);
    });

    let idle = tokio::time::sleep(TERMINAL_IDLE_TIMEOUT);
    tokio::pin!(idle);
    'bridge: loop {
        tokio::select! {
            () = &mut idle => break,
            frame = output.recv() => match frame {
                Some(Ok(bytes)) => {
                    if !send_terminal_bytes(&mut websocket_output, &bytes).await {
                        break 'bridge;
                    }
                    idle.as_mut().reset(tokio::time::Instant::now() + TERMINAL_IDLE_TIMEOUT);
                }
                Some(Err(message)) => {
                    let _ = send_terminal_message(
                        &mut websocket_output,
                        terminal_status("error", message),
                    ).await;
                    break;
                }
                None => break,
            },
            message = websocket_input.next() => match message {
                Some(Ok(Message::Binary(bytes))) => {
                    if input.try_send(AttachFrame::Input(bytes.to_vec())).is_err() {
                        break;
                    }
                    idle.as_mut().reset(tokio::time::Instant::now() + TERMINAL_IDLE_TIMEOUT);
                }
                Some(Ok(Message::Text(text))) => {
                    if let Ok(resize) = serde_json::from_str::<TerminalResize>(&text)
                        && resize.kind == "resize"
                        && valid_terminal_dimensions(resize.rows, resize.cols)
                        && input.try_send(AttachFrame::Resize {
                            rows: resize.rows,
                            cols: resize.cols,
                            pixel_width: resize.pixel_width,
                            pixel_height: resize.pixel_height,
                        }).is_err()
                    {
                        break;
                    }
                    idle.as_mut().reset(tokio::time::Instant::now() + TERMINAL_IDLE_TIMEOUT);
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            }
        }
    }
    let _ = input.try_send(AttachFrame::Detached);
    let _ = shutdown_stream.shutdown(std::net::Shutdown::Both);
    drop(input);
    drop(output);
    let _ = tokio::task::spawn_blocking(move || {
        let _ = writer.join();
        let _ = reader.join();
    })
    .await;
}

async fn send_terminal_message(
    output: &mut SplitSink<WebSocket, Message>,
    message: Message,
) -> bool {
    tokio::time::timeout(TERMINAL_SOCKET_WRITE_TIMEOUT, output.send(message))
        .await
        .is_ok_and(|result| result.is_ok())
}

async fn send_terminal_bytes(output: &mut SplitSink<WebSocket, Message>, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return send_terminal_message(output, Message::Binary(Vec::new().into())).await;
    }
    for chunk in bytes.chunks(TERMINAL_MAX_MESSAGE_BYTES) {
        if !send_terminal_message(output, Message::Binary(chunk.to_vec().into())).await {
            return false;
        }
    }
    true
}

fn terminal_status(kind: &str, message: &str) -> Message {
    Message::Text(
        serde_json::json!({ "type": kind, "message": message })
            .to_string()
            .into(),
    )
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
        daemon_connected: true,
        viewer,
        counts,
        agents,
    }
}

impl PresentationState {
    fn update(&mut self, combined: CombinedNodeSnapshot) {
        let mut snapshot = project_snapshot(combined, None);
        let mut next_states = HashMap::new();
        let mut next_completed = HashSet::new();
        for agent in &mut snapshot.agents {
            let key = (agent.node_id.clone(), agent.agent_id.clone());
            next_states.insert(key.clone(), agent.state);
            let retained = self.completed_agents.contains(&key)
                && agent.node_local
                && agent.run_current
                && agent.state == AgentState::Idle;
            let transitioned = self.baseline_ready
                && agent.node_local
                && agent.run_current
                && self.previous_states.get(&key) == Some(&AgentState::Working)
                && agent.state == AgentState::Idle;
            agent.just_completed = retained || transitioned;
            if agent.just_completed {
                next_completed.insert(key);
            }
        }
        snapshot.agents.sort_by(|left, right| {
            agent_priority(left)
                .cmp(&agent_priority(right))
                .then_with(|| right.observed_at_ms.cmp(&left.observed_at_ms))
                .then_with(|| left.name.cmp(&right.name))
                .then_with(|| left.node_id.cmp(&right.node_id))
        });
        self.previous_states = next_states;
        self.completed_agents = next_completed;
        self.baseline_ready = true;
        self.snapshot = Some(snapshot);
    }

    fn mark_disconnected(&mut self) -> bool {
        let was_connected = self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.daemon_connected);
        if let Some(snapshot) = &mut self.snapshot {
            snapshot.daemon_connected = false;
        }
        was_connected
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
                        just_completed: false,
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
                    just_completed: false,
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
    opencode_web_url: Option<&str>,
    opencode_runtime_hint: Option<&OpenCodeRuntimeHint>,
) -> Result<AgentDetail, ApiError> {
    // Runtime state is deliberately fetched for every detail. The startup value
    // is only a generation hint and cannot authorize a stale browser link.
    let current_opencode_runtime = client.get_opencode_shared_runtime().ok().flatten();
    let agent = project_visible_agents(combined)
        .into_iter()
        .find(|agent| agent.node_id == node_id && agent.agent_id == agent_id)
        .ok_or_else(ApiError::not_found)?;
    let mut timeline = Vec::new();
    let local_agent = combined
        .nodes
        .iter()
        .find(|node| node.node_id == agent.node_id)
        .and_then(|node| node.local_snapshot.as_ref())
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.agents)
                .find(|candidate| candidate.id == agent_id)
        })
        .cloned();
    let evidence = local_agent
        .as_ref()
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

    let (native_web, native_web_notice) = if !agent.node_local {
        (
            None,
            "Native Session links remain on the owning Node and are not stored in remote projections."
                .into(),
        )
    } else if agent.integration != "opencode" {
        (
            None,
            format!(
                "Native web handoff is not configured for {} Agents.",
                agent.integration
            ),
        )
    } else if let Some(runtime_hint) = opencode_runtime_hint {
        if let Some((directory, external_session_id)) = local_agent
            .as_ref()
            .and_then(|agent| Some((agent.cwd.as_deref()?, agent.external_session_id.as_deref()?)))
        {
            let Some(runtime) = current_opencode_runtime.as_ref().filter(|runtime| {
                runtime.generation_id.as_str() == runtime_hint.generation_id.as_ref()
                    && runtime.port == runtime_hint.port
            }) else {
                return Ok(agent_detail_without_native_web(
                    client,
                    combined,
                    agent,
                    timeline,
                    "The daemon-owned OpenCode shared runtime is unavailable or has been replaced.",
                ));
            };
            let claim = client
                .resolve_opencode_session_claim(runtime.generation_id.clone(), external_session_id);
            if !claim.is_ok_and(|(claim, resolved_agent)| {
                opencode_claim_matches_agent(
                    runtime_hint,
                    runtime,
                    external_session_id,
                    Some((&claim, &resolved_agent)),
                    &agent,
                )
            }) {
                (
                    None,
                    "This Session is not claimed by the shared runtime. The desktop TUI must be running through a Boomux-managed bare `opencode` launch."
                        .into(),
                )
            } else {
                match directory.to_str() {
                Some(directory) => (
                    Some(opencode_handoff(
                        opencode_web_url,
                        opencode_web_url.is_none().then_some(runtime.port),
                        directory,
                        external_session_id,
                    )),
                    "Open this Session to continue chatting, review tool activity, and respond to OpenCode prompts."
                        .into(),
                ),
                None => (
                    None,
                    "This OpenCode Session's working directory cannot be represented in a browser URL."
                        .into(),
                ),
                }
            }
        } else {
            (
                None,
                "This Agent has no retained canonical OpenCode Session identity and working directory."
                    .into(),
            )
        }
    } else {
        (
            None,
            "Native OpenCode Session handoff is disabled for this dashboard.".into(),
        )
    };

    Ok(agent_detail_with_terminal(
        client,
        combined,
        agent,
        timeline,
        native_web,
        native_web_notice,
    ))
}

fn opencode_claim_matches_agent(
    runtime_hint: &OpenCodeRuntimeHint,
    runtime: &OpenCodeSharedRuntimeSnapshot,
    root_session_id: &str,
    resolved: Option<(&OpenCodeSessionClaimSnapshot, &AgentInstanceSnapshot)>,
    agent: &AgentCard,
) -> bool {
    let Some((claim, resolved_agent)) = resolved else {
        return false;
    };
    runtime.generation_id == runtime_hint.generation_id.as_ref()
        && runtime.port == runtime_hint.port
        && claim.generation_id == runtime.generation_id
        && claim.root_session_id == root_session_id
        && claim.agent_id == agent.agent_id
        && claim.shell_id == agent.shell_id
        && claim.run_id == agent.run_id
        && resolved_agent.id == agent.agent_id
        && resolved_agent.shell_id == agent.shell_id
        && resolved_agent.run_id == agent.run_id
}

fn agent_detail_without_native_web(
    client: &Client,
    combined: &CombinedNodeSnapshot,
    agent: AgentCard,
    timeline: Vec<TimelineEntry>,
    native_web_notice: &str,
) -> AgentDetail {
    agent_detail_with_terminal(
        client,
        combined,
        agent,
        timeline,
        None,
        native_web_notice.into(),
    )
}

fn agent_detail_with_terminal(
    client: &Client,
    combined: &CombinedNodeSnapshot,
    agent: AgentCard,
    timeline: Vec<TimelineEntry>,
    native_web: Option<NativeWebHandoff>,
    native_web_notice: String,
) -> AgentDetail {
    let current_shell = combined
        .nodes
        .iter()
        .find(|node| node.node_id == agent.node_id)
        .and_then(|node| node.local_snapshot.as_ref())
        .and_then(|snapshot| {
            snapshot
                .workspaces
                .iter()
                .flat_map(|workspace| &workspace.shells)
                .find(|shell| shell.id == agent.shell_id)
        });
    let current_run = current_shell
        .and_then(|shell| shell.run.as_ref())
        .is_some_and(|run| run.id == agent.run_id);
    let terminal_control_available = agent.node_local
        && agent.node_current
        && agent.run_current
        && !agent.schedule_owned
        && current_shell.is_some_and(|shell| {
            shell.owner == ShellOwner::User && shell.status == ShellStatus::Running
        })
        && current_run
        && client
            .supports(ProtocolFeature::ReversibleAttachmentTakeover)
            .unwrap_or(false);
    let terminal_available = terminal_control_available;
    let notice = if terminal_control_available {
        "Terminal output remains hidden until this browser takes exclusive control."
    } else if current_run {
        "This Shell run is not eligible for web terminal control."
    } else if agent.node_local {
        "The Agent's exact Shell run is no longer current, so Boomux will not substitute output from a later run."
    } else {
        "Remote cached projections do not contain terminal output; the owning Node remains authoritative."
    };
    AgentDetail {
        agent,
        timeline,
        native_web,
        native_web_notice,
        terminal_available,
        terminal_control_available,
        notice,
    }
}

fn agent_priority(agent: &AgentCard) -> u8 {
    if agent.just_completed {
        return 1;
    }
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
    fn app_shell_is_installable_and_has_no_inline_code() {
        assert!(INDEX_HTML.contains("rel=\"manifest\""));
        assert!(INDEX_HTML.contains("type=\"module\" src=\"app.js\""));
        assert!(!INDEX_HTML.contains("<script>"));
        assert!(MANIFEST.contains("\"display\": \"standalone\""));
        assert!(SERVICE_WORKER.contains("/api/"));
        assert!(INDEX_HTML.contains("id=\"native-handoff-link\""));
        assert!(APP_JS.contains("payload.native_web"));
        assert!(APP_JS.contains("if (routeDetail()) requests.push(refreshDetail())"));
        assert!(APP_JS.contains("clearNativeHandoff();"));
        assert!(INDEX_HTML.contains("id=\"take-terminal-control\""));
        assert!(!INDEX_HTML.contains("id=\"terminal-output\""));
        assert!(APP_JS.contains("terminal_control_available"));
        assert!(!APP_JS.contains("payload.terminal_reconstruction"));
        assert!(!APP_JS.contains("decodeBase64Url"));
        assert!(APP_JS.contains("new WebSocket(url)"));
        assert!(APP_JS.contains("renderInactiveTerminal(view)"));
        assert!(TERMINAL_JS.contains("setWritable"));
        assert!(TERMINAL_JS.contains("ghostty-vt.wasm"));
        assert!(TERMINAL_JS.contains("ResizeObserver"));
        assert!(TERMINAL_JS.contains("addEventListener(\"wheel\""));
        assert!(TERMINAL_JS.contains("proposeDimensions"));
        assert!(TERMINAL_JS.contains("\\x1B[5~"));
        assert!(TERMINAL_JS.contains("\\x1B[6~"));
        assert!(SERVICE_WORKER.contains("./ghostty-vt.wasm"));
        assert!(
            APP_JS
                .matches("nativeLink.removeAttribute(\"href\");")
                .count()
                >= 2
        );
    }

    #[test]
    fn terminal_control_requires_an_exact_same_origin() {
        let allowed = HashSet::from(["https://desktop.example:3737".into()]);
        let mut headers = HeaderMap::new();
        headers.insert(HOST, HeaderValue::from_static("desktop.example:3737"));
        headers.insert(
            ORIGIN,
            HeaderValue::from_static("https://desktop.example:3737"),
        );
        assert!(require_same_origin(&headers, &allowed).is_ok());

        headers.insert(ORIGIN, HeaderValue::from_static("https://attacker.example"));
        assert!(require_same_origin(&headers, &allowed).is_err());
        headers.insert(HOST, HeaderValue::from_static("attacker.example"));
        assert!(require_same_origin(&headers, &allowed).is_err());
        headers.remove(ORIGIN);
        assert!(require_same_origin(&headers, &allowed).is_err());
    }

    #[test]
    fn terminal_public_origin_is_explicit_and_https() {
        let origins = terminal_origins(3737, Some("https://desktop.example")).unwrap();
        assert!(origins.contains("http://127.0.0.1:3737"));
        assert!(origins.contains("http://localhost:3737"));
        assert!(origins.contains("https://desktop.example"));
        assert_eq!(
            normalize_public_origin("https://desktop.example:443", "--public-url").unwrap(),
            "https://desktop.example"
        );
        assert!(terminal_origins(3737, Some("http://desktop.example")).is_err());
    }

    #[test]
    fn browser_terminal_profile_does_not_copy_server_environment() {
        let profile = browser_terminal_profile(37, 121);
        assert_eq!(profile.term.as_deref(), Some("xterm-256color"));
        assert_eq!(profile.colorterm.as_deref(), Some("truecolor"));
        assert_eq!(profile.rows, 37);
        assert_eq!(profile.cols, 121);
        assert_eq!(profile.pixel_width, 0);
        assert_eq!(profile.pixel_height, 0);
    }

    #[test]
    fn browser_terminal_dimensions_use_fitted_grid_with_bounded_fallback() {
        assert_eq!(
            requested_terminal_dimensions(
                TerminalDimensions {
                    rows: Some(42),
                    cols: Some(132),
                },
                24,
                80,
            )
            .ok(),
            Some((42, 132))
        );
        assert_eq!(
            requested_terminal_dimensions(TerminalDimensions::default(), 24, 80).ok(),
            Some((24, 80))
        );
        assert!(
            requested_terminal_dimensions(
                TerminalDimensions {
                    rows: Some(1_001),
                    cols: Some(80),
                },
                24,
                80,
            )
            .is_err()
        );
    }

    #[test]
    fn mobile_web_has_no_process_ownership() {
        let implementation = include_str!("mobile_web.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        assert!(!implementation.contains("std::process"));
        assert!(!implementation.contains("Command::new"));
    }

    #[test]
    fn native_web_url_requires_an_origin_without_a_path() {
        assert_eq!(
            normalize_native_web_url(" https://desktop.example.ts.net:4097/ ").unwrap(),
            "https://desktop.example.ts.net:4097"
        );
        assert!(normalize_native_web_url("desktop.example.ts.net:4097").is_err());
        assert!(normalize_native_web_url("ftp://desktop.example.ts.net").is_err());
        assert!(normalize_native_web_url("http://desktop.example.ts.net:4097").is_err());
        assert!(normalize_native_web_url("http://127.0.0.1:4097").is_ok());
        assert!(normalize_native_web_url("https://desktop.example.ts.net/opencode").is_err());
        assert!(normalize_native_web_url("https://desktop.example.ts.net/?token=secret").is_err());
        assert!(normalize_native_web_url("https://user:password@desktop.example.ts.net").is_err());
    }

    #[test]
    fn native_web_configuration_always_ensures_unless_disabled() {
        let default = opencode_web_configuration(3737, None, 4097, false).unwrap();
        assert_eq!(default.public_url, None);
        assert_eq!(default.runtime_port, Some(4097));

        let public = opencode_web_configuration(
            3737,
            Some("https://desktop.example.ts.net:4097"),
            4097,
            false,
        )
        .unwrap();
        assert_eq!(
            public.public_url.as_deref(),
            Some("https://desktop.example.ts.net:4097")
        );
        assert_eq!(public.runtime_port, Some(4097));

        let disabled = opencode_web_configuration(4097, None, 4097, true).unwrap();
        assert_eq!(disabled.runtime_port, None);
        assert!(opencode_web_configuration(4097, None, 4097, false).is_err());
    }

    #[test]
    fn opencode_handoff_targets_the_exact_directory_and_session() {
        let handoff = opencode_handoff(
            Some("https://desktop.example.ts.net:4097"),
            None,
            "/home/user/My Project",
            "session/with spaces",
        );

        assert_eq!(handoff.integration, "opencode");
        assert_eq!(handoff.label, "Open in OpenCode");
        assert_eq!(
            handoff.url,
            Some("https://desktop.example.ts.net:4097/L2hvbWUvdXNlci9NeSBQcm9qZWN0/session/session%2Fwith%20spaces".into())
        );
        assert_eq!(handoff.port, None);
        assert_eq!(
            handoff.path,
            "/L2hvbWUvdXNlci9NeSBQcm9qZWN0/session/session%2Fwith%20spaces"
        );

        let managed = opencode_handoff(None, Some(4097), "/work", "session-local");
        assert_eq!(managed.url, None);
        assert_eq!(managed.port, Some(4097));
        assert_eq!(managed.path, "/L3dvcms/session/session-local");
    }

    #[test]
    fn opencode_handoff_requires_exact_generation_and_claim_identity() {
        let combined = combined_snapshot();
        let agent = project_visible_agents(&combined)
            .into_iter()
            .find(|agent| agent.node_local)
            .unwrap();
        let resolved_agent = combined.nodes[0]
            .local_snapshot
            .as_ref()
            .unwrap()
            .workspaces[0]
            .agents[0]
            .clone();
        let hint = OpenCodeRuntimeHint {
            generation_id: Arc::from("generation-current"),
            port: 4097,
        };
        let runtime = OpenCodeSharedRuntimeSnapshot {
            generation_id: "generation-current".into(),
            url: "http://127.0.0.1:4097".into(),
            port: 4097,
            pid: Some(42),
        };
        let claim = OpenCodeSessionClaimSnapshot {
            generation_id: runtime.generation_id.clone(),
            claim_id: "claim-current".into(),
            holder_id: "holder-current".into(),
            root_session_id: "session-local".into(),
            workspace_id: agent.workspace_id.clone(),
            shell_id: agent.shell_id.clone(),
            run_id: agent.run_id.clone(),
            agent_id: agent.agent_id.clone(),
            holder_count: 1,
            holder_expires_at_ms: 100,
        };

        assert!(opencode_claim_matches_agent(
            &hint,
            &runtime,
            "session-local",
            Some((&claim, &resolved_agent)),
            &agent,
        ));
        assert!(!opencode_claim_matches_agent(
            &hint,
            &runtime,
            "session-local",
            None,
            &agent,
        ));

        let mut stale_runtime = runtime.clone();
        stale_runtime.generation_id = "generation-replaced".into();
        assert!(!opencode_claim_matches_agent(
            &hint,
            &stale_runtime,
            "session-local",
            Some((&claim, &resolved_agent)),
            &agent,
        ));

        for mismatch in ["agent", "shell", "run", "root"] {
            let mut mismatched_claim = claim.clone();
            match mismatch {
                "agent" => mismatched_claim.agent_id = "different-agent".into(),
                "shell" => mismatched_claim.shell_id = "different-shell".into(),
                "run" => mismatched_claim.run_id = "different-run".into(),
                "root" => mismatched_claim.root_session_id = "different-root".into(),
                _ => unreachable!(),
            }
            assert!(!opencode_claim_matches_agent(
                &hint,
                &runtime,
                "session-local",
                Some((&mismatched_claim, &resolved_agent)),
                &agent,
            ));
        }

        let mut mismatched_agent = resolved_agent;
        mismatched_agent.run_id = "different-run".into();
        assert!(!opencode_claim_matches_agent(
            &hint,
            &runtime,
            "session-local",
            Some((&claim, &mismatched_agent)),
            &agent,
        ));
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

    #[test]
    fn presentation_tracks_local_completion_only_after_working_baseline() {
        let mut presentation = PresentationState::default();
        presentation.update(combined_snapshot());
        assert!(
            presentation
                .snapshot
                .as_ref()
                .unwrap()
                .agents
                .iter()
                .all(|agent| !agent.just_completed)
        );

        let mut idle = combined_snapshot();
        let agent = &mut idle.nodes[0].local_snapshot.as_mut().unwrap().workspaces[0].agents[0];
        agent.observation.state = AgentState::Idle;
        agent.observation.revision += 1;
        agent.observation.observed_at_ms += 10;

        let mut idle_baseline = PresentationState::default();
        idle_baseline.update(idle.clone());
        assert!(
            idle_baseline
                .snapshot
                .as_ref()
                .unwrap()
                .agents
                .iter()
                .all(|agent| !agent.just_completed)
        );

        presentation.update(idle.clone());
        let completed = presentation
            .snapshot
            .as_ref()
            .unwrap()
            .agents
            .iter()
            .find(|agent| agent.name == "local-active")
            .unwrap();
        assert!(completed.just_completed);

        presentation.update(idle);
        assert!(
            presentation
                .snapshot
                .as_ref()
                .unwrap()
                .agents
                .iter()
                .find(|agent| agent.name == "local-active")
                .unwrap()
                .just_completed
        );

        let mut resumed = combined_snapshot();
        resumed.nodes[0].local_snapshot.as_mut().unwrap().workspaces[0].agents[0]
            .observation
            .observed_at_ms += 20;
        presentation.update(resumed);
        assert!(presentation.completed_agents.is_empty());
    }

    #[test]
    fn presentation_retains_snapshot_across_disconnect() {
        let mut presentation = PresentationState::default();
        presentation.update(combined_snapshot());

        assert!(presentation.mark_disconnected());
        let snapshot = presentation.snapshot.as_ref().unwrap();
        assert!(!snapshot.daemon_connected);
        assert_eq!(snapshot.counts.agents, 2);
        assert!(!presentation.mark_disconnected());

        presentation.update(combined_snapshot());
        assert!(presentation.snapshot.as_ref().unwrap().daemon_connected);
    }
}
