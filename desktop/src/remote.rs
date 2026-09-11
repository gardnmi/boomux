//! Desktop keys retain both owner and resource identity. They are never wire IDs.
use boomux::client::{Client, ClientError};
use boomux::protocol::ShellSpec;
use boomux::protocol::{
    QualifiedIdentity, RoutedOperation, RoutedOperationResult, ShellSnapshot, WorkspaceSnapshot,
};
use std::path::PathBuf;

pub fn qualify_shell(node: &str, shell: &mut ShellSnapshot) {
    shell.id = key(node, &shell.id);
    shell.workspace_id = key(node, &shell.workspace_id);
}

pub fn create_workspace(client: &Client, node: &str, name: &str) -> Result<ShellSnapshot, String> {
    let mut shell = client
        .create_remote_workspace(node, name)
        .map_err(|e| e.to_string())?;
    qualify_shell(node, &mut shell);
    Ok(shell)
}

pub fn create_shell(client: &Client, workspace_key: &str) -> Result<ShellSnapshot, String> {
    let id = identity(workspace_key).ok_or("Remote workspace identity is missing")?;
    let workspace = workspace(client, workspace_key).map_err(|e| e.to_string())?;
    let cwd = workspace
        .default_cwd
        .or_else(|| workspace.shells.first().map(|s| s.cwd.clone()))
        .ok_or("Remote workspace has no starting directory")?;
    create(client, &id.node_id, &id.inner_id, &workspace.name, cwd)
}

fn create(
    client: &Client,
    node: &str,
    workspace: &str,
    name: &str,
    cwd: PathBuf,
) -> Result<ShellSnapshot, String> {
    let shell_name = boomux::generated_names::random_excluding(std::iter::empty())
        .ok_or("Shell names exhausted")?;
    // Exact IDs are generated once. An ambiguous response is surfaced, never retried as new work.
    match client
        .route_node_operation(
            node,
            RoutedOperation::CreateWorkspaceShell {
                workspace_id: workspace.into(),
                workspace_name: name.into(),
                default_cwd: Some(cwd.clone()),
                shell_id: uuid::Uuid::new_v4().to_string(),
                shell: ShellSpec::login(shell_name, cwd),
            },
        )
        .map_err(|e| e.to_string())?
    {
        RoutedOperationResult::Shell { mut shell } => {
            qualify_shell(node, &mut shell);
            Ok(shell)
        }
        _ => Err("Unexpected remote creation response".into()),
    }
}

pub fn rename(
    client: &Client,
    resource: &str,
    name: &str,
    is_workspace: bool,
) -> Result<(), String> {
    let id = identity(resource).ok_or("Remote identity missing")?;
    let operation = if is_workspace {
        let current = workspace(client, resource).map_err(|e| e.to_string())?;
        RoutedOperation::RenameWorkspace {
            workspace_id: id.inner_id,
            name: name.into(),
            expected_revision: current.revision,
        }
    } else {
        let current = shell(client, resource).map_err(|e| e.to_string())?;
        RoutedOperation::RenameShell {
            shell_id: id.inner_id,
            name: name.into(),
            expected_revision: current.revision,
        }
    };
    client
        .route_node_operation(id.node_id, operation)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn close(client: &Client, resource: &str, is_workspace: bool) -> Result<(), String> {
    let id = identity(resource).ok_or("Remote identity missing")?;
    let operation = if is_workspace {
        let current = workspace(client, resource).map_err(|e| e.to_string())?;
        RoutedOperation::CloseWorkspace {
            workspace_id: id.inner_id,
            expected_revision: current.revision,
        }
    } else {
        let current = shell(client, resource).map_err(|e| e.to_string())?;
        RoutedOperation::CloseShell {
            shell_id: id.inner_id,
            expected_revision: current.revision,
        }
    };
    client
        .route_node_operation(id.node_id, operation)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn key(node: &str, resource: &str) -> String {
    format!("remote:{node}:{resource}")
}

pub fn identity(key: &str) -> Option<QualifiedIdentity> {
    let (node_id, inner_id) = key.strip_prefix("remote:")?.split_once(':')?;
    Some(QualifiedIdentity {
        node_id: node_id.into(),
        inner_id: inner_id.into(),
    })
}

pub fn shell(client: &Client, key: &str) -> Result<ShellSnapshot, ClientError> {
    let Some(id) = identity(key) else {
        return client.get_shell(key);
    };
    match client.route_node_operation(
        id.node_id,
        RoutedOperation::GetShell {
            shell_id: id.inner_id,
        },
    )? {
        RoutedOperationResult::Shell { shell } => Ok(shell),
        _ => Err(std::io::Error::other("unexpected remote Shell response").into()),
    }
}

pub fn workspace(client: &Client, key: &str) -> Result<WorkspaceSnapshot, ClientError> {
    let Some(id) = identity(key) else {
        return client.get_workspace(key);
    };
    match client.route_node_operation(
        id.node_id,
        RoutedOperation::GetWorkspace {
            workspace_id: id.inner_id,
        },
    )? {
        RoutedOperationResult::Workspace { workspace } => Ok(workspace),
        _ => Err(std::io::Error::other("unexpected remote Workspace response").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_workspace_resolves_owner_directory_and_routes_creation_without_local_fallback() {
        use boomux::protocol::{
            self, Envelope, HostServiceOperation, HostServiceResult, Request, Response,
        };
        use std::os::unix::net::UnixListener;
        let directory = std::env::temp_dir().join(format!("rw-{:016x}", fastrand::u64(..)));
        std::fs::create_dir(&directory).unwrap();
        let socket = directory.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            assert_eq!(
                request.message,
                Request::RouteNodeHostService {
                    node_id: "remote-owner".into(),
                    operation: HostServiceOperation::ResolveDirectory {
                        path: PathBuf::from(".")
                    },
                }
            );
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    protocol::PROTOCOL_VERSION,
                    Response::HostService {
                        result: HostServiceResult::Directory {
                            path: PathBuf::from("/remote/start"),
                        },
                    },
                ),
            )
            .unwrap();
            let (mut stream, _) = listener.accept().unwrap();
            let request: Envelope<Request> = protocol::read_message(&mut stream).unwrap();
            let Request::RouteNodeOperation {
                node_id,
                operation:
                    RoutedOperation::CreateWorkspaceShell {
                        workspace_id,
                        workspace_name,
                        default_cwd,
                        shell_id,
                        shell,
                    },
            } = request.message
            else {
                panic!("creation must be routed")
            };
            assert_eq!(node_id, "remote-owner");
            assert_eq!(workspace_name, "development");
            assert_eq!(shell.cwd, PathBuf::from("/remote/start"));
            assert_eq!(default_cwd, Some(shell.cwd.clone()));
            assert!(shell.command.is_empty());
            assert!(uuid::Uuid::parse_str(&workspace_id).is_ok());
            assert!(uuid::Uuid::parse_str(&shell_id).is_ok());
            let snapshot: ShellSnapshot = serde_json::from_value(serde_json::json!({
                "id": shell_id, "workspace_id": workspace_id, "name": shell.name, "cwd": shell.cwd, "status": "pending"
            })).unwrap();
            protocol::write_message(
                &mut stream,
                &Envelope::with_version(
                    protocol::PROTOCOL_VERSION,
                    Response::RoutedNodeOperation {
                        result: RoutedOperationResult::Shell { shell: snapshot },
                    },
                ),
            )
            .unwrap();
        });
        let client = Client::from_socket_path(socket);
        let shell = create_workspace(&client, "remote-owner", "development").unwrap();
        assert_eq!(identity(&shell.id).unwrap().node_id, "remote-owner");
        assert_eq!(
            identity(&shell.workspace_id).unwrap().node_id,
            "remote-owner"
        );
        server.join().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
    #[test]
    fn keys_keep_same_named_resources_on_different_owners_distinct() {
        assert_ne!(key("one", "shell"), key("two", "shell"));
        let id = identity(&key("one", "shell")).unwrap();
        assert_eq!(id.node_id, "one");
        assert_eq!(id.inner_id, "shell");
        assert!(identity("shell").is_none());
    }
}
