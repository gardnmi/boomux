//! On-demand Agent inspection; the owning daemon performs all filesystem reads.
use super::*;
use boomux::agent_inspection::{Inspection, Scope};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Tab {
    #[default]
    Overview,
    Skills,
    Mcp,
}

pub struct Model {
    pub agent: AgentChoice,
    pub machine: String,
    pub tab: Tab,
    pub expanded: Option<usize>,
    pub inspection: Option<Inspection>,
    pub error: Option<String>,
    pub busy: bool,
    pub copied: bool,
    pub generation: u64,
    pub task: Option<gpui::Task<()>>,
    pub scroll: ScrollHandle,
}

impl Model {
    fn apply_result(&mut self, generation: u64, result: Result<Inspection, String>) -> bool {
        if self.generation != generation {
            return false;
        }
        self.busy = false;
        self.expanded = None;
        match result {
            Ok(inspection) => {
                self.inspection = Some(inspection);
                self.error = None;
            }
            Err(error) => self.error = Some(error),
        }
        true
    }
}

fn scope(scope: &Scope) -> &'static str {
    match scope {
        Scope::User => "User",
        Scope::Project => "Project",
        Scope::System => "System",
    }
}

fn fetch(agent: &AgentChoice) -> Result<Inspection, String> {
    let client = boomux::client::Client::from_socket_path(
        boomux::client::socket_path().map_err(|e| e.to_string())?,
    );
    let identity = remote::identity(&agent.id);
    let result = client.inspect_agent(
        identity.as_ref().map(|i| i.node_id.as_str()),
        identity
            .as_ref()
            .map_or(agent.id.as_str(), |i| i.inner_id.as_str()),
        &agent.run_id,
        Duration::from_secs(5),
    );
    result.map_err(|error| format!("Could not inspect this Agent: {error}. Inspection requires Boomux protocol 55 or newer on this computer and the Agent's machine."))
}

fn field(label: &str, value: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(0xa6adc8))
                .child(label.to_owned()),
        )
        .child(div().text_sm().child(value.into()))
}

fn note(text: impl Into<SharedString>) -> Div {
    div().text_xs().text_color(rgb(0xa6adc8)).child(text.into())
}

impl Workspace {
    pub(super) fn open_agent_details(
        &mut self,
        agent: AgentChoice,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let machine = remote::identity(&agent.id).map_or_else(
            || "This computer".into(),
            |id| {
                self.node_views
                    .iter()
                    .find(|n| n.id == id.node_id)
                    .map_or(id.node_id, |n| n.label.clone())
            },
        );
        self.agent_details_generation = self.agent_details_generation.wrapping_add(1);
        self.agent_details = Some(Model {
            agent,
            machine,
            tab: Tab::Overview,
            expanded: None,
            inspection: None,
            error: None,
            busy: false,
            copied: false,
            generation: self.agent_details_generation,
            task: None,
            scroll: ScrollHandle::new(),
        });
        self.project_menu_open = false;
        self.sidebar_menu = None;
        window.focus(&self.focus_handle, cx);
        self.refresh_agent_details(cx);
    }

    fn refresh_agent_details(&mut self, cx: &mut Context<Self>) {
        let Some(model) = &mut self.agent_details else {
            return;
        };
        if model.busy {
            return;
        }
        model.busy = true;
        model.copied = false;
        let agent = model.agent.clone();
        let generation = model.generation;
        model.task = Some(cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { fetch(&agent) }).await;
            let _ = this.update(cx, |this, cx| {
                let Some(model) = &mut this.agent_details else {
                    return;
                };
                if !model.apply_result(generation, result) {
                    return;
                }
                cx.notify();
            });
        }));
        cx.notify();
    }

    pub(super) fn agent_details_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let Some(model) = &mut self.agent_details else {
            return;
        };
        match event.keystroke.key.as_str() {
            "escape" => {
                self.agent_details = None;
            }
            "up" | "down" | "pageup" | "pagedown" => {
                let offset = model.scroll.offset();
                let delta = if matches!(event.keystroke.key.as_str(), "pageup" | "pagedown") {
                    320.0
                } else {
                    48.0
                };
                let delta = if matches!(event.keystroke.key.as_str(), "up" | "pageup") {
                    delta
                } else {
                    -delta
                };
                model.scroll.set_offset(point(
                    offset.x,
                    (offset.y + px(delta))
                        .min(px(0.0))
                        .max(-model.scroll.max_offset().y),
                ));
            }
            "1" => {
                model.tab = Tab::Overview;
                model.expanded = None;
            }
            "2" => {
                model.tab = Tab::Skills;
                model.expanded = None;
            }
            "3" => {
                model.tab = Tab::Mcp;
                model.expanded = None;
            }
            _ => {}
        }
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn agent_details_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let model = self.agent_details.as_ref()?;
        let mut content = div().flex().flex_col().gap_4();
        if let Some(error) = &model.error {
            content = content.child(
                div()
                    .text_sm()
                    .text_color(rgb(0xf9e2af))
                    .child(error.clone()),
            );
            if model.inspection.is_some() {
                content = content.child(note(
                    "Last known details retained. Refresh did not succeed.",
                ));
            }
        }
        match model.tab {
            Tab::Overview => {
                content = content
                    .child(field("Harness", model.agent.integration.clone()))
                    .child(field("Machine", model.machine.clone()))
                    .child(field("Workspace", model.agent.workspace.clone()));
                if let Some(inspection) = &model.inspection {
                    let agent = &inspection.agent;
                    let state = format!("{:?}", agent.observation.state).to_lowercase();
                    content = content
                        .child(field("Reported state", state))
                        .child(field(
                            "Last Agent report",
                            relative_time(agent.observation.observed_at_ms),
                        ))
                        .child(field(
                            "Working directory",
                            agent
                                .cwd
                                .as_ref()
                                .map_or_else(|| "Not reported".into(), |p| p.display().to_string()),
                        ))
                        .child(field(
                            "Needs attention",
                            agent.attention.as_ref().map_or_else(
                                || "No attention request in this snapshot".to_string(),
                                |a| match a.reason {
                                    boomux::protocol::AgentAttentionReason::Blocked => {
                                        "Agent is waiting for attention".into()
                                    }
                                    boomux::protocol::AgentAttentionReason::Completed => {
                                        "Agent has reported completion".into()
                                    }
                                },
                            ),
                        ))
                        .child(field("Latest evidence", agent.observation.evidence.clone()));
                    for context in &agent.working_contexts {
                        content = content.child(field(
                            "Observed Git context",
                            format!("{} · {}", context.worktree_root.display(), context.branch),
                        ));
                    }
                    content = content.child(field("Skills found", inspection.skills.len().to_string()))
                        .child(field("MCP definitions found", inspection.mcp_servers.len().to_string()))
                        .child(note("Model, context usage, cost and live tool availability are not reported by this integration."));
                } else {
                    content = content
                        .child(field("Last known state", model.agent.state_label()))
                        .child(field(
                            "Last Agent report",
                            relative_time(model.agent.updated_at_ms),
                        ));
                }
                content = content
                    .child(field("Agent ID", model.agent.id.clone()))
                    .child(field("Run ID", model.agent.run_id.clone()));
            }
            Tab::Skills => {
                content = content.child(note("Found in skill files on the Agent’s machine. Discovery does not confirm that this run loaded or can use a skill."));
                if let Some(inspection) = &model.inspection {
                    if inspection.skills.is_empty() {
                        content = content.child(note("No skills found in the inspected locations. This is not a complete runtime inventory."));
                    }
                    for (index, skill) in inspection.skills.iter().enumerate() {
                        content = content.child(
                            div()
                                .id(SharedString::from(format!("agent-skill-{index}")))
                                .p_3()
                                .rounded_md()
                                .bg(rgb(0x181825))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(m) = &mut this.agent_details {
                                        m.expanded = (m.expanded != Some(index)).then_some(index);
                                    }
                                    cx.notify();
                                }))
                                .child(div().text_sm().child(format!(
                                    "{} {}",
                                    if model.expanded == Some(index) {
                                        "▾"
                                    } else {
                                        "▸"
                                    },
                                    skill.name
                                )))
                                .child(note(format!(
                                    "{} · Found in configuration",
                                    scope(&skill.scope)
                                )))
                                .when(model.expanded == Some(index), |row| {
                                    row.child(note(skill.description.clone()))
                                        .child(note(skill.source.display().to_string()))
                                }),
                        );
                    }
                }
            }
            Tab::Mcp => {
                content = content.child(note("Connection health, authentication and available tools are not reported. These are file definitions, not confirmed connections."));
                if let Some(inspection) = &model.inspection {
                    if inspection.mcp_servers.is_empty() {
                        content = content.child(note("No MCP definitions found in the inspected locations. Plugin and runtime connections may still exist."));
                    }
                    for (index, server) in inspection.mcp_servers.iter().enumerate() {
                        content = content.child(
                            div()
                                .id(SharedString::from(format!("agent-mcp-{index}")))
                                .p_3()
                                .rounded_md()
                                .bg(rgb(0x181825))
                                .cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    cx.stop_propagation();
                                    if let Some(m) = &mut this.agent_details {
                                        m.expanded = (m.expanded != Some(index)).then_some(index);
                                    }
                                    cx.notify();
                                }))
                                .child(div().text_sm().child(format!(
                                    "{} {}",
                                    if model.expanded == Some(index) {
                                        "▾"
                                    } else {
                                        "▸"
                                    },
                                    server.name
                                )))
                                .child(note(format!(
                                    "{} · {}",
                                    scope(&server.scope),
                                    match server.enabled {
                                        Some(false) => "Disabled in this file",
                                        Some(true) => "Enabled in this file",
                                        None => "Found in configuration",
                                    }
                                )))
                                .when(model.expanded == Some(index), |row| {
                                    row.child(note(format!(
                                        "Transport: {} · Connection not reported",
                                        server.transport
                                    )))
                                    .child(note(server.source.display().to_string()))
                                }),
                        );
                    }
                }
            }
        }
        if model.busy {
            content = content.child(note("Inspecting on the Agent's machine…"));
        }
        if let Some(inspection) = &model.inspection {
            if inspection.truncated {
                content =
                    content.child(note("Inspection limits reached; this list is incomplete."));
            }
            for warning in &inspection.warnings {
                content = content.child(note(warning.clone()));
            }
        }
        let mut tabs = div().flex().gap_2();
        for (tab, label) in [
            (Tab::Overview, "Overview"),
            (Tab::Skills, "Skills"),
            (Tab::Mcp, "MCP"),
        ] {
            tabs = tabs.child(
                Self::settings_option(label, label, model.tab == tab).on_click(cx.listener(
                    move |this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(m) = &mut this.agent_details {
                            m.tab = tab;
                            m.expanded = None;
                            m.scroll.set_offset(point(px(0.0), px(0.0)));
                        }
                        cx.notify();
                    },
                )),
            );
        }
        Some(
            div()
                .id("agent-details-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.agent_details = None;
                    cx.notify();
                }))
                .child(
                    div()
                        .id("agent-details-drawer")
                        .absolute()
                        .right_0()
                        .top_0()
                        .bottom_0()
                        .w(px(460.0))
                        .max_w_full()
                        .bg(rgb(0x1e1e2e))
                        .border_l_1()
                        .border_color(rgb(0x45475a))
                        .shadow_lg()
                        .flex()
                        .flex_col()
                        .p_4()
                        .gap_3()
                        .on_click(cx.listener(|_, _, _, cx| cx.stop_propagation()))
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().text_lg().child("Agent details"))
                                .child(
                                    Self::settings_option("close-agent-details", "Close", false)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.agent_details = None;
                                            cx.notify();
                                        })),
                                ),
                        )
                        .child(div().text_sm().child(model.agent.display_name.clone()))
                        .child(
                            div()
                                .flex()
                                .gap_2()
                                .child(
                                    Self::settings_option(
                                        "agent-details-terminal",
                                        "Open terminal",
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, window, cx| {
                                            cx.stop_propagation();
                                            if let Some(m) = this.agent_details.take() {
                                                this.activate_sidebar_shell(
                                                    &m.agent.shell_id,
                                                    window,
                                                    cx,
                                                );
                                            }
                                        },
                                    )),
                                )
                                .child(
                                    Self::settings_control(
                                        "agent-details-refresh",
                                        if model.busy {
                                            "Refreshing…"
                                        } else {
                                            "Refresh"
                                        },
                                        false,
                                        !model.busy,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.refresh_agent_details(cx);
                                        },
                                    )),
                                )
                                .child(
                                    Self::settings_option(
                                        "agent-details-copy",
                                        if model.copied {
                                            "Copied"
                                        } else {
                                            "Copy details"
                                        },
                                        false,
                                    )
                                    .on_click(cx.listener(
                                        |this, _, _, cx| {
                                            cx.stop_propagation();
                                            if let Some(m) = &mut this.agent_details {
                                                let text = export(m);
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    text,
                                                ));
                                                m.copied = true;
                                                cx.notify();
                                            }
                                        },
                                    )),
                                ),
                        )
                        .child(tabs)
                        .child(
                            div()
                                .id("agent-details-content")
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .track_scroll(&model.scroll)
                                .child(content),
                        )
                        .child(note(model.inspection.as_ref().map_or_else(
                            || "No inspection snapshot yet".into(),
                            |i| {
                                format!(
                                    "Inspected {} · 1/2/3 tabs · Esc closes",
                                    relative_time(i.inspected_at_ms)
                                )
                            },
                        ))),
                )
                .into_any_element(),
        )
    }
}

fn export(model: &Model) -> String {
    let mut text = format!(
        "Agent: {}\nMachine: {}\nHarness: {}\nWorkspace: {}\nAgent ID: {}\nRun ID: {}\n",
        model.agent.display_name,
        model.machine,
        model.agent.integration,
        model.agent.workspace,
        model.agent.id,
        model.agent.run_id
    );
    if let Some(inspection) = &model.inspection {
        text.push_str("\nFile inventory only; not confirmed runtime capabilities.\n");
        if let Ok(json) = serde_json::to_string_pretty(inspection) {
            text.push_str(&json);
        }
    }
    if let Some(error) = &model.error {
        text.push_str(&format!("\nLast refresh failed: {error}\n"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_details_rejects_late_results_and_retains_snapshot_on_refresh_failure() {
        let inspection: Inspection = serde_json::from_value(serde_json::json!({
            "agent": {
                "id":"agent", "workspace_id":"workspace", "shell_id":"shell", "run_id":"run",
                "name":"test", "integration":"codex", "external_session_id":null,
                "started_at_ms":1, "ended_at_ms":null,
                "observation":{"revision":1,"state":"idle","authority":"lifecycle_integration","evidence":"fixture","confidence":100,"observed_at_ms":1}
            },
            "inspected_at_ms":1, "skills":[], "mcp_servers":[], "warnings":[], "truncated":false
        })).unwrap();
        let mut model = Model {
            agent: AgentChoice {
                id: "agent".into(),
                run_id: "run".into(),
                shell_name: "shell".into(),
                display_name: "test".into(),
                workspace: "workspace".into(),
                shell_id: "shell".into(),
                integration: "codex".into(),
                state: boomux::protocol::AgentState::Idle,
                updated_at_ms: 1,
                needs_attention: false,
                completed_attention: false,
                attention_revision: None,
            },
            machine: "This computer".into(),
            tab: Tab::Overview,
            expanded: None,
            inspection: None,
            error: None,
            busy: true,
            copied: false,
            generation: 2,
            task: None,
            scroll: ScrollHandle::new(),
        };
        assert!(!model.apply_result(1, Ok(inspection.clone())));
        assert!(model.busy);
        assert!(model.inspection.is_none());
        assert!(model.apply_result(2, Ok(inspection.clone())));
        assert!(!model.busy);
        model.busy = true;
        assert!(model.apply_result(2, Err("Owner unavailable".into())));
        assert_eq!(model.inspection.as_ref(), Some(&inspection));
        assert_eq!(model.error.as_deref(), Some("Owner unavailable"));
        assert!(!model.apply_result(1, Err("Stale error".into())));
        assert_eq!(model.error.as_deref(), Some("Owner unavailable"));
        assert!(model.apply_result(2, Ok(inspection)));
        assert!(model.error.is_none());
    }
}
