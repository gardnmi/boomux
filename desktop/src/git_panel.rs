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
    pub height: Option<f32>,
    pub resizing: bool,
    pub search: String,
    pub search_focused: bool,
    pub search_open: bool,
    pub busy: bool,
    pub nodes: Vec<NodeOverview>,
    pub expanded: HashSet<(String, PathBuf)>,
    pub task: Option<gpui::Task<()>>,
    pub scroll_handle: ScrollHandle,
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
                item.error = Some("Machine unavailable; last Git observation retained".into());
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
                label: "More machines".into(),
                error: Some("Showing the first 16 machines".into()),
                ..Default::default()
            });
        }
        Ok(nodes)
    })();
    result.unwrap_or_else(|error| {
        let mut previous = previous;
        if previous.is_empty() {
            previous.push(NodeOverview {
                label: "This machine".into(),
                local: true,
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

fn local_status(row: &Worktree) -> (String, u32) {
    let Some(status) = &row.status else {
        return (
            row.error.clone().unwrap_or_else(|| "Status unknown".into()),
            0xf9e2af,
        );
    };
    if status.conflicts > 0 {
        (format!("{} conflicts", status.conflicts), 0xf38ba8)
    } else if status.staged + status.unstaged + status.untracked == 0 {
        ("Clean".into(), 0xa6e3a1)
    } else {
        (
            [
                (status.staged, "staged"),
                (status.unstaged, "unstaged"),
                (status.untracked, "untracked"),
            ]
            .into_iter()
            .filter(|(count, _)| *count > 0)
            .map(|(count, label)| format!("{count} {label}"))
            .collect::<Vec<_>>()
            .join(" · "),
            0xf9e2af,
        )
    }
}

fn upstream_status(row: &Worktree) -> (String, u32) {
    let Some(status) = &row.status else {
        return ("Upstream unknown".into(), 0x7f849c);
    };
    if status.upstream.is_none() {
        ("No upstream".into(), 0xf9e2af)
    } else if !status.divergence_known {
        ("Upstream unknown".into(), 0xf9e2af)
    } else if status.ahead == 0 && status.behind == 0 {
        ("Up to date".into(), 0xa6e3a1)
    } else {
        (
            format!("{} ahead · {} behind", status.ahead, status.behind),
            0xf9e2af,
        )
    }
}

fn pr_status(row: &Worktree) -> Option<(String, u32)> {
    if row.pr.error.is_some()
        || row.pr.summary.is_empty()
        || matches!(
            row.pr.summary.as_str(),
            "No matching PR" | "No branch PR lookup"
        )
        || row.pr.summary.starts_with("PR lookup unavailable")
        || row.pr.summary.starts_with("PR lookup truncated")
    {
        return None;
    }
    let color = if row.pr.summary.contains("failed") || row.pr.summary.contains("changes requested")
    {
        0xf38ba8
    } else if row.pr.summary.contains("pending") || row.pr.summary.contains("unknown") {
        0xf9e2af
    } else {
        0xa6e3a1
    };
    Some((row.pr.summary.clone(), color))
}

fn status_item(label: String, color: u32) -> Div {
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .text_color(rgb(0xa6adc8))
        .child(
            div()
                .size(px(6.0))
                .flex_none()
                .rounded_full()
                .bg(rgb(color)),
        )
        .child(div().min_w_0().truncate().child(label))
}

fn detail_row(label: &'static str, value: String) -> Div {
    div()
        .flex()
        .gap_3()
        .min_w_0()
        .text_xs()
        .child(
            div()
                .w(px(58.0))
                .flex_none()
                .text_color(rgb(0x7f849c))
                .child(label),
        )
        .child(div().min_w_0().text_color(rgb(0xa6adc8)).child(value))
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
        self.sidebar_visible = true;
        self.settings_open = false;
        self.select_git_tab(true, cx);
    }

    pub(crate) fn select_git_tab(&mut self, git: bool, cx: &mut Context<Self>) {
        self.nodes_open = false;
        if self.git_panel.open == git {
            self.save_settings();
            cx.notify();
            return;
        }
        self.git_panel.open = git;
        self.reconcile_sidebar_item();
        self.save_settings();
        if self.git_panel.open {
            self.refresh_git_panel(false, cx);
            self.git_panel.task = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(3)).await;
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
        cx.notify();
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
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }
    pub(crate) fn render_git_controls(&self, cx: &mut Context<Self>) -> Div {
        let refreshing = self.git_panel.busy
            || self
                .git_panel
                .nodes
                .iter()
                .any(|node| node.overview.refreshing);
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                toolbar_button(
                    "git-search-toggle",
                    "Search worktrees",
                    "⌕",
                    self.git_panel.search_open,
                )
                .button_chrome()
                .on_click(cx.listener(|this, _, _, cx| {
                    this.git_panel.search_open = !this.git_panel.search_open;
                    this.git_panel.search_focused = this.git_panel.search_open;
                    if !this.git_panel.search_open {
                        this.git_panel.search.clear();
                    }
                    cx.notify();
                })),
            )
            .child(
                toolbar_button(
                    "git-refresh",
                    if refreshing {
                        "Refreshing Git status…"
                    } else {
                        "Refresh Git status"
                    },
                    "↻",
                    refreshing,
                )
                .button_chrome()
                .on_click(cx.listener(|this, _, _, cx| this.refresh_git_panel(true, cx))),
            )
    }

    pub(crate) fn render_git_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.git_panel.open {
            return None;
        }
        let mut rows = Vec::new();
        let query = self.git_panel.search.to_lowercase();
        let refreshing = self.git_panel.busy
            || self
                .git_panel
                .nodes
                .iter()
                .any(|node| node.overview.refreshing);
        let initial_loading = refreshing
            && self
                .git_panel
                .nodes
                .iter()
                .all(|node| node.overview.worktrees.is_empty() && node.error.is_none());
        if initial_loading {
            rows.push(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .items_center()
                    .justify_center()
                    .gap_2()
                    .text_color(rgb(0xa6adc8))
                    .child(div().size_2().rounded_full().bg(rgb(0x89b4fa)))
                    .child(div().text_sm().child("Loading repositories…"))
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(0x7f849c))
                            .child("Inspecting managed Shells and Agents"),
                    )
                    .into_any_element(),
            );
        }
        let show_nodes =
            self.git_panel.nodes.len() > 1 || self.git_panel.nodes.iter().any(|node| !node.local);
        for node in &self.git_panel.nodes {
            if show_nodes {
                rows.push(
                    div()
                        .text_xs()
                        .text_color(rgb(0xa6adc8))
                        .child(if node.local {
                            "This machine".into()
                        } else {
                            format!("Remote · {}", node.label)
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
                            .gap_1()
                            .overflow_hidden()
                            .child(
                                div()
                                    .px_3()
                                    .pt_1()
                                    .pb_1()
                                    .text_sm()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(rgb(0xa6adc8))
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
                    .py_1()
                    .ml_3()
                    .border_l_1()
                    .border_color(rgb(if expanded { 0x89b4fa } else { 0x313244 }))
                    .when(expanded, |row| row.bg(rgb(0x1e1e2e)))
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "git-expand-{}-{}",
                                node.id,
                                row.root.display()
                            )))
                            .cursor_pointer()
                            .rounded_sm()
                            .hover(|header| header.bg(rgb(0x29293d)))
                            .flex()
                            .items_center()
                            .gap_2()
                            .min_w_0()
                            .text_sm()
                            .font_weight(gpui::FontWeight::NORMAL)
                            .button_chrome()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if !this.git_panel.expanded.remove(&toggle) {
                                    this.git_panel.expanded.clear();
                                    this.git_panel.expanded.insert(toggle.clone());
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w(px(10.0))
                                    .flex_none()
                                    .text_color(rgb(0x6c7086))
                                    .child(if expanded { "▾" } else { "▸" }),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .child(row.branch.clone()),
                            )
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
                let local = local_status(row);
                let upstream = upstream_status(row);
                let pr = pr_status(row);
                card = card.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_3()
                        .min_w_0()
                        .child(status_item(local.0, local.1))
                        .child(status_item(upstream.0, upstream.1))
                        .when_some(pr, |statuses, (label, color)| {
                            statuses.child(status_item(label, color))
                        }),
                );
                if row.pr.error.is_none()
                    && !row.pr.summary.is_empty()
                    && row.pr.summary != "No matching PR"
                    && row.pr.head.as_ref().is_some_and(|head| head != &row.head)
                {
                    card = card.child(
                        div()
                            .text_xs()
                            .text_color(rgb(0xf9e2af))
                            .child("PR status is for a different commit"),
                    );
                }
                if expanded {
                    card = card.child(
                        div()
                            .mt_2()
                            .child(detail_row("Path", row.root.display().to_string())),
                    );
                    if let Some(commit) = &row.last_commit {
                        card = card.child(detail_row("Commit", commit.clone()));
                    }
                    if let Some(upstream) = row.status.as_ref().and_then(|s| s.upstream.as_ref()) {
                        card = card.child(detail_row("Upstream", upstream.clone()));
                    }
                    if !row.shells.is_empty() || !agents.is_empty() {
                        card = card.child(
                            div()
                                .mt_2()
                                .text_xs()
                                .text_color(rgb(0x7f849c))
                                .child("Linked activity"),
                        );
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
                                .button_chrome()
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
                                .button_chrome()
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
                    card = card.child(div().mt_2().text_xs().text_color(rgb(0x7f849c)).child(
                        format!(
                            "Updated {} · Local upstream refs{}",
                            age(row.observed_at_ms),
                            if pr_status(row).is_some() {
                                format!(" · PR checked {}", age(row.pr.observed_at_ms))
                            } else {
                                String::new()
                            }
                        ),
                    ));
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
                        .button_chrome()
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
                            .button_chrome()
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
                    .child("No worktrees match this search.")
                    .into_any_element(),
            );
        }
        Some(
            div()
                .relative()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .w_full()
                .flex_1()
                .min_h_0()
                .min_w_0()
                .flex()
                .flex_col()
                .bg(rgb(0x181825))
                .overflow_hidden()
                .when(self.git_panel.search_open, |panel| {
                    panel.child(
                        div()
                            .id("git-search")
                            .role(gpui::Role::SearchInput)
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
                            .button_chrome()
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
                        .track_scroll(&self.git_panel.scroll_handle)
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
