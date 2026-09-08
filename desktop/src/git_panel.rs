//! Git presentation and navigation; no terminal or repository lifecycle ownership.
use super::*;
use boomux::git_work::{AgentLink, Overview, Worktree};
use std::path::PathBuf;

struct AgentAssociation<'a> {
    agent: &'a AgentLink,
    associated_shell: bool,
    observed_context: bool,
}

fn agent_associations(links: &[AgentLink]) -> Vec<AgentAssociation<'_>> {
    let mut agents = std::collections::BTreeMap::new();
    for link in links {
        let association = agents
            .entry((link.id.as_str(), link.run_id.as_str()))
            .or_insert(AgentAssociation {
                agent: link,
                associated_shell: false,
                observed_context: false,
            });
        association.associated_shell |= !link.observed_context;
        association.observed_context |= link.observed_context;
    }
    agents.into_values().collect()
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct NodeOverview {
    pub id: String,
    pub label: String,
    pub local: bool,
    pub overview: Overview,
    pub error: Option<String>,
}
#[derive(Default)]
pub(crate) struct Model {
    pub open: bool,
    pub compact: bool,
    pub wide: bool,
    pub width: Option<f32>,
    pub resizing: bool,
    pub search: String,
    pub search_focused: bool,
    pub search_open: bool,
    pub filters_open: bool,
    pub current_workspace: bool,
    pub needs_attention: bool,
    pub busy: bool,
    pub nodes: Vec<NodeOverview>,
    pub expanded: HashSet<(String, PathBuf)>,
    pub task: Option<gpui::Task<()>>,
}
fn fetch(previous: Vec<NodeOverview>, refresh: bool) -> Vec<NodeOverview> {
    let result = (|| -> Result<Vec<NodeOverview>, String> {
        let client = boomux::client::Client::from_socket_path(
            boomux::client::socket_path().map_err(|e| e.to_string())?,
        );
        let snapshot = client
            .combined_node_snapshot_with_timeout(Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        let mut nodes = Vec::new();
        for node in snapshot.nodes.iter().take(16) {
            let mut item = previous
                .iter()
                .find(|p| p.id == node.node_id)
                .cloned()
                .unwrap_or_default();
            item.id = node.node_id.clone();
            item.label = node.alias.clone();
            item.local = node.local;
            let response = if node.current {
                client.git_overview(
                    (!node.local).then_some(node.node_id.as_str()),
                    refresh,
                    Duration::from_secs(2),
                )
            } else {
                item.error = Some("Node unavailable; last Git observation retained".into());
                nodes.push(item);
                continue;
            };
            match response {
                Ok(overview) => {
                    item.overview = overview;
                    item.error = None;
                }
                Err(error) => item.error = Some(error.to_string()),
            }
            nodes.push(item);
        }
        if snapshot.nodes.len() > 16 {
            nodes.push(NodeOverview {
                label: "More Nodes".into(),
                error: Some("Showing the first 16 Nodes".into()),
                ..Default::default()
            });
        }
        Ok(nodes)
    })();
    result.unwrap_or_else(|error| {
        let mut previous = previous;
        if previous.is_empty() {
            previous.push(NodeOverview {
                label: "Local Node".into(),
                ..Default::default()
            });
        }
        for node in &mut previous {
            node.error = Some(error.clone());
        }
        previous
    })
}
fn toolbar_button(
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    active: bool,
) -> Stateful<Div> {
    sidebar_header_button(id, label, glyph, active)
        .border_0()
        .text_color(rgb(if active { 0xcba6f7 } else { 0xa6adc8 }))
}

fn status(row: &Worktree) -> String {
    let Some(status) = &row.status else {
        return row
            .error
            .clone()
            .unwrap_or_else(|| "Git status unknown".into());
    };
    let local = if status.conflicts > 0 {
        format!("{} conflicts", status.conflicts)
    } else if status.staged + status.unstaged + status.untracked == 0 {
        "Clean".into()
    } else {
        [
            (status.staged, "staged"),
            (status.unstaged, "unstaged"),
            (status.untracked, "untracked"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect::<Vec<_>>()
        .join(" · ")
    };
    let push = if status.upstream.is_none() {
        "No upstream".into()
    } else if !status.divergence_known {
        "Upstream comparison unavailable".into()
    } else if status.ahead == 0 && status.behind == 0 {
        "Matches upstream".into()
    } else {
        format!("{} ahead · {} behind", status.ahead, status.behind)
    };
    format!("{local} · {push}")
}
fn attention(row: &Worktree) -> bool {
    row.error.is_some()
        || row.pr.error.is_some()
        || row.status.as_ref().is_some_and(|s| {
            s.conflicts + s.staged + s.unstaged + s.untracked + s.ahead + s.behind > 0
                || s.upstream.is_none()
                || !s.divergence_known
        })
        || row.agents.iter().any(|a| a.state == AgentState::Blocked)
        || row.pr.summary.contains("failed")
        || row.pr.summary.contains("changes requested")
}
fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Blocked => "blocked",
        AgentState::Idle => "idle",
        AgentState::Inactive => "inactive",
        AgentState::Done => "done",
        AgentState::Unknown => "unknown",
    }
}
fn age(timestamp: u64) -> String {
    if timestamp == 0 {
        return "not checked".into();
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let seconds = now.saturating_sub(timestamp) / 1000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else {
        format!("{}m ago", seconds / 60)
    }
}
impl Workspace {
    pub(crate) fn toggle_git_panel(&mut self, cx: &mut Context<Self>) {
        self.git_panel.open = !self.git_panel.open;
        if self.git_panel.open {
            self.git_panel.task = Some(cx.spawn(async move |this, cx| {
                loop {
                    let proceed = this
                        .update(cx, |this, cx| {
                            if !this.git_panel.open {
                                return false;
                            }
                            this.refresh_git_panel(false, cx);
                            true
                        })
                        .unwrap_or(false);
                    if !proceed {
                        break;
                    }
                    cx.background_executor().timer(Duration::from_secs(3)).await;
                }
            }));
        } else {
            self.git_panel.task = None;
            self.git_panel.search_focused = false;
            self.git_panel.resizing = false;
        }
        cx.notify();
    }
    fn refresh_git_panel(&mut self, refresh: bool, cx: &mut Context<Self>) {
        if self.git_panel.busy {
            return;
        }
        self.git_panel.busy = true;
        let previous = self.git_panel.nodes.clone();
        cx.spawn(async move |this, cx| {
            let nodes = cx
                .background_executor()
                .spawn(async move { fetch(previous, refresh) })
                .await;
            this.update(cx, |this, cx| {
                this.git_panel.busy = false;
                if this.git_panel.nodes != nodes {
                    this.git_panel.nodes = nodes;
                    this.git_panel.expanded.retain(|(node, path)| {
                        this.git_panel.nodes.iter().any(|n| {
                            &n.id == node && n.overview.worktrees.iter().any(|r| &r.root == path)
                        })
                    });
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }
    pub(crate) fn git_panel_width(&self, window: &Window) -> f32 {
        if !self.git_panel.open {
            return 0.0;
        }
        let available =
            (f32::from(window.viewport_size().width) - self.sidebar_width() - 240.0).max(0.0);
        available.min(self.git_panel.width.unwrap_or(if self.git_panel.wide {
            720.0
        } else {
            420.0
        }))
    }
    pub(crate) fn render_git_panel(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        if !self.git_panel.open {
            return None;
        }
        let workspace = self
            .terminals
            .get(&self.focused)
            .and_then(|p| p.shell.as_ref())
            .map(|s| s.workspace_id.as_str());
        let mut rows = Vec::new();
        let query = self.git_panel.search.to_lowercase();
        let refreshing = self
            .git_panel
            .nodes
            .iter()
            .any(|node| node.overview.refreshing);
        let show_nodes =
            self.git_panel.nodes.len() > 1 || self.git_panel.nodes.iter().any(|node| !node.local);
        for node in &self.git_panel.nodes {
            if show_nodes {
                rows.push(
                    div()
                        .text_xs()
                        .text_color(rgb(0xa6adc8))
                        .child(if node.local {
                            "Node · This machine".into()
                        } else {
                            format!("Node · {}", node.label)
                        })
                        .into_any_element(),
                );
            }
            if let Some(error) = &node.error {
                rows.push(
                    div()
                        .text_xs()
                        .text_color(rgb(0xf9e2af))
                        .child(format!("Stale / unavailable: {error}"))
                        .into_any_element(),
                );
            }
            for warning in &node.overview.warnings {
                rows.push(
                    div()
                        .text_xs()
                        .text_color(rgb(0xf9e2af))
                        .child(warning.clone())
                        .into_any_element(),
                );
            }
            if node.overview.worktrees.is_empty() && !node.overview.refreshing {
                rows.push(
                    div()
                        .text_xs()
                        .child("No Git repositories discovered from managed Shells or Agents.")
                        .into_any_element(),
                );
            }
            let mut previous_repository = None;
            let mut repository: Option<Div> = None;
            for row in &node.overview.worktrees {
                if self.git_panel.current_workspace
                    && !row
                        .shells
                        .iter()
                        .any(|s| Some(s.workspace_id.as_str()) == workspace)
                    && !row
                        .agents
                        .iter()
                        .any(|a| Some(a.workspace_id.as_str()) == workspace)
                {
                    continue;
                }
                if self.git_panel.needs_attention && !attention(row) {
                    continue;
                }
                if !query.is_empty()
                    && !format!(
                        "{} {} {} {}",
                        row.repository,
                        row.branch,
                        row.root.display(),
                        row.shells
                            .iter()
                            .map(|s| s.workspace.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                    .to_lowercase()
                    .contains(&query)
                {
                    continue;
                }
                let agents = agent_associations(&row.agents);
                if previous_repository.as_ref() != Some(&row.common_dir) {
                    if let Some(group) = repository.take() {
                        rows.push(group.into_any_element());
                    }
                    repository = Some(
                        div()
                            .flex()
                            .flex_col()
                            .min_w_0()
                            .flex_none()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x313244))
                            .overflow_hidden()
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .child(row.repository.clone()),
                            ),
                    );
                    previous_repository = Some(row.common_dir.clone());
                }
                let key = (node.id.clone(), row.root.clone());
                let expanded = self.git_panel.expanded.contains(&key);
                let toggle = key.clone();
                let mut card = div()
                    .id(SharedString::from(format!(
                        "git-row-{}-{}",
                        node.id,
                        row.root.display()
                    )))
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .gap_1()
                    .px_3()
                    .py_2()
                    .border_t_1()
                    .border_color(rgb(0x313244))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "git-expand-{}-{}",
                                node.id,
                                row.root.display()
                            )))
                            .cursor_pointer()
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .text_sm()
                            .text_color(rgb(0x89b4fa))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.git_panel.expanded.remove(&toggle) {
                                    this.git_panel.expanded.clear();
                                    this.git_panel.expanded.insert(toggle.clone());
                                }
                                cx.notify();
                            }))
                            .child(div().flex_1().min_w_0().truncate().child(format!(
                                "{} {}",
                                if expanded { "▾" } else { "▸" },
                                row.branch
                            )))
                            .when(!agents.is_empty(), |header| {
                                header.child(
                                    div().text_xs().flex_none().text_color(rgb(0xa6adc8)).child(
                                        format!(
                                            "{} {}",
                                            agents.len(),
                                            if agents.len() == 1 { "Agent" } else { "Agents" }
                                        ),
                                    ),
                                )
                            }),
                    );
                let pr = if row.pr.summary.is_empty() {
                    if row.pr.error.is_some() {
                        "PR unavailable"
                    } else {
                        "PR pending"
                    }
                } else if row.pr.summary == "No matching PR" {
                    "No PR"
                } else {
                    &row.pr.summary
                };
                let summary = format!(
                    "{} · {}{}{}",
                    status(row),
                    pr,
                    if row.pr.error.is_some() {
                        " · stale/unavailable"
                    } else {
                        ""
                    },
                    if row.pr.head.as_ref().is_some_and(|head| head != &row.head) {
                        " · PR at different commit"
                    } else {
                        ""
                    }
                );
                card = card.child(
                    div()
                        .text_xs()
                        .text_color(rgb(if attention(row) { 0xf9e2af } else { 0xa6adc8 }))
                        .child(summary),
                );
                if expanded {
                    card = card
                        .child(
                            div()
                                .mt_1()
                                .text_xs()
                                .text_color(rgb(0xa6adc8))
                                .child(row.root.display().to_string()),
                        )
                        .child(div().text_xs().text_color(rgb(0xa6adc8)).child(format!(
                            "{} Shells · {} {}",
                            row.shells.len(),
                            agents.len(),
                            if agents.len() == 1 { "Agent" } else { "Agents" }
                        )))
                        .when_some(row.pr.error.clone(), |card, error| {
                            card.child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0xf9e2af))
                                    .child(format!("PR unavailable / stale: {error}")),
                            )
                        });
                    if let Some(commit) = &row.last_commit {
                        card = card.child(div().text_xs().child(format!("Last commit: {commit}")));
                    }
                    if let Some(upstream) = row.status.as_ref().and_then(|s| s.upstream.as_ref()) {
                        card = card.child(div().text_xs().child(format!("Upstream: {upstream}")));
                    }
                    for shell in &row.shells {
                        let id = shell.id.clone();
                        let local = node.local;
                        let expected = shell.run_id.clone();
                        card = card.child(
                            div()
                                .id(SharedString::from(format!(
                                    "git-shell-{}-{}",
                                    node.id, shell.id
                                )))
                                .text_xs()
                                .text_color(rgb(0x89b4fa))
                                .cursor(if local {
                                    gpui::CursorStyle::PointingHand
                                } else {
                                    gpui::CursorStyle::Arrow
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.git_panel.search_focused = false;
                                    if local
                                        && this
                                            .boomux_shells
                                            .iter()
                                            .any(|s| s.id == id && s.run_id == expected)
                                    {
                                        this.activate_sidebar_shell(&id, window, cx);
                                    }
                                }))
                                .child(format!(
                                    "{} / {} · {}{}",
                                    shell.workspace,
                                    shell.name,
                                    if shell.live_cwd {
                                        "Shell process cwd"
                                    } else {
                                        "launch cwd"
                                    },
                                    if local { "" } else { " · remote" }
                                )),
                        );
                    }
                    for association in &agents {
                        let agent = association.agent;
                        let shell_id = agent.shell_id.clone();
                        let run_id = agent.run_id.clone();
                        let local = node.local;
                        card = card.child(
                            div()
                                .id(SharedString::from(format!(
                                    "git-agent-{}-{}-{}",
                                    node.id, agent.id, agent.run_id
                                )))
                                .text_xs()
                                .cursor(if local {
                                    gpui::CursorStyle::PointingHand
                                } else {
                                    gpui::CursorStyle::Arrow
                                })
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    if local
                                        && this.boomux_shells.iter().any(|s| {
                                            s.id == shell_id && s.run_id.as_deref() == Some(&run_id)
                                        })
                                    {
                                        this.git_panel.search_focused = false;
                                        this.activate_sidebar_shell(&shell_id, window, cx);
                                    }
                                }))
                                .child(format!(
                                    "{} · {} · {}{}",
                                    agent.name,
                                    state_label(agent.state),
                                    age(agent.observed_at_ms),
                                    match (
                                        association.associated_shell,
                                        association.observed_context
                                    ) {
                                        (true, true) => " · associated Shell · observed work here",
                                        (false, true) => " · observed work here",
                                        _ => " · associated Shell",
                                    }
                                )),
                        );
                    }
                    if !row.branches.is_empty() {
                        card =
                            card.child(div().text_xs().text_color(rgb(0xa6adc8)).child(format!(
                                "Other local branches: {}",
                                row.branches.join(", ")
                            )));
                    }
                    card = card.child(div().text_xs().text_color(rgb(0xa6adc8)).child(format!(
                        "Git: {} · PR: {} · Upstream uses local refs",
                        age(row.observed_at_ms),
                        age(row.pr.observed_at_ms)
                    )));
                    let path = row.root.display().to_string();
                    card = card.child(
                        Self::settings_option(
                            SharedString::from(format!("git-copy-{}-{}", node.id, path)),
                            "Copy path",
                            false,
                        )
                        .flex_none()
                        .self_start()
                        .py_1()
                        .px_2()
                        .border_0()
                        .text_xs()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(path.clone()))
                        })),
                    );
                    if let Some(url) = &row.pr.url {
                        let url = url.clone();
                        card = card.child(
                            Self::settings_option(
                                SharedString::from(format!(
                                    "git-pr-{}-{}",
                                    node.id,
                                    row.root.display()
                                )),
                                "Open PR",
                                false,
                            )
                            .flex_none()
                            .self_start()
                            .py_1()
                            .px_2()
                            .border_0()
                            .text_xs()
                            .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url))),
                        );
                    }
                }
                repository = repository.map(|group| group.child(card));
            }
            if let Some(group) = repository {
                rows.push(group.into_any_element());
            }
        }
        if rows.is_empty() && !refreshing {
            rows.push(
                div()
                    .text_xs()
                    .text_color(rgb(0xa6adc8))
                    .child("No worktrees match these filters.")
                    .into_any_element(),
            );
        }
        Some(
            div()
                .relative()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .w(px(self.git_panel_width(window)))
                .h_full()
                .min_w_0()
                .flex_none()
                .flex()
                .flex_col()
                .border_l_1()
                .border_color(rgb(0x45475a))
                .bg(rgb(0x181825))
                .overflow_hidden()
                .child(
                    div()
                        .id("git-resize")
                        .absolute()
                        .left_0()
                        .top_0()
                        .bottom_0()
                        .w(px(5.0))
                        .cursor(gpui::CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.git_panel.resizing = true;
                                cx.stop_propagation();
                            }),
                        ),
                )
                .child(
                    div()
                        .px_3()
                        .py_2()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .flex()
                                .items_center()
                                .gap_2()
                                .child("Git")
                                .child(
                                    div()
                                        .id("git-refresh-indicator")
                                        .size(px(6.0))
                                        .flex_none()
                                        .rounded_full()
                                        .bg(rgb(0x89b4fa))
                                        .opacity(if refreshing { 1.0 } else { 0.0 })
                                        .tooltip(|_, cx| {
                                            cx.new(|_| HeaderTooltip("Refreshing Git status"))
                                                .into()
                                        }),
                                ),
                        )
                        .child(
                            toolbar_button(
                                "git-search-toggle",
                                "Search worktrees",
                                "⌕",
                                self.git_panel.search_open,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_panel.search_open = !this.git_panel.search_open;
                                this.git_panel.search_focused = this.git_panel.search_open;
                                if !this.git_panel.search_open {
                                    this.git_panel.search.clear();
                                }
                                this.git_panel.filters_open = false;
                                cx.notify();
                            })),
                        )
                        .child(
                            toolbar_button(
                                "git-filter-toggle",
                                "Filter worktrees",
                                "≡",
                                self.git_panel.filters_open
                                    || self.git_panel.current_workspace
                                    || self.git_panel.needs_attention,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_panel.filters_open = !this.git_panel.filters_open;
                                this.git_panel.search_focused = false;
                                cx.notify();
                            })),
                        )
                        .child(
                            toolbar_button("git-refresh", "Refresh Git status", "↻", false)
                                .on_click(
                                    cx.listener(|this, _, _, cx| this.refresh_git_panel(true, cx)),
                                ),
                        )
                        .child(
                            toolbar_button(
                                "git-expand-panel",
                                "Expand or narrow panel",
                                "↔",
                                self.git_panel.wide,
                            )
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_panel.wide = !this.git_panel.wide;
                                this.git_panel.width = None;
                                cx.notify();
                            })),
                        )
                        .child(
                            toolbar_button("git-close", "Close Git panel", "×", false)
                                .on_click(cx.listener(|this, _, _, cx| this.toggle_git_panel(cx))),
                        ),
                )
                .when(self.git_panel.search_open, |panel| {
                    panel.child(
                        div()
                            .id("git-search")
                            .mx_3()
                            .mb_2()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(if self.git_panel.search_focused {
                                0x89b4fa
                            } else {
                                0x45475a
                            }))
                            .text_xs()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.git_panel.search_focused = true;
                                cx.notify();
                            }))
                            .child(if self.git_panel.search.is_empty() {
                                "Filter repository, branch, path…".into()
                            } else {
                                self.git_panel.search.clone()
                            }),
                    )
                })
                .child(
                    div()
                        .id("git-panel-scroll")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .px_3()
                        .pb_3()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .children(rows),
                )
                .when(self.git_panel.filters_open, |panel| {
                    panel.child(
                        div()
                            .absolute()
                            .top(px(42.0))
                            .right(px(12.0))
                            .w(px(220.0))
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(rgb(0x45475a))
                            .bg(rgb(0x1e1e2e))
                            .shadow_lg()
                            .occlude()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                Self::settings_option(
                                    "git-workspace-filter",
                                    if self.git_panel.current_workspace {
                                        "Current Workspace"
                                    } else {
                                        "All Workspaces"
                                    },
                                    self.git_panel.current_workspace,
                                )
                                .flex_none()
                                .py_1()
                                .text_xs()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.git_panel.current_workspace =
                                            !this.git_panel.current_workspace;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Self::settings_option(
                                    "git-attention-filter",
                                    "Needs attention",
                                    self.git_panel.needs_attention,
                                )
                                .flex_none()
                                .py_1()
                                .text_xs()
                                .on_click(cx.listener(
                                    |this, _, _, cx| {
                                        this.git_panel.needs_attention =
                                            !this.git_panel.needs_attention;
                                        cx.notify();
                                    },
                                )),
                            ),
                    )
                })
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_panel_counts_agents_once_and_preserves_association_evidence() {
        let shell = AgentLink {
            id: "agent-1".into(),
            run_id: "run-1".into(),
            workspace_id: "workspace-1".into(),
            observed_at_ms: 1,
            shell_id: "shell-1".into(),
            name: "Codex".into(),
            state: AgentState::Working,
            observed_context: false,
        };
        let context = AgentLink {
            observed_context: true,
            ..shell.clone()
        };
        for links in [
            vec![shell.clone(), context.clone(), context.clone()],
            vec![context.clone(), shell.clone()],
        ] {
            let agents = agent_associations(&links);
            assert_eq!(agents.len(), 1);
            assert!(agents[0].associated_shell);
            assert!(agents[0].observed_context);
        }
        let links = [context];
        let agents = agent_associations(&links);
        assert!(!agents[0].associated_shell);
        assert!(agents[0].observed_context);

        // Same display names do not merge distinct Agents or ShellRuns.
        let other_agent = AgentLink {
            id: "agent-2".into(),
            ..shell.clone()
        };
        let other_run = AgentLink {
            run_id: "run-2".into(),
            ..shell.clone()
        };
        assert_eq!(
            agent_associations(&[shell, other_agent, other_run]).len(),
            3
        );
    }
}
