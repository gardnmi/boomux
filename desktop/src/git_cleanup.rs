//! On-demand cleanup review; filesystem and lifecycle authority remain on the owner.
use super::*;
use boomux::{
    git_cleanup::Review,
    protocol::{HostServiceOperation as Operation, HostServiceResult as ResultValue},
};
use std::path::PathBuf;

#[derive(Clone)]
pub(crate) struct Row {
    node: Option<String>,
    machine: String,
    path: PathBuf,
    review: Option<Review>,
    error: Option<String>,
    selected: bool,
    outcome: Option<String>,
}
impl Row {
    fn eligible(&self) -> bool {
        self.error.is_none()
            && self.outcome.is_none()
            && self
                .review
                .as_ref()
                .is_some_and(|r| r.blockers.is_empty() || r.can_discard_changes())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Ready,
    Review,
    Protected,
    Removed,
}

impl Row {
    fn group(&self) -> Group {
        if self.outcome.is_some() {
            return Group::Removed;
        }
        if !self.eligible() {
            return Group::Protected;
        }
        let review = self.review.as_ref().unwrap();
        let merged = review
            .reasons
            .iter()
            .any(|reason| reason.starts_with("Merged into ") || reason == "PR merged");
        if merged && review.status.ahead == 0 && !review.has_local_changes() {
            Group::Ready
        } else {
            Group::Review
        }
    }
    fn summary(&self) -> String {
        if self.outcome.is_some() {
            return "Branch retained".into();
        }
        if self.error.is_some() {
            return "Could not verify · expand for details".into();
        }
        let Some(r) = &self.review else {
            return "Not scanned".into();
        };
        if r.blockers.iter().any(|b| b == "Primary worktree") {
            return "Primary worktree".into();
        }
        if let Some(blocker) = r.blockers.first() {
            return blocker.clone();
        }
        if r.status.ahead > 0 {
            return format!("{} unpushed commits", r.status.ahead);
        }
        r.reasons
            .iter()
            .find(|reason| reason.starts_with("Merged into ") || reason.as_str() == "PR merged")
            .or_else(|| r.reasons.first())
            .map(|s| s.replace("refs/remotes/", "").replace("refs/heads/", ""))
            .unwrap_or_else(|| "No merge confirmed".into())
    }
}

#[derive(Default)]
pub(crate) struct Model {
    rows: Vec<Row>,
    busy: bool,
    cancelled: bool,
    confirm: bool,
    discard_acknowledged: bool,
    picker: bool,
    message: String,
    scroll: ScrollHandle,
    show_review: bool,
    show_protected: bool,
    expanded: Option<(Option<String>, PathBuf)>,
}
impl Model {
    fn can_remove(&self) -> bool {
        if !self.confirm || self.busy || self.picker {
            return false;
        }
        let mut any = false;
        for row in self.rows.iter().filter(|r| r.selected && r.eligible()) {
            any = true;
            if row.review.as_ref().is_some_and(|r| r.has_local_changes())
                && !self.discard_acknowledged
            {
                return false;
            }
        }
        any
    }
}

fn request(node: Option<&str>, operation: Operation) -> Result<ResultValue, String> {
    let client = boomux::client::Client::from_socket_path(
        boomux::client::socket_path().map_err(|e| e.to_string())?,
    );
    client
        .git_cleanup(node, operation)
        .map_err(|e| e.to_string())
}
fn size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GiB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else {
        format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}
fn local_work(review: &Review) -> String {
    let s = &review.status;
    let dirty = s.staged + s.unstaged + s.untracked + s.conflicts;
    let changes = if dirty == 0 {
        "Clean".into()
    } else {
        format!(
            "{} staged · {} unstaged · {} untracked · {} conflicts",
            s.staged, s.unstaged, s.untracked, s.conflicts
        )
    };
    let upstream = if s.divergence_known {
        format!("{} ahead · {} behind upstream", s.ahead, s.behind)
    } else if s.upstream.is_some() {
        "Upstream comparison unavailable".into()
    } else {
        "No upstream; unpushed count unknown".into()
    };
    format!(
        "{changes} · {upstream} · {} ignored entries",
        review.ignored_entries
    )
}

impl Workspace {
    pub(crate) fn open_git_cleanup(&mut self, cx: &mut Context<Self>) {
        if self.git_panel.cleanup.is_some() {
            return;
        }
        self.git_panel.search_focused = false;
        self.release_layout_leader(cx);
        self.git_panel.cleanup_generation = self.git_panel.cleanup_generation.wrapping_add(1);
        let mut rows = Vec::new();
        for node in &self.git_panel.nodes {
            for worktree in &node.overview.worktrees {
                if rows.len() == 128 {
                    break;
                }
                rows.push(Row {
                    node: (!node.local).then(|| node.id.clone()),
                    machine: node.label.clone(),
                    path: worktree.root.clone(),
                    review: None,
                    error: None,
                    selected: false,
                    outcome: None,
                });
            }
        }
        self.git_panel.cleanup = Some(Model {
            rows,
            ..Default::default()
        });
        self.scan_git_cleanup(cx);
    }

    pub(crate) fn cleanup_key_down(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        if event.keystroke.key == "escape" {
            let state = self.git_panel.cleanup.as_mut().unwrap();
            if state.confirm {
                state.confirm = false;
            } else if state.busy {
                state.cancelled = true;
            } else {
                self.git_panel.cleanup = None;
            }
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn scan_git_cleanup(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.git_panel.cleanup else {
            return;
        };
        if state.busy || state.picker {
            return;
        }
        state.busy = true;
        state.cancelled = false;
        state.confirm = false;
        state.discard_acknowledged = false;
        state.message = "Scanning worktrees…".into();
        for row in state.rows.iter_mut().filter(|r| r.outcome.is_none()) {
            row.selected = false;
            row.review = None;
            row.error = None;
        }
        let pending: Vec<_> = state
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.outcome.is_none())
            .map(|(i, r)| (i, r.node.clone(), r.path.clone()))
            .collect();
        let generation = self.git_panel.cleanup_generation;
        cx.spawn(async move |this, cx| {
            let total = pending.len();
            for (position, (index, node, path)) in pending.into_iter().enumerate() {
                let proceed = this
                    .update(cx, |this, cx| {
                        if this.git_panel.cleanup_generation != generation {
                            return false;
                        }
                        let Some(state) = &mut this.git_panel.cleanup else {
                            return false;
                        };
                        if state.cancelled {
                            return false;
                        }
                        state.message = format!("Scanning {} of {total}…", position + 1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !proceed {
                    break;
                }
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        request(node.as_deref(), Operation::InspectCleanupWorktree { path })
                    })
                    .await;
                if this
                    .update(cx, |this, cx| {
                        if this.git_panel.cleanup_generation != generation {
                            return;
                        }
                        if let Some(state) = &mut this.git_panel.cleanup {
                            match result {
                                Ok(ResultValue::CleanupWorktree { review }) => {
                                    state.rows[index].review = Some(review)
                                }
                                Ok(_) => {
                                    state.rows[index].error =
                                        Some("Unexpected cleanup response".into())
                                }
                                Err(error) => state.rows[index].error = Some(error),
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    return;
                }
            }
            this.update(cx, |this, cx| {
                if this.git_panel.cleanup_generation != generation {
                    return;
                }
                if let Some(state) = &mut this.git_panel.cleanup {
                    state.busy = false;
                    state.message = if state.cancelled {
                        "Scan stopped. Unscanned worktrees cannot be selected.".into()
                    } else if state.rows.is_empty() {
                        "Choose a repository to find abandoned worktrees.".into()
                    } else {
                        String::new()
                    };
                    state.rows.sort_by_key(|r| {
                        std::cmp::Reverse(r.review.as_ref().map_or(0, |v| v.bytes))
                    });
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn choose_cleanup_repository(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.git_panel.cleanup else {
            return;
        };
        if state.busy || state.picker {
            return;
        }
        state.picker = true;
        let generation = self.git_panel.cleanup_generation;
        let picker = cx.prompt_for_paths(gpui::PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose local Git repository".into()),
        });
        cx.spawn(async move |this, cx| {
            let selection = picker
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r.map_err(|e| e.to_string()));
            let path = match selection {
                Ok(Some(paths)) => paths.into_iter().next(),
                Ok(None) => None,
                Err(error) => {
                    this.update(cx, |this, cx| {
                        if this.git_panel.cleanup_generation == generation
                            && let Some(state) = &mut this.git_panel.cleanup
                        {
                            state.picker = false;
                            state.message = error;
                            cx.notify();
                        }
                    })
                    .ok();
                    return;
                }
            };
            let result = if let Some(path) = path {
                Some(
                    cx.background_executor()
                        .spawn(
                            async move { request(None, Operation::ListCleanupWorktrees { path }) },
                        )
                        .await,
                )
            } else {
                None
            };
            this.update(cx, |this, cx| {
                if this.git_panel.cleanup_generation != generation {
                    return;
                }
                let Some(state) = &mut this.git_panel.cleanup else {
                    return;
                };
                state.picker = false;
                match result {
                    Some(Ok(ResultValue::CleanupWorktrees { paths })) => {
                        state.rows.clear();
                        for path in paths {
                            if state.rows.len() >= 128 {
                                break;
                            }
                            if !state
                                .rows
                                .iter()
                                .any(|r| r.node.is_none() && r.path == path)
                            {
                                state.rows.push(Row {
                                    node: None,
                                    machine: "This machine".into(),
                                    path,
                                    review: None,
                                    error: None,
                                    selected: false,
                                    outcome: None,
                                });
                            }
                        }
                        this.scan_git_cleanup(cx);
                    }
                    Some(Err(error)) => state.message = error,
                    Some(Ok(_)) => state.message = "Unexpected repository response".into(),
                    None => {}
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn remove_cleanup_selection(&mut self, cx: &mut Context<Self>) {
        let Some(state) = &mut self.git_panel.cleanup else {
            return;
        };
        if !state.can_remove() {
            return;
        }
        let selected: Vec<_> = state
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.selected && r.eligible())
            .map(|(i, r)| {
                (
                    i,
                    r.node.clone(),
                    r.review.as_ref().unwrap().target.clone(),
                    r.review.as_ref().unwrap().has_local_changes(),
                )
            })
            .collect();
        if selected.is_empty() {
            return;
        }
        state.busy = true;
        state.cancelled = false;
        state.confirm = false;
        let generation = self.git_panel.cleanup_generation;
        cx.spawn(async move |this, cx| {
            let total = selected.len();
            for (position, (index, node, expected, discard_changes)) in
                selected.into_iter().enumerate()
            {
                let proceed = this
                    .update(cx, |this, cx| {
                        if this.git_panel.cleanup_generation != generation {
                            return false;
                        }
                        let Some(state) = &mut this.git_panel.cleanup else {
                            return false;
                        };
                        if state.cancelled {
                            return false;
                        }
                        state.message = format!("Removing {} of {total}…", position + 1);
                        cx.notify();
                        true
                    })
                    .unwrap_or(false);
                if !proceed {
                    break;
                }
                let result = cx
                    .background_executor()
                    .spawn(async move {
                        request(
                            node.as_deref(),
                            Operation::RemoveCleanupWorktree {
                                expected,
                                discard_changes,
                            },
                        )
                    })
                    .await;
                this.update(cx, |this, cx| {
                    if this.git_panel.cleanup_generation != generation {
                        return;
                    }
                    if let Some(state) = &mut this.git_panel.cleanup {
                        let row = &mut state.rows[index];
                        row.selected = false;
                        match result {
                            Ok(ResultValue::CleanupRemoved { .. }) => {
                                row.outcome = Some("Removed directory · branch retained".into())
                            }
                            Ok(_) => {
                                row.error = Some(
                                    "Unexpected response; outcome unknown. Scan before retrying."
                                        .into(),
                                )
                            }
                            Err(error) => {
                                row.error = Some(format!(
                                    "Removal not confirmed: {error}. Scan before retrying."
                                ))
                            }
                        }
                    }
                    cx.notify();
                })
                .ok();
            }
            this.update(cx, |this, cx| {
                if this.git_panel.cleanup_generation != generation {
                    return;
                }
                if let Some(state) = &mut this.git_panel.cleanup {
                    state.busy = false;
                    state.message = "Cleanup finished. Branches retained.".into();
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn cleanup_row(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> Stateful<Div> {
        let state = self.git_panel.cleanup.as_ref().unwrap();
        let enabled = row.eligible() && !state.busy && !state.picker && !state.confirm;
        let key = (row.node.clone(), row.path.clone());
        let expanded = state.expanded.as_ref() == Some(&key);
        let toggle_key = key.clone();
        let branch = row
            .review
            .as_ref()
            .map(|r| r.target.branch.clone())
            .unwrap_or_else(|| {
                row.path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            });
        let repository = row
            .review
            .as_ref()
            .and_then(|r| r.target.common_dir.parent())
            .and_then(|p| p.file_name())
            .unwrap_or_default()
            .to_string_lossy();
        let color = match row.group() {
            Group::Ready => 0xa6e3a1,
            Group::Review => 0xf9e2af,
            Group::Protected => 0x7f849c,
            Group::Removed => 0xa6e3a1,
        };
        let mut card = div()
            .id(SharedString::from(format!("cleanup-row-{index}")))
            .flex_none()
            .min_w_0()
            .flex()
            .flex_col()
            .border_b_1()
            .border_color(rgb(0x313244))
            .when(row.selected, |d| d.bg(rgb(0x252537)))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .min_w_0()
                    .child(
                        Self::settings_control(
                            SharedString::from(format!("cleanup-select-{index}")),
                            if row.selected {
                                "☑"
                            } else if row.eligible() {
                                "☐"
                            } else {
                                "—"
                            },
                            row.selected,
                            enabled,
                        )
                        .flex_none()
                        .w(px(26.0))
                        .border_0()
                        .bg(gpui::transparent_black())
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(state) = &mut this.git_panel.cleanup
                                && !state.busy
                                && !state.picker
                                && !state.confirm
                                && let Some(row) = state
                                    .rows
                                    .iter_mut()
                                    .find(|r| (r.node.clone(), r.path.clone()) == toggle_key)
                                && row.eligible()
                            {
                                row.selected = !row.selected;
                            }
                            cx.notify();
                        })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("cleanup-detail-{index}")))
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(state) = &mut this.git_panel.cleanup {
                                    state.expanded = if state.expanded.as_ref() == Some(&key) {
                                        None
                                    } else {
                                        Some(key.clone())
                                    };
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().text_sm().truncate().child(branch))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(rgb(0x7f849c))
                                            .truncate()
                                            .child(format!("{repository} · {}", row.machine)),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .max_w(px(260.0))
                                    .truncate()
                                    .text_color(rgb(color))
                                    .child(row.summary()),
                            )
                            .child(
                                div()
                                    .w(px(78.0))
                                    .flex_none()
                                    .text_right()
                                    .text_xs()
                                    .text_color(rgb(0xa6adc8))
                                    .child(row.review.as_ref().map_or("—".into(), |r| {
                                        format!(
                                            "{}{}",
                                            if r.size_complete { "" } else { "≥ " },
                                            size(r.bytes)
                                        )
                                    })),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(rgb(0x7f849c))
                                    .child(if expanded { "▾" } else { "▸" }),
                            ),
                    ),
            );
        if expanded {
            let mut details = div()
                .pl(px(44.0))
                .pr_3()
                .pb_3()
                .flex()
                .flex_col()
                .gap_1()
                .text_xs()
                .text_color(rgb(0xa6adc8))
                .child(row.path.display().to_string());
            if let Some(review) = &row.review {
                details = details.child(local_work(review));
                if !review.pr.is_empty() {
                    details = details.child(review.pr.clone());
                }
                for activity in &review.activity {
                    details = details.child(activity.clone());
                }
                for blocker in &review.blockers {
                    details = details.child(div().text_color(rgb(0xf9e2af)).child(blocker.clone()));
                }
            }
            if let Some(error) = &row.error {
                details = details.child(div().text_color(rgb(0xf38ba8)).child(error.clone()));
            }
            if let Some(outcome) = &row.outcome {
                details = details.child(outcome.clone());
            }
            card = card.child(details);
        }
        card
    }

    pub(crate) fn git_cleanup_overlay(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let state = self.git_panel.cleanup.as_ref()?;
        let selected: Vec<_> = state
            .rows
            .iter()
            .filter(|r| r.selected && r.eligible())
            .collect();
        let bytes = selected.iter().fold(0u64, |sum, r| {
            sum.saturating_add(r.review.as_ref().unwrap().bytes)
        });
        let discard_count = selected
            .iter()
            .filter(|r| r.review.as_ref().is_some_and(|r| r.has_local_changes()))
            .count();
        let busy = state.busy || state.picker;
        let mut sections = Vec::new();
        for group in [
            Group::Ready,
            Group::Review,
            Group::Protected,
            Group::Removed,
        ] {
            let mut rows: Vec<_> = state
                .rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.group() == group)
                .collect();
            rows.sort_by_key(|(_, r)| std::cmp::Reverse(r.review.as_ref().map_or(0, |v| v.bytes)));
            if rows.is_empty() && group != Group::Ready {
                continue;
            }
            let open = match group {
                Group::Ready | Group::Removed => true,
                Group::Review => {
                    state.show_review || state.confirm && rows.iter().any(|(_, r)| r.selected)
                }
                Group::Protected => state.show_protected,
            };
            let total = rows.iter().fold(0u64, |sum, (_, r)| {
                sum.saturating_add(r.review.as_ref().map_or(0, |v| v.bytes))
            });
            let title = match group {
                Group::Ready => "Ready for cleanup",
                Group::Review => "Needs review",
                Group::Protected => "Protected",
                Group::Removed => "Removed",
            };
            let mut header = div().flex().items_center().gap_2().py_2().text_xs().child(
                div()
                    .id(SharedString::from(format!("cleanup-group-{group:?}")))
                    .flex_1()
                    .cursor_pointer()
                    .text_color(rgb(if group == Group::Ready {
                        0xa6e3a1
                    } else {
                        0xa6adc8
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = &mut this.git_panel.cleanup {
                            match group {
                                Group::Review => s.show_review = !s.show_review,
                                Group::Protected => s.show_protected = !s.show_protected,
                                _ => {}
                            }
                        }
                        cx.notify();
                    }))
                    .child(format!(
                        "{} {title}  ·  {}{}",
                        if open { "▾" } else { "▸" },
                        rows.len(),
                        if total > 0 {
                            format!("  ·  ~{}", size(total))
                        } else {
                            String::new()
                        }
                    )),
            );
            if group == Group::Ready && !rows.is_empty() && !state.confirm {
                let all_selected = rows.iter().all(|(_, r)| r.selected);
                header = header.child(
                    Self::settings_control(
                        "cleanup-select-ready",
                        if all_selected { "Clear" } else { "Select all" },
                        false,
                        !busy,
                    )
                    .flex_none()
                    .h(px(24.0))
                    .border_0()
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(s) = &mut this.git_panel.cleanup
                            && !s.busy
                            && !s.picker
                            && !s.confirm
                        {
                            for row in &mut s.rows {
                                if row.group() == Group::Ready {
                                    row.selected = !all_selected;
                                }
                            }
                        }
                        cx.notify();
                    })),
                );
            }
            let mut section = div().flex_none().min_w_0().flex().flex_col().child(header);
            if open {
                if rows.is_empty() {
                    section = section.child(div().px_2().py_3().text_sm().text_color(rgb(0x7f849c))
                        .child(if state.busy { "Checking worktrees…" } else { "No merged, inactive worktrees ready. Expand Needs review to inspect other candidates." }));
                }
                for (index, row) in rows {
                    section = section.child(self.cleanup_row(index, row, cx));
                }
            }
            sections.push(section);
        }
        let mut actions = div().flex().items_center().gap_2();
        if state.busy {
            actions = actions.child(
                Self::settings_option("cleanup-stop", "Stop", false)
                    .flex_none()
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(s) = &mut this.git_panel.cleanup {
                            s.cancelled = true;
                            s.message = "Stopping after this worktree…".into();
                        }
                        cx.notify();
                    })),
            );
        } else if state.confirm {
            actions = actions
                .child(
                    Self::settings_option("cleanup-back", "Back", false)
                        .flex_none()
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(s) = &mut this.git_panel.cleanup {
                                s.confirm = false;
                            }
                            cx.notify();
                        })),
                )
                .child(
                    Self::settings_control(
                        "cleanup-confirm",
                        if discard_count > 0 {
                            "Discard changes and remove"
                        } else {
                            "Remove directories"
                        },
                        true,
                        state.can_remove(),
                    )
                    .flex_none()
                    .when(discard_count > 0, |d| d.text_color(rgb(0xf38ba8)))
                    .when(discard_count > 0 && !state.discard_acknowledged, |d| {
                        d.opacity(0.4)
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.remove_cleanup_selection(cx))),
                );
        } else {
            actions = actions
                .child(
                    Self::settings_option("cleanup-close", "Close", false)
                        .flex_none()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.git_panel.cleanup = None;
                            cx.notify();
                        })),
                )
                .child(
                    Self::settings_control(
                        "cleanup-review",
                        "Review removal…",
                        !selected.is_empty(),
                        !selected.is_empty() && !busy,
                    )
                    .flex_none()
                    .when(selected.is_empty() || busy, |d| d.opacity(0.4))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(s) = &mut this.git_panel.cleanup
                            && !s.busy
                            && !s.picker
                            && s.rows.iter().any(|r| r.selected && r.eligible())
                        {
                            s.confirm = true;
                            s.discard_acknowledged = false;
                        }
                        cx.notify();
                    })),
                );
        }
        Some(div().id("cleanup-backdrop").absolute().occlude().inset_0().size_full().p_4()
            .flex().items_center().justify_center().bg(rgba(0x000000aa))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().id("cleanup-dialog").w(px(840.0)).max_w_full().h(px(560.0)).max_h_full()
                .p_4().rounded_lg().border_1().border_color(rgb(0x45475a)).bg(rgb(0x1e1e2e))
                .flex().flex_col().gap_2().shadow_lg()
                .child(div().flex_none().flex().items_center().gap_2()
                    .child(div().flex_1().text_lg().font_weight(gpui::FontWeight::BOLD).child("Clean up worktrees"))
                    .when(!state.confirm, |d| d
                        .child(Self::settings_control("cleanup-browse", "Choose repository…", false, !busy).flex_none().border_0()
                            .on_click(cx.listener(|this, _, _, cx| this.choose_cleanup_repository(cx))))
                        .child(Self::settings_control("cleanup-scan", "Rescan", false, !busy).flex_none().border_0()
                            .on_click(cx.listener(|this, _, _, cx| this.scan_git_cleanup(cx))))))
                .when(!state.message.is_empty(), |d| d.child(div().flex_none().text_xs().text_color(rgb(0xa6adc8)).child(state.message.clone())))
                .child(div().id("cleanup-scroll").track_scroll(&state.scroll).flex_1().min_h_0().overflow_y_scroll().flex().flex_col().gap_2().children(sections))
                .when(state.confirm, |d| d.child(div().flex_none().p_3().text_sm().bg(rgb(0x313244)).child(format!(
                    "Remove {} directories (~{})? Ignored files, including .env and build output, will be deleted. Branches and commits are kept. Shells and panes remain. Stop other tools using these directories first.", selected.len(), size(bytes)))))
                .when(state.confirm && discard_count > 0, |d| d.child(
                    Self::settings_option("cleanup-discard-ack", format!("{} Permanently discard uncommitted and untracked files in {discard_count} selected worktree(s)", if state.discard_acknowledged { "☑" } else { "☐" }), state.discard_acknowledged)
                        .flex_none().h_auto().py_2().text_color(rgb(0xf38ba8))
                        .on_click(cx.listener(|this, _, _, cx| {
                            if let Some(s) = &mut this.git_panel.cleanup && s.confirm && !s.busy { s.discard_acknowledged = !s.discard_acknowledged; }
                            cx.notify();
                        }))))
                .child(div().flex_none().pt_2().border_t_1().border_color(rgb(0x313244)).flex().items_center().gap_2()
                    .child(div().flex_1().text_xs().text_color(rgb(0xa6adc8)).child(format!("{} selected · ~{}", selected.len(), size(bytes))))
                    .child(actions)))
            .into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cleanup_unknown_or_failed_rows_cannot_be_selected() {
        let mut row = Row {
            node: None,
            machine: String::new(),
            path: PathBuf::new(),
            review: None,
            error: None,
            selected: true,
            outcome: None,
        };
        assert!(!row.eligible());
        row.review = Some(Review {
            target: boomux::git_cleanup::Target {
                root: "/tmp/tree".into(),
                common_dir: "/tmp/repo/.git".into(),
                git_dir: "/tmp/repo/.git/worktrees/tree".into(),
                branch: "feature".into(),
                head: "abc".into(),
                device: 1,
                inode: 2,
            },
            reasons: vec![],
            blockers: vec![],
            status: boomux::git_work::Status {
                ahead: 2,
                divergence_known: true,
                ..Default::default()
            },
            activity: vec![],
            bytes: 100,
            size_complete: true,
            ignored_entries: 0,
            pr: String::new(),
        });
        assert!(row.eligible()); // Unpushed commits remain on the retained branch.
        assert_eq!(row.group(), Group::Review);
        row.review.as_mut().unwrap().reasons = vec!["PR closed".into()];
        row.review.as_mut().unwrap().status.ahead = 0;
        assert_eq!(row.group(), Group::Review); // Closed is not merged.
        row.review.as_mut().unwrap().reasons = vec!["Upstream ref gone".into()];
        assert_eq!(row.group(), Group::Review); // Gone is not merged either.
        row.review.as_mut().unwrap().reasons = vec!["Merged into refs/heads/main".into()];
        assert_eq!(row.group(), Group::Ready);
        row.review.as_mut().unwrap().status.ahead = 1;
        assert_eq!(row.group(), Group::Review);
        row.review.as_mut().unwrap().status.ahead = 0;
        row.review.as_mut().unwrap().status.untracked = 1;
        row.review.as_mut().unwrap().blockers = vec!["Local changes or untracked files".into()];
        assert!(row.eligible());
        assert_eq!(row.group(), Group::Review); // A merged branch with local work is never Ready.
        let mut model = Model {
            rows: vec![row.clone()],
            confirm: true,
            ..Default::default()
        };
        assert!(!model.can_remove());
        model.discard_acknowledged = true;
        assert!(model.can_remove());
        model.rows[0]
            .review
            .as_mut()
            .unwrap()
            .blockers
            .push("Locked worktree".into());
        assert!(!model.can_remove());
        row.review.as_mut().unwrap().status.untracked = 0;
        row.review.as_mut().unwrap().blockers.clear();
        row.error = Some("lost channel".into());
        assert!(!row.eligible()); // An old successful scan cannot authorize retry.
        assert_eq!(row.group(), Group::Protected);
        row.error = None;
        row.review
            .as_mut()
            .unwrap()
            .blockers
            .push("running Shell".into());
        assert!(!row.eligible());
        row.review.as_mut().unwrap().blockers.clear();
        row.outcome = Some("removed".into());
        assert!(!row.eligible());
    }
}
