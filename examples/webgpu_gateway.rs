//! Local-only browser client for the existing Boomux daemon. Run with an
//! explicitly selected Boomux runtime; this executable never starts a daemon.
#[path = "../poc/webgpu-tiling/daemon_bridge.rs"]
mod daemon_bridge;
// Reuse Desktop's qualified resource IDs and owner-routed operations.
#[allow(dead_code)]
#[path = "../desktop/src/remote.rs"]
mod remote;

use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Request, State, WebSocketUpgrade},
    http::{HeaderMap, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use boomux::{
    client::{self, Client},
    protocol::{ProtocolFeature, ShellSpec, TerminalProfile},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;
#[derive(Clone)]
struct App {
    client: Client,
    node_id: String,
    workspace_id: String,
    root: PathBuf,
    host: String,
    origin: String,
    grants: Arc<Mutex<HashMap<String, (Instant, daemon_bridge::Grant)>>>,
    attachments: Arc<Semaphore>,
    operations: Arc<Semaphore>,
}
fn fail(status: StatusCode, message: impl ToString) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message.to_string()})))
}
async fn operation<F>(app: App, f: F) -> ApiResult
where
    F: FnOnce(&App) -> Result<Value, String> + Send + 'static,
{
    let _permit = app
        .operations
        .clone()
        .try_acquire_owned()
        .map_err(|_| fail(StatusCode::TOO_MANY_REQUESTS, "Gateway busy"))?;
    tokio::task::spawn_blocking(move || {
        let _permit = _permit;
        if app.client.node_identity().map_err(|e| e.to_string())? != app.node_id {
            return Err("Node identity changed; reconnect the gateway explicitly".into());
        }
        f(&app)
    })
    .await
    .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
    .map(Json)
    .map_err(|e| fail(StatusCode::CONFLICT, e))
}
async fn guard(State(app): State<App>, req: Request, next: Next) -> Response {
    if req.headers().get("host").and_then(|v| v.to_str().ok()) != Some(&app.host) {
        return fail(StatusCode::FORBIDDEN, "Invalid Host").into_response();
    }
    if (req.method() != "GET" || req.uri().path() == "/pty")
        && req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(&app.origin)
    {
        return fail(StatusCode::FORBIDDEN, "Invalid Origin").into_response();
    }
    let mut response = next.run(req).await;
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
        .headers_mut()
        .insert("x-content-type-options", "nosniff".parse().unwrap());
    response.headers_mut().insert(
        "cross-origin-resource-policy",
        "same-origin".parse().unwrap(),
    );
    response.headers_mut().insert("content-security-policy", "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; font-src 'self'; connect-src 'self'; img-src 'self' data:; object-src 'none'; frame-ancestors 'none'".parse().unwrap());
    response
}
fn remote_workspaces(node: &boomux::protocol::CombinedNode) -> Vec<Value> {
    let Some(projection) = &node.remote_projection else {
        return vec![];
    };
    let mut shells = HashMap::<&str, Vec<Value>>::new();
    let mut agents = HashMap::<&str, Vec<Value>>::new();
    for shell in &projection.shells {
        shells.entry(&shell.workspace_id).or_default().push(json!({
            "id":remote::key(&node.node_id,&shell.id),
            "workspace_id":remote::key(&node.node_id,&shell.workspace_id),
            "name":shell.name,"cwd":null,
            "status":shell.status,"run":shell.run_id.as_ref().map(|id|json!({"id":id})),
        }));
    }
    for agent in &projection.agents {
        agents.entry(&agent.workspace_id).or_default().push(json!({
            "id":remote::key(&node.node_id,&agent.id),
            "shell_id":remote::key(&node.node_id,&agent.shell_id),
            "run_id":agent.run_id,"observation":{"state":agent.state},"attention":agent.attention,
        }));
    }
    projection
        .workspaces
        .iter()
        .map(|workspace| {
            json!({
                "id":remote::key(&node.node_id,&workspace.id),"name":workspace.name,
                "remote":{"node_id":node.node_id,"alias":node.alias,"health":node.health,
                    "stale":node.stale,"current":node.current},
                "shells":shells.remove(workspace.id.as_str()).unwrap_or_default(),
                "agents":agents.remove(workspace.id.as_str()).unwrap_or_default(),
            })
        })
        .collect()
}
async fn snapshot(State(app): State<App>) -> ApiResult {
    operation(app, |app| {
        let mut snapshot = serde_json::to_value(app.client.snapshot().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        let warning = match app.client.combined_node_snapshot(None) {
            Ok(combined) => {
                let workspaces = snapshot["workspaces"]
                    .as_array_mut()
                    .ok_or("Invalid local snapshot")?;
                for node in combined.nodes.iter().filter(|node| !node.local) {
                    workspaces.extend(remote_workspaces(node));
                }
                None
            }
            Err(error) => Some(format!("Remote discovery unavailable: {error}")),
        };
        Ok(
            json!({"mode":"daemon","node_id":app.node_id,"workspace_id":app.workspace_id,
            "snapshot":snapshot,"warning":warning}),
        )
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateShell {
    node_id: String,
    workspace_id: String,
}
fn profile(rows: u16, cols: u16) -> TerminalProfile {
    TerminalProfile {
        rows,
        cols,
        term: Some("xterm-256color".into()),
        colorterm: Some("truecolor".into()),
        term_program: Some("boomux-webgpu-poc".into()),
        term_program_version: Some(env!("CARGO_PKG_VERSION").into()),
        pixel_width: 0,
        pixel_height: 0,
    }
}
fn shell_spec(root: &std::path::Path, cwd: PathBuf) -> ShellSpec {
    ShellSpec {
        name: format!("web-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        command: vec![
            "bash".into(),
            "--noprofile".into(),
            "--rcfile".into(),
            root.join("poc/webgpu-tiling/shell.bash")
                .to_string_lossy()
                .into_owned(),
            "-i".into(),
        ],
        cwd,
    }
}
async fn create_shell(State(app): State<App>, Json(request): Json<CreateShell>) -> ApiResult {
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        if remote::identity(&request.workspace_id).is_some() {
            let workspace =
                remote::workspace(&app.client, &request.workspace_id).map_err(|e| e.to_string())?;
            if workspace.shells.len() >= 64 {
                return Err("PoC limit: 64 Shells per Workspace".into());
            }
            let shell = remote::create_shell(&app.client, &request.workspace_id)?;
            let identity = remote::identity(&shell.id).ok_or("Remote Shell identity missing")?;
            // This is the newly created Shell, never an existing user run.
            drop(
                app.client
                    .attach_node(
                        identity.clone(),
                        false,
                        false,
                        shell.run.as_ref().map(|r| r.id.clone()),
                        profile(24, 80),
                    )
                    .map_err(|e| e.to_string())?,
            );
            let mut shell = remote::shell(&app.client, &shell.id).map_err(|e| e.to_string())?;
            remote::qualify_shell(&identity.node_id, &mut shell);
            return Ok(json!({"node_id":app.node_id,"shell":shell}));
        }
        let workspace = app
            .client
            .get_workspace(&request.workspace_id)
            .map_err(|e| e.to_string())?;
        if workspace.shells.len() >= 64 {
            return Err("PoC limit: 64 Shells per Workspace".into());
        }
        let shell = app
            .client
            .create_started_shell(
                &workspace.id,
                shell_spec(
                    &app.root,
                    workspace.default_cwd.unwrap_or_else(|| app.root.clone()),
                ),
                profile(24, 80),
            )
            .map_err(|e| e.to_string())?;
        Ok(json!({"node_id":app.node_id,"shell":shell}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Attach {
    node_id: String,
    shell_id: String,
    run_id: String,
    rows: u16,
    cols: u16,
    #[serde(default)]
    takeover: bool,
}
async fn grant(State(app): State<App>, Json(request): Json<Attach>) -> ApiResult {
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        if !(1..=200).contains(&request.rows) || !(2..=500).contains(&request.cols) {
            return Err("Invalid grid dimensions".into());
        }
        let shell = remote::shell(&app.client, &request.shell_id).map_err(|e| e.to_string())?;
        if shell.run.as_ref().map(|r| r.id.as_str()) != Some(&request.run_id) {
            return Err("ShellRun changed; select its current run explicitly".into());
        }
        let mut grants = app.grants.lock().map_err(|_| "Grant lock failed")?;
        grants.retain(|_, (expires, _)| *expires > Instant::now());
        if grants.len() >= 64 {
            return Err("Too many pending attachments".into());
        }
        let token = uuid::Uuid::new_v4().to_string();
        grants.insert(
            token.clone(),
            (
                Instant::now() + Duration::from_secs(30),
                daemon_bridge::Grant {
                    node_id: app.node_id.clone(),
                    shell_id: request.shell_id,
                    run_id: request.run_id,
                    profile: profile(request.rows, request.cols),
                    takeover: request.takeover,
                },
            ),
        );
        Ok(json!({"token":token}))
    })
    .await
}
async fn terminal(State(app): State<App>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    let Some(token) = headers
        .get("sec-websocket-protocol")
        .and_then(|h| h.to_str().ok())
        .and_then(|h| h.split(',').map(str::trim).find(|s| *s != "boomux-poc"))
    else {
        return fail(StatusCode::FORBIDDEN, "Missing attachment grant").into_response();
    };
    let grant = app
        .grants
        .lock()
        .ok()
        .and_then(|mut grants| grants.remove(token))
        .filter(|(expiry, _)| *expiry > Instant::now())
        .map(|(_, grant)| grant);
    let Some(grant) = grant else {
        return fail(StatusCode::FORBIDDEN, "Expired or consumed grant").into_response();
    };
    let Ok(permit) = app.attachments.clone().try_acquire_owned() else {
        return fail(StatusCode::TOO_MANY_REQUESTS, "Attachment limit").into_response();
    };
    ws.protocols(["boomux-poc"])
        .max_message_size(65536)
        .max_frame_size(65536)
        .max_write_buffer_size(2 * 1024 * 1024)
        .on_upgrade(move |socket| async move {
            let _permit = permit;
            daemon_bridge::run(socket, app.client, grant).await
        })
}
async fn asset(State(app): State<App>, uri: Uri) -> Response {
    let (path, mime) = match uri.path() {
        "/" | "/index.html" => ("poc/webgpu-tiling/index.html", "text/html"),
        "/app.js" => ("poc/webgpu-tiling/app.js", "text/javascript"),
        "/terminal.js" => ("poc/webgpu-tiling/terminal.js", "text/javascript"),
        "/renderer.js" => ("poc/webgpu-tiling/renderer.js", "text/javascript"),
        "/layout.js" => ("poc/webgpu-tiling/layout.js", "text/javascript"),
        "/style.css" => ("poc/webgpu-tiling/style.css", "text/css"),
        "/vendor/ghostty-web.js" => (
            "node_modules/ghostty-web/dist/ghostty-web.js",
            "text/javascript",
        ),
        "/vendor/ghostty-vt.wasm" => (
            "node_modules/ghostty-web/ghostty-vt.wasm",
            "application/wasm",
        ),
        "/vendor/jetbrains-mono.woff2" => (
            "node_modules/@fontsource/jetbrains-mono/files/jetbrains-mono-latin-400-normal.woff2",
            "font/woff2",
        ),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    match tokio::task::spawn_blocking(move || std::fs::read(app.root.join(path))).await {
        Ok(Ok(bytes)) => ([("content-type", mime)], bytes).into_response(),
        _ => fail(
            StatusCode::NOT_FOUND,
            "Asset missing; run bun install --frozen-lockfile",
        )
        .into_response(),
    }
}
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let client = client::connect_if_running()?
        .ok_or("Start the selected Boomux daemon before running the gateway")?;
    if !client.supports(ProtocolFeature::CreateStartedShell)? {
        return Err("This PoC requires a daemon with CreateStartedShell support".into());
    }
    let node_id = client.node_identity()?;
    let workspace_id = if let Ok(id) = std::env::var("POC_WORKSPACE_ID") {
        client.get_workspace(&id)?.id
    } else {
        // Only retain our exact Workspace identity; names never imply adoption.
        let manifest = root
            .join("target/webgpu-poc")
            .join(format!("{node_id}.json"));
        let stored = match std::fs::read(&manifest) {
            Ok(bytes) => Some(serde_json::from_slice::<String>(&bytes)?),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        match stored {
            Some(id) => client.get_workspace(&id)?.id,
            None => {
                // Create with a pending Shell atomically: Desktop may remove
                // empty Workspaces, so do not publish an empty one first.
                let workspace = client.create_workspace_with_default_cwd(
                    "WebGPU playground",
                    Some(root.clone()),
                    vec![shell_spec(&root, root.clone())],
                )?;
                std::fs::create_dir_all(manifest.parent().unwrap())?;
                let temporary = manifest.with_extension("tmp");
                std::fs::write(&temporary, serde_json::to_vec(&workspace.id)?)?;
                std::fs::rename(&temporary, &manifest)?;
                drop(client.attach(&workspace.shells[0].id, false, profile(24, 80))?);
                workspace.id
            }
        }
    };
    let port = std::env::var("POC_PORT")
        .unwrap_or_else(|_| "4389".into())
        .parse::<u16>()?;
    let host = format!("127.0.0.1:{port}");
    let origin = format!("http://{host}");
    let app = App {
        client,
        node_id,
        workspace_id,
        root,
        host: host.clone(),
        origin: origin.clone(),
        grants: Arc::new(Mutex::new(HashMap::new())),
        attachments: Arc::new(Semaphore::new(24)),
        operations: Arc::new(Semaphore::new(8)),
    };
    let router = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/shell", post(create_shell))
        .route("/api/attach", post(grant))
        .route("/pty", get(terminal))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&host).await?;
    println!("Daemon-backed WebGPU playground: {origin}");
    axum::serve(listener, router).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_projection_retains_owner_run_and_staleness() {
        let mut node: boomux::protocol::CombinedNode = serde_json::from_value(json!({
            "node_id":"owner-a","alias":"remote","local":false,"health":"unreachable",
            "current":false,"stale":true,"observed_at_ms":0,
            "remote_projection":{"node_id":"owner-a",
                "workspaces":[{"id":"workspace","name":"same name","item_count":1,"attention_count":0}],
                "shells":[{"id":"shell","workspace_id":"workspace","name":"shell","status":"running","run_id":"exact-run"}],
                "agents":[],"launchers":[]}
        })).unwrap();
        let first = remote_workspaces(&node).remove(0);
        assert_eq!(first["id"], "remote:owner-a:workspace");
        assert_eq!(first["shells"][0]["id"], "remote:owner-a:shell");
        assert_eq!(first["shells"][0]["run"]["id"], "exact-run");
        assert_eq!(first["remote"]["stale"], true);
        node.node_id = "owner-b".into();
        let second = remote_workspaces(&node).remove(0);
        assert_ne!(first["id"], second["id"]);
        assert_ne!(first["shells"][0]["id"], second["shells"][0]["id"]);
        node.remote_projection = None;
        assert!(remote_workspaces(&node).is_empty());
    }
}
