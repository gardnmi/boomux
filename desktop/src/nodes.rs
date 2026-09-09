//! Read-only Node presentation. The daemon owns registrations and observations.

use boomux::protocol::{CombinedNode, CombinedNodeSnapshot, NodeProjectionHealthCode};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeView {
    pub id: String,
    pub label: String,
    pub local: bool,
    pub route: Option<String>,
    pub health: NodeProjectionHealthCode,
    pub current: bool,
    pub observed_at_ms: u64,
    pub version: Option<String>,
    pub workspace_count: usize,
    pub shell_count: usize,
}

impl NodeView {
    pub fn connected(&self) -> bool {
        self.current && self.health == NodeProjectionHealthCode::Online
    }

    pub fn status(&self) -> &'static str {
        match self.health {
            NodeProjectionHealthCode::Online if self.current => "Connected",
            NodeProjectionHealthCode::Online | NodeProjectionHealthCode::Stale => "Stale",
            NodeProjectionHealthCode::Unobserved => "Not yet connected",
            NodeProjectionHealthCode::Reconnecting => "Reconnecting",
            NodeProjectionHealthCode::Unreachable => "Unreachable",
            NodeProjectionHealthCode::AuthenticationRequired => "Sign-in required",
            NodeProjectionHealthCode::IdentityChanged => "Identity changed",
            NodeProjectionHealthCode::IdentityConflict => "Identity conflict",
            NodeProjectionHealthCode::Unsupported => "Incompatible",
        }
    }

    pub fn guidance(&self) -> &'static str {
        match self.health {
            NodeProjectionHealthCode::AuthenticationRequired => {
                "Sign in through your existing SSH route to reconnect."
            }
            NodeProjectionHealthCode::IdentityChanged
            | NodeProjectionHealthCode::IdentityConflict => {
                "This route no longer identifies the expected Node. Inspect it before changing the registration."
            }
            NodeProjectionHealthCode::Unsupported => {
                "The remote Boomux version is incompatible. Update Boomux on that machine to match this installation."
            }
            _ if !self.connected() => {
                "Showing the last observation. A lost connection does not establish whether remote work has stopped."
            }
            _ => "Closing a terminal pane detaches it; its Shell stays on the owning Node.",
        }
    }

    pub fn last_seen(&self, now_ms: u64) -> String {
        if self.observed_at_ms == 0 {
            return "No observation yet".into();
        }
        let seconds = now_ms.saturating_sub(self.observed_at_ms) / 1_000;
        if seconds < 60 {
            "Last observed less than a minute ago".into()
        } else if seconds < 3_600 {
            format!("Last observed {} min ago", seconds / 60)
        } else {
            format!("Last observed {} h ago", seconds / 3_600)
        }
    }
}

pub fn project(snapshot: &CombinedNodeSnapshot) -> Vec<NodeView> {
    let mut nodes: Vec<_> = snapshot
        .nodes
        .iter()
        .map(|node: &CombinedNode| {
            let (workspace_count, shell_count) = if node.local {
                node.local_snapshot.as_ref().map_or((0, 0), |snapshot| {
                    (
                        snapshot.workspaces.len(),
                        snapshot
                            .workspaces
                            .iter()
                            .map(|workspace| workspace.shells.len())
                            .sum(),
                    )
                })
            } else {
                node.remote_projection
                    .as_ref()
                    .map_or((0, 0), |projection| {
                        (projection.workspaces.len(), projection.shells.len())
                    })
            };
            NodeView {
                id: node.node_id.clone(),
                label: if node.local {
                    "This computer".into()
                } else {
                    node.alias.clone()
                },
                local: node.local,
                route: node.route.clone(),
                health: node.health,
                current: node.current && !node.stale,
                observed_at_ms: node.observed_at_ms,
                version: node.observed_helper_version.clone(),
                workspace_count,
                shell_count,
            }
        })
        .collect();
    nodes.sort_by(|a, b| (!a.local, &a.label, &a.id).cmp(&(!b.local, &b.label, &b.id)));
    nodes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: &str, local: bool) -> CombinedNode {
        CombinedNode {
            node_id: id.into(),
            alias: "same-name".into(),
            local,
            route: (!local).then(|| "host".into()),
            registration_revision: None,
            health: NodeProjectionHealthCode::Online,
            current: true,
            stale: false,
            observed_at_ms: 1_000,
            observed_protocol_version: None,
            observed_capabilities: vec![],
            observed_helper_version: None,
            workspace_owner_eligible: true,
            workspace_owner_unavailable_reason: None,
            local_snapshot: None,
            remote_projection: None,
        }
    }

    #[test]
    fn node_identity_survives_equal_labels_and_routes() {
        let nodes = project(&CombinedNodeSnapshot {
            nodes: vec![node("b", false), node("local", true), node("a", false)],
            workspaces: vec![],
            external_workspaces: vec![],
            focused_terminal: None,
        });
        assert_eq!(
            nodes
                .iter()
                .map(|node| node.id.as_str())
                .collect::<Vec<_>>(),
            ["local", "a", "b"]
        );
        assert_eq!(nodes[0].label, "This computer");
    }

    #[test]
    fn stale_projection_never_looks_connected() {
        let mut input = node("remote", false);
        input.stale = true;
        let nodes = project(&CombinedNodeSnapshot {
            nodes: vec![input],
            workspaces: vec![],
            external_workspaces: vec![],
            focused_terminal: None,
        });
        assert!(!nodes[0].connected());
        assert_eq!(nodes[0].status(), "Stale");
        assert_eq!(nodes[0].last_seen(121_000), "Last observed 2 min ago");
        assert_eq!(
            nodes[0].last_seen(0),
            "Last observed less than a minute ago"
        );
    }
    #[test]
    fn health_states_keep_distinct_recovery_meanings() {
        let cases = [
            (NodeProjectionHealthCode::Unobserved, "Not yet connected"),
            (NodeProjectionHealthCode::Reconnecting, "Reconnecting"),
            (NodeProjectionHealthCode::Unreachable, "Unreachable"),
            (
                NodeProjectionHealthCode::AuthenticationRequired,
                "Sign-in required",
            ),
            (
                NodeProjectionHealthCode::IdentityChanged,
                "Identity changed",
            ),
            (
                NodeProjectionHealthCode::IdentityConflict,
                "Identity conflict",
            ),
            (NodeProjectionHealthCode::Unsupported, "Incompatible"),
        ];
        for (health, label) in cases {
            let mut input = node("remote", false);
            input.health = health;
            let nodes = project(&CombinedNodeSnapshot {
                nodes: vec![input],
                workspaces: vec![],
                external_workspaces: vec![],
                focused_terminal: None,
            });
            assert_eq!(nodes[0].status(), label);
            assert!(!nodes[0].connected());
        }
    }

    #[test]
    fn counts_come_from_the_owning_node_projection() {
        let mut remote = node("remote", false);
        remote.remote_projection = Some(boomux::protocol::NodeProjectionSnapshot {
            node_id: "remote".into(),
            workspaces: vec![boomux::protocol::NodeProjectionWorkspace {
                id: "workspace".into(),
                name: "same-name".into(),
                item_count: 99,
                attention_count: 99,
            }],
            shells: vec![],
            launchers: vec![],
            agents: vec![],
        });
        let nodes = project(&CombinedNodeSnapshot {
            nodes: vec![remote],
            workspaces: vec![],
            external_workspaces: vec![],
            focused_terminal: None,
        });
        assert_eq!(nodes[0].workspace_count, 1);
        assert_eq!(nodes[0].shell_count, 0); // Item count is not a Shell count.
    }
}
