//! Local-only browser client for the existing Boomux daemon. Run with an
//! explicitly selected Boomux runtime; this executable never starts a daemon.
#[path = "../poc/webgpu-tiling/daemon_bridge.rs"]
mod daemon_bridge;
#[allow(dead_code)]
#[path = "../src/tailscale_serve.rs"]
mod tailscale_serve;
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
    protocol::{
        ProtocolFeature, Request as DaemonRequest, Response as DaemonResponse, RoutedOperation,
        RoutedOperationResult, ShellSpec, TerminalProfile,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::IntoFuture;
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
    tailnet_origin: Option<String>,
    grants: Arc<Mutex<HashMap<String, (Instant, daemon_bridge::Grant)>>>,
    attachments: Arc<Semaphore>,
    operations: Arc<Semaphore>,
    watchers: Arc<Semaphore>,
    setups: Arc<Mutex<HashMap<String, SetupLaunch>>>,
}
// Ephemeral launch proof; never infer cleanup authority from names or discovery.
struct SetupLaunch {
    created: Instant,
    workspace: String,
    shell: String,
    empty_revision: u64,
    receiver: Option<boomux::desktop_connect::ConnectResultReceiver>,
}
fn setup_cleanup_request(
    launch: &SetupLaunch,
    workspace: &boomux::protocol::WorkspaceSnapshot,
) -> Option<DaemonRequest> {
    (workspace.id == launch.workspace
        && workspace.revision == launch.empty_revision
        && workspace.shells.is_empty()
        && workspace.agents.is_empty()
        && workspace.launchers.is_empty())
    .then(|| DaemonRequest::GuardedCloseWorkspace {
        workspace_id: workspace.id.clone(),
        expected_revision: launch.empty_revision,
    })
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
    let host = req.headers().get("host").and_then(|v| v.to_str().ok());
    let origin = if host == Some(app.host.as_str()) {
        Some(app.origin.as_str())
    } else {
        app.tailnet_origin
            .as_deref()
            .filter(|origin| origin.strip_prefix("https://") == host)
    };
    let Some(origin) = origin else {
        return fail(StatusCode::FORBIDDEN, "Invalid Host").into_response();
    };
    if (req.method() != "GET" || req.uri().path() == "/pty")
        && req.headers().get("origin").and_then(|v| v.to_str().ok()) != Some(origin)
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
            "name":agent.name,"integration":agent.integration,"run_id":agent.run_id,"observation":{"state":agent.state,"observed_at_ms":agent.observed_at_ms},"attention":agent.attention,
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
        let mut nodes = Vec::new();
        let warning = match app.client.combined_node_snapshot(None) {
            Ok(combined) => {
                nodes = combined.nodes.iter().map(|n|json!({"id":n.node_id,"alias":n.alias,"local":n.local,"route":n.route,"health":n.health,"current":n.current,"stale":n.stale,"registration_revision":n.registration_revision,"observed_at_ms":n.observed_at_ms,"version":n.observed_helper_version})).collect();
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
            "snapshot":snapshot,"warning":warning,"nodes":nodes}),
        )
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GitRequest {
    node_id: String,
    owner: Option<String>,
    #[serde(default)]
    refresh: bool,
}
async fn git_overview(State(app): State<App>, Json(request): Json<GitRequest>) -> ApiResult {
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong gateway Node".into());
        }
        let overview = app
            .client
            .git_overview(
                request.owner.as_deref(),
                request.refresh,
                Duration::from_secs(2),
            )
            .map_err(|e| e.to_string())?;
        serde_json::to_value(overview).map_err(|e| e.to_string())
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangesRequest {
    node_id: String,
    cursor: Option<boomux::protocol::EventCursor>,
}
async fn changes(State(mut app): State<App>, Json(request): Json<ChangesRequest>) -> ApiResult {
    // Long polls have a separate small budget so they cannot starve mutations.
    app.operations = app.watchers.clone();
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        let batch = app
            .client
            .events(request.cursor, 256, 25_000)
            .map_err(|e| e.to_string())?;
        let changed = batch.snapshot.is_some()
            || batch.events.iter().any(|event| {
                !matches!(
                    event.kind,
                    boomux::protocol::DaemonEventKind::OutputChanged { .. }
                        | boomux::protocol::DaemonEventKind::FocusedTerminalPresentationChanged
                )
            });
        Ok(json!({"cursor":batch.cursor,"changed":changed}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DesktopQuery {
    node_id: String,
    workspace_id: Option<String>,
}
async fn desktop_data(State(app): State<App>, Json(query): Json<DesktopQuery>) -> ApiResult {
    operation(app, move |app| {
        if query.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        use boomux::protocol::{HostServiceOperation, HostServiceResult};
        let result = if let Some(workspace) = query.workspace_id {
            let owner = remote::identity(&workspace);
            let operation = HostServiceOperation::ListWorkspaceConversations {
                workspace_id: owner
                    .as_ref()
                    .map_or(workspace.as_str(), |id| id.inner_id.as_str())
                    .into(),
            };
            match owner {
                Some(owner) => app.client.route_node_host_service(owner.node_id, operation),
                None => app.client.host_service(operation),
            }
        } else {
            app.client
                .host_service(HostServiceOperation::DiscoverProjects)
        }
        .map_err(|e| e.to_string())?;
        match result {
            HostServiceResult::WorkspaceConversations { conversations } => {
                Ok(json!({"conversations":conversations}))
            }
            HostServiceResult::Projects { discovery } => {
                Ok(json!({"projects":discovery.projects,"warnings":discovery.warnings}))
            }
            _ => Err("Unexpected Desktop data response".into()),
        }
    })
    .await
}
#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum ResourceAction {
    FinishSetup {
        token: String,
    },
    OpenConversation {
        workspace_id: String,
        agent_id: String,
        shell_id: String,
    },
    RenameNode {
        id: String,
        name: String,
        revision: u64,
    },
    ForgetNode {
        id: String,
    },
    Rename {
        id: String,
        name: String,
        workspace: bool,
    },
    CreateWorkspace {
        name: String,
        cwd: Option<PathBuf>,
        owner: Option<String>,
    },
    RemoveWorkspace {
        id: String,
    },
    StartShell {
        id: String,
    },
    Guided {
        workflow: GuidedWorkflow,
        owner: Option<String>,
    },
    AcknowledgeAgent {
        id: String,
        revision: u64,
    },
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum GuidedWorkflow {
    Connect,
    Upgrade,
    Uninstall,
    Reauthenticate,
    Configure,
    Setup,
}
fn guided_arguments(workflow: &GuidedWorkflow, owner: Option<&str>) -> Result<Vec<String>, String> {
    let command = match workflow {
        GuidedWorkflow::Connect => "__guided-node-add",
        GuidedWorkflow::Upgrade => "__guided-node-upgrade",
        GuidedWorkflow::Uninstall => "__guided-node-uninstall",
        GuidedWorkflow::Reauthenticate => "__guided-node-reauthenticate",
        GuidedWorkflow::Configure => "config",
        GuidedWorkflow::Setup => "__desktop-setup",
    };
    let mut args = vec![command.to_string()];
    if matches!(
        workflow,
        GuidedWorkflow::Upgrade | GuidedWorkflow::Uninstall | GuidedWorkflow::Reauthenticate
    ) {
        args.push(
            owner
                .filter(|id| !id.is_empty())
                .ok_or("Select a remote Node")?
                .to_string(),
        );
    }
    if matches!(workflow, GuidedWorkflow::Configure) {
        args.push("edit".into());
    }
    Ok(args)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceRequest {
    node_id: String,
    operation: ResourceAction,
}
async fn resource_action(
    State(app): State<App>,
    Json(request): Json<ResourceRequest>,
) -> ApiResult {
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        match request.operation {
            ResourceAction::FinishSetup { token } => {
                let shell_id = {
                    let setups = app.setups.lock().map_err(|_| "Setup state unavailable")?;
                    setups
                        .get(&token)
                        .ok_or("Setup result expired; open the remote Workspace from the sidebar")?
                        .shell
                        .clone()
                };
                match app.client.get_shell(&shell_id) {
                    Ok(_) => return Ok(json!({"pending":true})),
                    Err(client::ClientError::Remote(error))
                        if error.code == Some(boomux::protocol::ErrorCode::NotFound) => {}
                    Err(error) => return Err(error.to_string()),
                }
                let launch = app
                    .setups
                    .lock()
                    .map_err(|_| "Setup state unavailable")?
                    .remove(&token)
                    .ok_or("Setup already consumed")?;
                let cleanup = match app.client.get_workspace(&launch.workspace) {
                    Ok(workspace) => setup_cleanup_request(&launch, &workspace)
                        .map(|request| app.client.request(request))
                        .transpose()
                        .map(|_| ())
                        .map_err(|e| e.to_string()),
                    Err(client::ClientError::Remote(error))
                        if error.code == Some(boomux::protocol::ErrorCode::NotFound) =>
                    {
                        Ok(())
                    }
                    Err(error) => Err(error.to_string()),
                };
                let identity = launch
                    .receiver
                    .map(|receiver| receiver.receive())
                    .transpose()
                    .map_err(|e| e.to_string())?
                    .flatten();
                let shell = identity
                    .map(|identity| {
                        let key = remote::key(&identity.node_id, &identity.inner_id);
                        let mut shell =
                            remote::shell(&app.client, &key).map_err(|e| e.to_string())?;
                        remote::qualify_shell(&identity.node_id, &mut shell);
                        Ok::<_, String>(shell)
                    })
                    .transpose()?;
                Ok(json!({"pending":false,"shell":shell,"warning":cleanup.err()}))
            }

            ResourceAction::OpenConversation {
                workspace_id,
                agent_id,
                shell_id,
            } => {
                uuid::Uuid::parse_str(&shell_id).map_err(|_| "Invalid conversation attempt ID")?;
                let owner = remote::identity(&workspace_id);
                let mut shell = app
                    .client
                    .open_workspace_conversation(
                        owner.as_ref().map(|id| id.node_id.as_str()),
                        owner
                            .as_ref()
                            .map_or(workspace_id.as_str(), |id| id.inner_id.as_str()),
                        &agent_id,
                        &shell_id,
                    )
                    .map_err(|e| e.to_string())?;
                if let Some(owner) = owner {
                    remote::qualify_shell(&owner.node_id, &mut shell);
                }
                Ok(json!({"shell":shell,"workspace_id":workspace_id}))
            }
            ResourceAction::RenameNode { id, name, revision } => {
                if id == app.node_id {
                    return Err("Select a remote Node".into());
                }
                app.client
                    .rename_node_registration(id, name, revision)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"ok":true}))
            }
            ResourceAction::ForgetNode { id } => {
                if id == app.node_id {
                    return Err("Select a remote Node".into());
                }
                app.client
                    .forget_node_registration(id)
                    .map_err(|e| e.to_string())?;
                Ok(json!({"ok":true}))
            }
            ResourceAction::Guided { workflow, owner } => {
                if matches!(
                    workflow,
                    GuidedWorkflow::Upgrade
                        | GuidedWorkflow::Uninstall
                        | GuidedWorkflow::Reauthenticate
                ) {
                    let owner = owner.as_deref().ok_or("Select a remote Node")?;
                    if owner == app.node_id {
                        return Err("Select a remote Node".into());
                    }
                    app.client
                        .node_registration(owner)
                        .map_err(|e| e.to_string())?;
                }
                let temporary = matches!(
                    workflow,
                    GuidedWorkflow::Connect
                        | GuidedWorkflow::Upgrade
                        | GuidedWorkflow::Uninstall
                        | GuidedWorkflow::Reauthenticate
                );
                // Hold the short creation reservation so concurrent requests cannot exceed the cap.
                let mut setups = app.setups.lock().map_err(|_| "Setup state unavailable")?;
                setups.retain(|_, launch| launch.created.elapsed() < Duration::from_secs(7200));
                if temporary && setups.len() >= 8 {
                    return Err("Finish an existing remote setup before opening another".into());
                }
                let receiver = if matches!(workflow, GuidedWorkflow::Connect) {
                    Some(
                        boomux::desktop_connect::ConnectResultReceiver::new()
                            .map_err(|e| e.to_string())?,
                    )
                } else {
                    None
                };
                let executable = std::env::current_exe().map_err(|e| e.to_string())?;
                let cli = executable
                    .parent()
                    .and_then(|p| p.parent())
                    .map(|p| p.join("boomux"))
                    .filter(|p| p.is_file())
                    .ok_or(
                        "Matching Boomux CLI is unavailable; build the CLI beside this gateway",
                    )?;
                let mut command = vec![cli.to_str().ok_or("CLI path is not UTF-8")?.to_string()];
                command.extend(guided_arguments(&workflow, owner.as_deref())?);
                if let Some(receiver) = &receiver {
                    command.push("--result-socket".into());
                    command.push(
                        receiver
                            .path()
                            .to_str()
                            .ok_or("Invalid setup result path")?
                            .into(),
                    );
                }
                let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
                let workspace = app
                    .client
                    .create_workspace_with_default_cwd(
                        format!("setup-{}", &uuid::Uuid::new_v4().to_string()[..8]),
                        Some(cwd.clone()),
                        vec![ShellSpec {
                            name: "Boomux setup".into(),
                            command,
                            cwd,
                        }],
                    )
                    .map_err(|e| e.to_string())?;
                // Return the exact newly-created pending Shell. Starting it is
                // explicit; the guided CLI retains its own confirmation flow.
                let empty_revision = workspace
                    .revision
                    .checked_add(1)
                    .ok_or("Workspace revision overflow")?;
                let shell = workspace
                    .shells
                    .into_iter()
                    .next()
                    .ok_or("Setup Shell missing")?;
                let token = if temporary {
                    let token = uuid::Uuid::new_v4().to_string();
                    setups.insert(
                        token.clone(),
                        SetupLaunch {
                            created: Instant::now(),
                            workspace: workspace.id.clone(),
                            shell: shell.id.clone(),
                            empty_revision,
                            receiver,
                        },
                    );
                    Some(token)
                } else {
                    None
                };
                Ok(json!({"shell":shell,"workspace_id":workspace.id,"setup_token":token}))
            }
            ResourceAction::AcknowledgeAgent { id, revision } => {
                if let Some(identity) = remote::identity(&id) {
                    app.client
                        .route_node_operation(
                            identity.node_id,
                            RoutedOperation::AcknowledgeAgentAttention {
                                agent_id: identity.inner_id,
                                observation_revision: revision,
                            },
                        )
                        .map_err(|e| e.to_string())?;
                } else {
                    app.client
                        .acknowledge_agent_attention(id, revision)
                        .map_err(|e| e.to_string())?;
                }
                Ok(json!({"ok":true}))
            }
            ResourceAction::Rename {
                id,
                name,
                workspace,
            } => {
                if name.trim().is_empty() || name.len() > 256 {
                    return Err("Use a name between 1 and 256 bytes".into());
                }
                if remote::identity(&id).is_some() {
                    remote::rename(&app.client, &id, &name, workspace)?;
                } else {
                    let request = if workspace {
                        let current = app.client.get_workspace(&id).map_err(|e| e.to_string())?;
                        DaemonRequest::GuardedRenameWorkspace {
                            workspace_id: id,
                            name,
                            expected_revision: current.revision,
                        }
                    } else {
                        let current = app.client.get_shell(&id).map_err(|e| e.to_string())?;
                        DaemonRequest::GuardedRenameShell {
                            shell_id: id,
                            name,
                            expected_revision: current.revision,
                        }
                    };
                    match app.client.request(request).map_err(|e| e.to_string())? {
                        DaemonResponse::Workspace { .. } if workspace => {}
                        DaemonResponse::Shell { .. } if !workspace => {}
                        _ => return Err("Unexpected rename response".into()),
                    }
                }
                Ok(json!({"ok":true}))
            }
            ResourceAction::CreateWorkspace { name, cwd, owner } => {
                if name.trim().is_empty() || name.len() > 256 {
                    return Err("Use a name between 1 and 256 bytes".into());
                }
                if let Some(owner) = owner.filter(|owner| owner != &app.node_id) {
                    let shell = remote::create_workspace(&app.client, &owner, &name)?;
                    Ok(json!({"workspace_id":shell.workspace_id}))
                } else {
                    let cwd = cwd.unwrap_or(std::env::current_dir().map_err(|e| e.to_string())?);
                    if !cwd.is_absolute() || !cwd.is_dir() {
                        return Err("Choose an existing absolute directory".into());
                    }
                    let workspace = app
                        .client
                        .create_workspace_with_default_cwd(name, Some(cwd), Vec::new())
                        .map_err(|e| e.to_string())?;
                    Ok(json!({"workspace_id":workspace.id}))
                }
            }
            ResourceAction::RemoveWorkspace { id } => {
                if remote::identity(&id).is_some() {
                    remote::close(&app.client, &id, true)?;
                } else {
                    let current = app.client.get_workspace(&id).map_err(|e| e.to_string())?;
                    match app
                        .client
                        .request(DaemonRequest::GuardedCloseWorkspace {
                            workspace_id: id,
                            expected_revision: current.revision,
                        })
                        .map_err(|e| e.to_string())?
                    {
                        DaemonResponse::Ok => {}
                        _ => return Err("Unexpected removal response".into()),
                    }
                }
                Ok(json!({"ok":true}))
            }
            ResourceAction::StartShell { id } => {
                let shell = remote::shell(&app.client, &id).map_err(|e| e.to_string())?;
                if shell.run.is_some() {
                    return Err("Shell already has a run; open its current run".into());
                }
                // Never restart an exited run or take another controller's attachment.
                if let Some(identity) = remote::identity(&id) {
                    drop(
                        app.client
                            .attach_node(identity, false, false, None, profile(24, 80))
                            .map_err(|e| e.to_string())?,
                    );
                } else {
                    drop(
                        app.client
                            .attach(&id, false, profile(24, 80))
                            .map_err(|e| e.to_string())?,
                    );
                }
                let mut shell = remote::shell(&app.client, &id).map_err(|e| e.to_string())?;
                if let Some(identity) = remote::identity(&id) {
                    remote::qualify_shell(&identity.node_id, &mut shell);
                }
                Ok(json!({"shell":shell}))
            }
        }
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
fn shell_spec(cwd: PathBuf) -> ShellSpec {
    ShellSpec::login(
        format!("web-{}", &uuid::Uuid::new_v4().to_string()[..8]),
        cwd,
    )
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
                shell_spec(workspace.default_cwd.unwrap_or_else(|| app.root.clone())),
                profile(24, 80),
            )
            .map_err(|e| e.to_string())?;
        Ok(json!({"node_id":app.node_id,"shell":shell}))
    })
    .await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveShell {
    node_id: String,
    shell_id: String,
    run_id: Option<String>,
}
fn check_remove_run(expected: Option<&str>, current: Option<&str>) -> Result<(), String> {
    if expected != current {
        return Err("ShellRun changed; refresh and confirm removal again".into());
    }
    Ok(())
}
async fn remove_shell(State(app): State<App>, Json(request): Json<RemoveShell>) -> ApiResult {
    operation(app, move |app| {
        if request.node_id != app.node_id {
            return Err("Wrong owning Node".into());
        }
        // Resolve the live owner before any mutation; cached remote projections
        // are never authority to remove a resource. After the run preflight,
        // use Desktop's revision-guarded removal to reject metadata changes.
        let shell = remote::shell(&app.client, &request.shell_id).map_err(|e| e.to_string())?;
        check_remove_run(
            request.run_id.as_deref(),
            shell.run.as_ref().map(|r| r.id.as_str()),
        )?;
        if let Some(identity) = remote::identity(&request.shell_id) {
            match app
                .client
                .route_node_operation(
                    identity.node_id,
                    RoutedOperation::CloseShell {
                        shell_id: identity.inner_id,
                        expected_revision: shell.revision,
                    },
                )
                .map_err(|e| e.to_string())?
            {
                RoutedOperationResult::Ok => {}
                _ => return Err("Unexpected remote Shell removal response".into()),
            }
        } else {
            match app
                .client
                .request(DaemonRequest::GuardedCloseShell {
                    shell_id: shell.id,
                    expected_revision: shell.revision,
                })
                .map_err(|e| e.to_string())?
            {
                DaemonResponse::Ok => {}
                _ => return Err("Unexpected Shell removal response".into()),
            }
        }
        Ok(json!({"removed":true}))
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
        "/desktop-panels.js" => ("poc/webgpu-tiling/desktop-panels.js", "text/javascript"),
        "/app.js" => ("poc/webgpu-tiling/app.js", "text/javascript"),
        "/themes.js" => ("poc/webgpu-tiling/themes.js", "text/javascript"),
        "/terminal.js" => ("poc/webgpu-tiling/terminal.js", "text/javascript"),
        "/renderer.js" => ("poc/webgpu-tiling/renderer.js", "text/javascript"),
        "/layout.js" => ("poc/webgpu-tiling/layout.js", "text/javascript"),
        "/vendor/jetbrains-mono-nerd.woff2" => (
            "poc/webgpu-tiling/fonts/jetbrains-mono-nerd.woff2",
            "font/woff2",
        ),
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
async fn main() {
    if let Err(error) = run_gateway().await {
        println!("{}", json!({"error":error.to_string()}));
        std::process::exit(1);
    }
}
async fn run_gateway() -> Result<(), Box<dyn std::error::Error>> {
    let desktop = std::env::args().any(|arg| arg == "--desktop");
    let tailscale = std::env::args().any(|arg| arg == "--tailscale");
    let (closed, lifetime) = tokio::sync::oneshot::channel();
    if desktop {
        std::thread::spawn(move || {
            use std::io::Read;
            let _ = std::io::stdin().read(&mut [0]);
            let _ = closed.send(());
        });
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let client = client::connect_if_running()?
        .ok_or("Start the selected Boomux daemon before running the gateway")?;
    if !client.supports(ProtocolFeature::CreateStartedShell)? {
        return Err("This PoC requires a daemon with CreateStartedShell support".into());
    }
    let node_id = client.node_identity()?;
    let workspace_id = if desktop {
        client
            .snapshot()?
            .workspaces
            .first()
            .map(|workspace| workspace.id.clone())
            .unwrap_or_default()
    } else if let Ok(id) = std::env::var("POC_WORKSPACE_ID") {
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
                    vec![shell_spec(root.clone())],
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
    // Bind before mutating Serve: a second publisher cannot clean up the live route.
    let listener = tokio::net::TcpListener::bind(&host).await?;
    for asset in [
        "poc/webgpu-tiling/index.html",
        "node_modules/ghostty-web/ghostty-vt.wasm",
        "node_modules/ghostty-web/dist/ghostty-web.js",
    ] {
        if !root.join(asset).is_file() {
            return Err(format!(
                "Web UI assets missing: {asset}; run bun install --frozen-lockfile in {}",
                root.display()
            )
            .into());
        }
    }
    let exposure = if tailscale {
        Some(tailscale_serve::Exposure::enable_tiling(port)?)
    } else {
        None
    };
    let tailnet_origin = exposure.as_ref().map(|exposure| exposure.dashboard_url());
    let url = tailnet_origin.clone().unwrap_or_else(|| origin.clone());
    let app = App {
        client,
        node_id,
        workspace_id,
        root,
        host: host.clone(),
        origin: origin.clone(),
        tailnet_origin,
        grants: Arc::new(Mutex::new(HashMap::new())),
        attachments: Arc::new(Semaphore::new(24)),
        operations: Arc::new(Semaphore::new(8)),
        watchers: Arc::new(Semaphore::new(4)),
        setups: Arc::new(Mutex::new(HashMap::new())),
    };
    let router = Router::new()
        .route("/api/snapshot", get(snapshot))
        .route("/api/git", post(git_overview))
        .route("/api/desktop", post(desktop_data))
        .route("/api/shell", post(create_shell))
        .route("/api/shell/remove", post(remove_shell))
        .route("/api/resource", post(resource_action))
        .route("/api/changes", post(changes))
        .route("/api/attach", post(grant))
        .route("/pty", get(terminal))
        .fallback(get(asset))
        .layer(DefaultBodyLimit::max(4096))
        .layer(middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app);
    println!("{}", json!({"url":url}));
    use std::io::Write;
    std::io::stdout().flush()?;
    let shutdown = async move {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {},
            _ = terminate.recv() => {},
            _ = lifetime, if desktop => {},
        }
    };
    // Drop live WebSockets on stop; waiting for browser disconnect would retain sharing.
    tokio::select! {
        result = axum::serve(listener, router).into_future() => { result?; },
        _ = shutdown => {},
    }
    if let Some(mut exposure) = exposure {
        exposure.cleanup()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn guided_workflows_preserve_exact_owner_arguments() {
        let owner = "node; $(touch /tmp/never)";
        assert_eq!(
            super::guided_arguments(&super::GuidedWorkflow::Upgrade, Some(owner)).unwrap(),
            vec!["__guided-node-upgrade", owner]
        );
        assert!(super::guided_arguments(&super::GuidedWorkflow::Uninstall, None).is_err());
        assert_eq!(
            super::guided_arguments(&super::GuidedWorkflow::Configure, None).unwrap(),
            vec!["config", "edit"]
        );
        assert!(serde_json::from_value::<super::ResourceRequest>(serde_json::json!({"node_id":"local","operation":{"action":"guided","workflow":"arbitrary-command"}})).is_err());
    }

    use super::*;

    #[test]
    fn setup_cleanup_never_adopts_a_new_revision_or_another_workspace() {
        let launch = SetupLaunch {
            created: Instant::now(),
            workspace: "setup".into(),
            shell: "shell".into(),
            empty_revision: 2,
            receiver: None,
        };
        let mut workspace: boomux::protocol::WorkspaceSnapshot = serde_json::from_value(
            json!({"id":"setup","name":"setup","revision":2,"shells":[],"agents":[]}),
        )
        .unwrap();
        assert!(matches!(
            setup_cleanup_request(&launch, &workspace),
            Some(DaemonRequest::GuardedCloseWorkspace {
                expected_revision: 2,
                ..
            })
        ));
        workspace.revision = 3;
        assert!(setup_cleanup_request(&launch, &workspace).is_none());
        workspace.revision = 2;
        workspace.id = "another".into();
        assert!(setup_cleanup_request(&launch, &workspace).is_none());
        workspace.id = "setup".into();
        workspace.shells.push(serde_json::from_value(json!({"id":"new-shell","workspace_id":"setup","name":"added work","command":["bash"],"cwd":"/tmp","status":"pending"})).unwrap());
        assert!(setup_cleanup_request(&launch, &workspace).is_none());
    }

    #[test]
    fn web_shell_uses_owner_default_startup() {
        let cwd = PathBuf::from("/workspace with spaces");
        let spec = shell_spec(cwd.clone());
        assert_eq!(spec.command, ShellSpec::login("desktop", &cwd).command);
        assert!(spec.command.is_empty());
        assert_eq!(spec.cwd, cwd);
    }

    #[test]
    fn removal_rejects_stale_run_including_pending_transitions() {
        assert!(check_remove_run(Some("run-a"), Some("run-a")).is_ok());
        assert!(check_remove_run(None, None).is_ok());
        assert!(check_remove_run(Some("run-a"), Some("run-b")).is_err());
        assert!(check_remove_run(None, Some("run-a")).is_err());
        assert!(check_remove_run(Some("run-a"), None).is_err());
    }

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
