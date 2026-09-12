//! Conversion between live pane entities and durable presentation metadata.
use crate::{
    layout_state::{Arrangement, Floating, Pane, Tree},
    *,
};

/// Ignore synthetic/stationary pointer enters while restoring keyboard focus.
pub(super) enum PointerGuard {
    Inactive,
    Waiting,
    Anchored((f32, f32)),
}
impl PointerGuard {
    pub(super) fn allows_focus(&mut self, position: (f32, f32)) -> bool {
        match *self {
            Self::Inactive => true,
            Self::Waiting => {
                *self = Self::Anchored(position);
                false
            }
            Self::Anchored(anchor) if !pointer_moved_from(anchor, position) => false,
            Self::Anchored(_) => {
                *self = Self::Inactive;
                true
            }
        }
    }
}

// Reuse is restricted to the same owner-scoped Shell and exact running instance.
fn same_running_shell(current: Option<&ShellChoice>, target: Option<&ShellChoice>) -> bool {
    match (current, target) {
        (Some(current), Some(target)) => {
            matches!(current.status, boomux::protocol::ShellStatus::Running)
                && matches!(target.status, boomux::protocol::ShellStatus::Running)
                && current.id == target.id
                && current.run_id.is_some()
                && current.run_id == target.run_id
        }
        _ => false,
    }
}

fn encode_tree(node: &Node) -> Tree {
    match node {
        Node::Pane(id) => Tree::Pane(*id as u64),
        Node::Split {
            axis,
            ratio,
            first,
            second,
        } => Tree::Split {
            horizontal: *axis == Axis::Horizontal,
            ratio: *ratio,
            first: Box::new(encode_tree(first)),
            second: Box::new(encode_tree(second)),
        },
    }
}
fn decode_tree(node: &Tree, ids: &HashMap<u64, usize>) -> Node {
    match node {
        Tree::Pane(id) => Node::Pane(ids[id]),
        Tree::Split {
            horizontal,
            ratio,
            first,
            second,
        } => Node::Split {
            axis: if *horizontal {
                Axis::Horizontal
            } else {
                Axis::Vertical
            },
            ratio: *ratio,
            first: Box::new(decode_tree(first, ids)),
            second: Box::new(decode_tree(second, ids)),
        },
    }
}
impl TerminalPane {
    fn saved_reference(&self) -> Option<Pane> {
        if self.temporary_setup || self.shell.as_ref().is_some_and(|shell| shell.desktop_setup) {
            return None;
        }
        self.shell
            .as_ref()
            .map(|shell| Pane {
                shell: Some(shell.id.clone()),
                workspace: Some(shell.workspace_id.clone()),
            })
            .or_else(|| self.restored.clone())
    }
}

impl Workspace {
    pub(super) fn initialize_layout(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_viewport_width = f32::from(window.viewport_size().width);
        self.layout_canvas = self.panel_size(window);
        self.workspace_order = if self.layout_document.workspace_order.is_empty() {
            self.workspace_order.clone()
        } else {
            self.layout_document.workspace_order.clone()
        };
        self.minimized_shells = self.layout_document.minimized.iter().cloned().collect();
        let key = self.layout_document.active.clone();
        if key == "mixed" {
            self.workspace_pane_mode = WorkspacePaneMode::Mixed;
            self.pane_layout_mode = PaneLayoutMode::Tiled;
        }
        if let Some(workspace) = key.strip_prefix("workspace:") {
            self.workspace_pane_mode = WorkspacePaneMode::Workspace;
            self.expanded_workspaces = HashSet::from([workspace.to_owned()]);
        }
        if let Some(arrangement) = self.layout_document.arrangements.get(&key).cloned() {
            self.restore_arrangement(arrangement, window, cx);
            self.layout_changed(cx);
        } else {
            self.layout_document.active.clear();
        }
        cx.observe_window_bounds(window, |this, window, cx| {
            this.sidebar_viewport_width = f32::from(window.viewport_size().width);
            let canvas = this.panel_size(window);
            if canvas.0 > 0.0 && canvas.1 > 0.0 {
                this.layout_canvas = canvas;
                for pane in &mut this.floating {
                    *pane = clamp_floating_to_panel(pane.clone(), canvas);
                }
                this.layout_changed(cx);
                cx.notify();
            }
        })
        .detach();
        let entity = cx.weak_entity();
        window.on_window_should_close(cx, move |window, cx| {
            entity
                .update(cx, |this, cx| this.close_after_layout_save(window, cx))
                .unwrap_or(true)
        });
        cx.on_app_quit(|this, _| {
            this.layout_save_task.take();
            this.capture_arrangement();
            if this.layout_frozen
                && !this.layout_closing
                && let Some(writer) = this.layout_writer.take()
            {
                writer.close();
            }
            let pending = this.layout_writer.take().map(|writer| {
                let pending = layout_state::submit(&writer, this.layout_document.clone());
                writer.close();
                pending
            });
            async move {
                if let Some(pending) = pending
                    && let Ok(Err(error)) = pending.recv().await
                {
                    eprintln!("Could not save Desktop layout: {error}");
                }
            }
        })
        .detach();
    }

    pub(super) fn capture_arrangement(&mut self) -> bool {
        if self.layout_frozen || self.layout_restoring || self.pointer_drag.is_some() {
            return false;
        }
        let key = if self.workspace_pane_mode == WorkspacePaneMode::Mixed {
            "mixed".to_string()
        } else {
            self.terminals
                .get(&self.focused)
                .and_then(TerminalPane::saved_reference)
                .and_then(|pane| pane.workspace)
                .map(|w| format!("workspace:{w}"))
                .unwrap_or_else(|| self.layout_document.active.clone())
        };
        if key.is_empty() {
            return false;
        }
        let ids = ordered_pane_ids(self.layout.as_ref(), &self.floating);
        let panes = ids
            .iter()
            .filter_map(|id| {
                self.terminals.get(id).map(|pane| {
                    let reference = pane.saved_reference().unwrap_or(Pane {
                        shell: None,
                        workspace: None,
                    });
                    (*id as u64, reference)
                })
            })
            .collect();
        let mut arrangement = Arrangement {
            tree: self.layout.as_ref().map(encode_tree),
            floating: self
                .floating
                .iter()
                .map(|p| Floating {
                    pane: p.id as u64,
                    rect: [p.x, p.y, p.width, p.height],
                })
                .collect(),
            panes,
            focused: ids.contains(&self.focused).then_some(self.focused as u64),
            expanded: self
                .fullscreen
                .filter(|id| ids.contains(id))
                .map(|id| id as u64),
            canvas: [self.layout_canvas.0.max(1.0), self.layout_canvas.1.max(1.0)],
        };
        arrangement.discard_unbound_panes();
        let retained_panes: usize = self
            .layout_document
            .arrangements
            .iter()
            .filter(|(saved_key, _)| **saved_key != key)
            .map(|(_, saved)| saved.panes.len())
            .sum();
        if (self.layout_document.arrangements.len() >= 256
            && !self.layout_document.arrangements.contains_key(&key))
            || retained_panes + arrangement.panes.len() > 4096
        {
            self.layout_error = Some("Saved layout limit reached".into());
            return false;
        }
        if self.layout_error.as_deref() == Some("Saved layout limit reached") {
            self.layout_error = None;
        }
        self.layout_document.active = key.clone();
        self.layout_document.arrangements.insert(key, arrangement);
        self.layout_document.workspace_order = self.workspace_order.clone();
        self.layout_document
            .minimized
            .retain(|id| self.minimized_shells.contains(id));
        for workspace in &self.boomux_overview.workspaces {
            for shell in &workspace.shells {
                if self.minimized_shells.contains(&shell.id)
                    && !self.layout_document.minimized.contains(&shell.id)
                {
                    self.layout_document.minimized.push(shell.id.clone());
                }
            }
        }
        true
    }

    pub(super) fn layout_changed(&mut self, cx: &mut Context<Self>) {
        if self.layout_frozen || self.layout_restoring || self.layout_writer.is_none() {
            return;
        }
        self.layout_generation = self.layout_generation.wrapping_add(1);
        let generation = self.layout_generation;
        self.layout_save_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(250))
                .await;
            let pending = this
                .update(cx, |this, cx| {
                    if this.layout_generation != generation || this.pointer_drag.is_some() {
                        return None;
                    }
                    if !this.capture_arrangement() {
                        cx.notify();
                        return None;
                    }
                    this.layout_writer
                        .as_ref()
                        .map(|writer| layout_state::submit(writer, this.layout_document.clone()))
                })
                .ok()
                .flatten();
            if let Some(pending) = pending {
                let result = pending.recv().await;
                let _ = this.update(cx, |this, cx| {
                    if let Ok(result) = result {
                        this.layout_error = result.err();
                        cx.notify();
                    }
                });
            }
        }));
    }

    pub(super) fn restore_arrangement(
        &mut self,
        saved: Arrangement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.restore_arrangement_with_transition(saved, None, window, cx);
    }

    fn restore_arrangement_with_transition(
        &mut self,
        saved: Arrangement,
        direction: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut saved = saved;
        saved.discard_unbound_panes();
        self.layout_restoring = true;
        self.restore_pointer_guard = PointerGuard::Waiting;
        // A quick return can find this Workspace still sliding out. Preserve
        // those sessions, screens, and pane IDs before finishing the old slide;
        // detaching and immediately reattaching races the owner's controller.
        let mut reusable = HashMap::new();
        if direction.is_some() {
            let outgoing = self
                .workspace_transition
                .as_ref()
                .map(|transition| {
                    transition
                        .outgoing
                        .iter()
                        .filter_map(|outgoing| {
                            self.terminals
                                .get(&outgoing.id)
                                .and_then(|pane| pane.shell.as_ref())
                                .map(|shell| (shell.id.clone(), outgoing.id))
                        })
                        .collect::<HashMap<_, _>>()
                })
                .unwrap_or_default();
            for (saved_id, reference) in &saved.panes {
                let target = reference
                    .shell
                    .as_ref()
                    .and_then(|key| self.boomux_shells.iter().find(|shell| &shell.id == key));
                let candidate = reference
                    .shell
                    .as_ref()
                    .and_then(|key| outgoing.get(key))
                    .copied()
                    .filter(|id| {
                        self.terminals.get(id).is_some_and(|pane| {
                            (pane.attaching
                                || pane
                                    .session
                                    .as_ref()
                                    .is_some_and(|session| session.status_message().is_none()))
                                && same_running_shell(pane.shell.as_ref(), target)
                        })
                    });
                if let Some(id) = candidate {
                    reusable.insert(*saved_id, (id, self.terminals.remove(&id).unwrap()));
                }
            }
        }
        if let Some(direction) = direction {
            self.begin_workspace_transition(direction, window, cx);
        } else {
            self.finish_current_workspace_transition(window);
            self.detach_all_panes(window);
        }
        let mut ids = HashMap::new();
        for (saved_id, reference) in &saved.panes {
            if let Some((id, mut pane)) = reusable.remove(saved_id) {
                pane.restored = Some(reference.clone());
                ids.insert(*saved_id, id);
                self.terminals.insert(id, pane);
                continue;
            }
            let id = self.next_id;
            self.next_id += 1;
            ids.insert(*saved_id, id);
            let shell = reference
                .shell
                .as_ref()
                .and_then(|key| self.boomux_shells.iter().find(|s| &s.id == key))
                .cloned();
            self.terminals.insert(id, TerminalPane {
                shell, restored: Some(reference.clone()),
                error: reference.shell.as_ref().map(|_| "Saved Shell is unavailable or stopped. Select it in the sidebar to reconnect or start it.".into()),
                ..Default::default()
            });
        }
        self.layout = saved.tree.as_ref().map(|node| decode_tree(node, &ids));
        let actual_canvas = self.panel_size(window);
        let canvas = if actual_canvas.0 > 0.0 && actual_canvas.1 > 0.0 {
            actual_canvas
        } else {
            (saved.canvas[0], saved.canvas[1])
        };
        self.floating = saved
            .floating
            .iter()
            .map(|p| {
                clamp_floating_to_panel(
                    FloatingPane {
                        id: ids[&p.pane],
                        x: p.rect[0] * canvas.0 / saved.canvas[0],
                        y: p.rect[1] * canvas.1 / saved.canvas[1],
                        width: p.rect[2],
                        height: p.rect[3],
                    },
                    canvas,
                )
            })
            .collect();
        self.focused = saved
            .focused
            .and_then(|id| ids.get(&id).copied())
            .or_else(|| ids.values().copied().min())
            .unwrap_or(0);
        self.fullscreen = saved.expanded.and_then(|id| ids.get(&id).copied());
        self.animate_workspace_arrival();
        self.reconnect_saved_panes(window, cx);
        self.layout_restoring = false;
        cx.notify();
    }

    pub(super) fn restore_workspace_layout(
        &mut self,
        workspace: &str,
        preferred: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let outgoing_mixed = self.layout_document.active == "mixed";
        if outgoing_mixed == (self.workspace_pane_mode == WorkspacePaneMode::Mixed) {
            self.capture_arrangement();
        }
        if self.workspace_pane_mode == WorkspacePaneMode::Mixed {
            return false;
        }
        let key = format!("workspace:{workspace}");
        if key == self.layout_document.active {
            return false;
        }
        if let Some(saved) = self.layout_document.arrangements.get(&key).cloned() {
            let current = self.layout_document.active.strip_prefix("workspace:");
            let direction = workspace_slide_direction(&self.workspace_order, current, workspace);
            self.layout_document.active = key;
            self.project_menu_open = false;
            self.sidebar_menu = None;
            self.expanded_workspaces = HashSet::from([workspace.to_owned()]);
            self.restore_arrangement_with_transition(saved, Some(direction), window, cx);
            if let Some(preferred) = preferred {
                self.activate_sidebar_shell(preferred, window, cx);
            }
            self.layout_changed(cx);
            return true;
        }
        false
    }

    pub(super) fn start_restored_attachment(
        &mut self,
        id: usize,
        shell: ShellChoice,
        size: (u16, u16, u16, u16),
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self.terminals.get_mut(&id) {
            pane.attaching = true;
            pane.restore_attempt = shell.run_id.clone();
            pane.error = None;
        }
        cx.spawn(async move |this, cx| {
            let attached = shell.clone();
            let result = cx
                .background_spawn(async move {
                    TerminalSession::restore(shell, size.0, size.1, size.2, size.3)
                })
                .await;
            let _ = this.update(cx, |this, cx| {
                let Some(pane) = this.terminals.get_mut(&id) else {
                    return;
                };
                // The user may have explicitly attached something else while restoring.
                if pane.session.is_some() || pane.shell.as_ref().is_none_or(|s| s.id != attached.id)
                {
                    return;
                }
                pane.attaching = false;
                match result {
                    Ok(session) => {
                        pane.screen = Some(session.screen());
                        pane.session = Some(session);
                        pane.shell = Some(attached.clone());
                        this.watch_terminal(id, attached.id, cx);
                    }
                    Err(error) => {
                        pane.error = Some(error);
                        pane.restore_failures = pane.restore_failures.saturating_add(1);
                        pane.restore_retry_after = Some(
                            Instant::now()
                                + Duration::from_secs(
                                    (1u64 << pane.restore_failures.min(5)).min(30),
                                ),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }
}

impl Workspace {
    pub(super) fn reconnect_saved_pane(
        &mut self,
        id: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = self
            .terminals
            .get(&id)
            .and_then(|p| p.restored.as_ref())
            .and_then(|p| p.shell.as_ref());
        let shell = key
            .and_then(|key| self.boomux_shells.iter().find(|s| &s.id == key))
            .cloned();
        if let Some(shell) = shell {
            let size = self.terminal_grid_size(id, window);
            self.start_terminal_attachment(id, shell, size, cx);
        } else if let Some(pane) = self.terminals.get_mut(&id) {
            pane.error =
                Some("Shell is unavailable. Reconnect its Node or dismiss this pane.".into());
            cx.notify();
        }
    }

    pub(super) fn reconnect_saved_panes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let available = 4usize.saturating_sub(
            self.terminals
                .values()
                .filter(|p| p.restored.is_some() && p.attaching)
                .count(),
        );
        let mut pending = Vec::new();
        for (id, pane) in &mut self.terminals {
            if pane.session.is_some() || pane.attaching {
                continue;
            }
            let Some(key) = pane.restored.as_ref().and_then(|p| p.shell.as_ref()) else {
                continue;
            };
            if let Some(remote) = remote::identity(key)
                && !self
                    .node_views
                    .iter()
                    .any(|n| n.id == remote.node_id && n.connected())
            {
                pane.restore_attempt = None;
                continue;
            }
            let Some(shell) = self.boomux_shells.iter().find(|s| &s.id == key) else {
                continue;
            };
            let Some(run) = &shell.run_id else {
                continue;
            };
            let retry_due = pane
                .restore_retry_after
                .is_some_and(|deadline| Instant::now() >= deadline);
            if pending.len() < available
                && matches!(shell.status, boomux::protocol::ShellStatus::Running)
                && (pane.restore_attempt.as_ref() != Some(run) || retry_due)
            {
                pane.restore_attempt = Some(run.clone());
                pane.restore_retry_after = None;
                pane.shell = Some(shell.clone());
                pending.push((*id, shell.clone()));
            }
        }
        for (id, shell) in pending {
            let size = self.terminal_grid_size(id, window);
            self.start_restored_attachment(id, shell, size, cx);
        }
    }
}

impl Workspace {
    fn close_after_layout_save(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.layout_writer.is_none() || self.layout_frozen && !self.layout_closing {
            return true;
        }
        if self.layout_closing {
            return false;
        }
        if self
            .layout_error
            .as_ref()
            .is_some_and(|e| e.ends_with("Close again to exit without saving."))
        {
            return true;
        }
        self.capture_arrangement();
        if self.layout_error.as_deref() == Some("Saved layout limit reached") {
            self.layout_error =
                Some("Saved layout limit reached. Close again to exit without saving.".into());
            cx.notify();
            return false;
        }
        self.layout_save_task.take();
        self.layout_closing = true;
        self.layout_frozen = true;
        let pending = layout_state::submit(
            self.layout_writer.as_ref().unwrap(),
            self.layout_document.clone(),
        );
        let handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let result = pending
                .recv()
                .await
                .unwrap_or_else(|_| Err("Layout save interrupted".into()));
            let _ = handle.update(cx, |_, window, cx| {
                this.update(cx, |this, cx| match result {
                    Ok(()) => {
                        this.layout_closing = false;
                        window.remove_window();
                    }
                    Err(error) => {
                        this.layout_closing = false;
                        this.layout_frozen = false;
                        this.layout_error =
                            Some(format!("{error}. Close again to exit without saving."));
                        cx.notify();
                    }
                })
            });
        })
        .detach();
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn setup_overlay_does_not_replace_saved_workspace_or_panes() {
        let reference = Pane {
            shell: Some("original-shell".into()),
            workspace: Some("original-workspace".into()),
        };
        let mut pane = TerminalPane {
            restored: Some(reference.clone()),
            ..Default::default()
        };
        assert_eq!(
            pane.saved_reference().unwrap().workspace,
            reference.workspace
        );
        pane.temporary_setup = true;
        assert!(pane.saved_reference().is_none());
        // Saving while setup has focus must prune only the overlay and retain
        // the underlying tiled geometry, identity, and maximized state.
        let mut arrangement = Arrangement {
            tree: Some(Tree::Pane(1)),
            floating: vec![Floating {
                pane: 2,
                rect: [10.0, 10.0, 500.0, 400.0],
            }],
            panes: [
                (1, reference),
                (
                    2,
                    pane.saved_reference().unwrap_or(Pane {
                        shell: None,
                        workspace: None,
                    }),
                ),
            ]
            .into(),
            focused: Some(2),
            expanded: Some(1),
            canvas: [1200.0, 800.0],
        };
        arrangement.discard_unbound_panes();
        assert!(matches!(arrangement.tree, Some(Tree::Pane(1))));
        assert!(arrangement.floating.is_empty());
        assert_eq!(arrangement.panes.len(), 1);
        assert_eq!(arrangement.focused, Some(1));
        assert_eq!(arrangement.expanded, Some(1));
    }

    #[test]
    fn layout_reuse_requires_the_same_owner_shell_and_running_instance() {
        let current = ShellChoice {
            id: "remote:owner-a:shell".into(),
            name: "same label".into(),
            workspace_id: "workspace".into(),
            cwd: Default::default(),
            status: boomux::protocol::ShellStatus::Running,
            run_id: Some("run-a".into()),
            desktop_setup: false,
        };
        assert!(same_running_shell(Some(&current), Some(&current)));
        let mut changed = current.clone();
        changed.id = "remote:owner-b:shell".into();
        assert!(!same_running_shell(Some(&current), Some(&changed)));
        changed = current.clone();
        changed.run_id = Some("run-b".into());
        assert!(!same_running_shell(Some(&current), Some(&changed)));
        changed.run_id = None;
        assert!(!same_running_shell(Some(&changed), Some(&changed)));
        changed = current.clone();
        changed.status = boomux::protocol::ShellStatus::Pending;
        assert!(!same_running_shell(Some(&current), Some(&changed)));
        assert!(!same_running_shell(None, Some(&current)));
    }

    #[test]
    fn layout_restore_keeps_focus_until_the_pointer_actually_moves() {
        let mut guard = PointerGuard::Waiting;
        assert!(!guard.allows_focus((640.0, 400.0)));
        assert!(!guard.allows_focus((640.0, 400.0)));
        assert!(guard.allows_focus((650.0, 400.0)));
        assert!(guard.allows_focus((650.0, 400.0)));
    }

    #[test]
    fn layout_tree_remaps_ephemeral_ids_without_changing_split_sizes() {
        let tree = Node::Split {
            axis: Axis::Vertical,
            ratio: 0.37,
            first: Box::new(Node::Pane(17)),
            second: Box::new(Node::Pane(42)),
        };
        let restored = decode_tree(&encode_tree(&tree), &HashMap::from([(17, 101), (42, 102)]));
        assert_eq!(restored.pane_ids(), vec![101, 102]);
        assert_eq!(restored.rects()[0].1.height, 0.37);
        assert_eq!(restored.rects()[1].1.y, 0.37);
    }
}
