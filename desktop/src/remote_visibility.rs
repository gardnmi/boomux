//! Local sidebar preferences; never contacts a remote owner or deletes its work.
use super::*;
use std::collections::BTreeMap;

pub fn filter(overview: &mut BoomuxOverview, hidden: &BTreeMap<String, String>) {
    if hidden.is_empty() {
        return;
    }
    let shells: HashSet<_> = overview
        .workspaces
        .iter()
        .filter(|workspace| hidden.contains_key(&workspace.id))
        .flat_map(|workspace| workspace.shells.iter().map(|shell| shell.id.clone()))
        .collect();
    overview
        .workspaces
        .retain(|workspace| !hidden.contains_key(&workspace.id));
    overview
        .agents
        .retain(|agent| !shells.contains(&agent.shell_id));
    if overview
        .focused_shell_id
        .as_ref()
        .is_some_and(|id| shells.contains(id))
    {
        overview.focused_shell_id = None;
    }
}

impl Workspace {
    pub(super) fn hide_remote_workspace(
        &mut self,
        target: SidebarResource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let SidebarResource::Workspace { id, name } = &target else {
            return;
        };
        if remote::identity(id).is_none() {
            return;
        }
        if self.layout_frozen || self.layout_restoring || self.layout_writer.is_none() {
            self.boomux_error = Some(
                "Sidebar preferences cannot be saved right now; reopen Desktop and retry.".into(),
            );
            cx.notify();
            return;
        }
        if self.layout_document.hidden_remote_workspaces.len() >= 256 {
            self.boomux_error = Some(
                "Hidden Workspace limit reached. Show an entry from Remotes before hiding another."
                    .into(),
            );
            cx.notify();
            return;
        }
        self.layout_document
            .hidden_remote_workspaces
            .insert(id.clone(), name.clone());
        self.remove_resource_panes(&target, window);
        for saved in self.layout_document.arrangements.values_mut() {
            saved
                .panes
                .retain(|_, pane| pane.workspace.as_ref() != Some(id));
            saved.discard_unbound_panes();
        }
        self.layout_document
            .arrangements
            .remove(&format!("workspace:{id}"));
        self.expanded_workspaces.remove(id);
        self.sidebar_menu = None;
        let overview = self.boomux_overview.clone();
        self.set_boomux_overview(overview);
        self.reconcile_sidebar_item();
        self.save_remote_visibility(cx);
        cx.notify();
    }

    pub(super) fn show_remote_workspace(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.layout_frozen || self.layout_restoring || self.layout_writer.is_none() {
            return;
        }
        self.layout_document.hidden_remote_workspaces.remove(id);
        self.save_remote_visibility(cx);
        // The normal local-daemon snapshot refresh restores the cached entry.
        cx.notify();
    }

    fn save_remote_visibility(&mut self, cx: &mut Context<Self>) {
        self.capture_arrangement();
        let Some(writer) = &self.layout_writer else {
            return;
        };
        let pending = layout_state::submit(writer, self.layout_document.clone());
        cx.spawn(async move |this, cx| {
            if let Ok(result) = pending.recv().await {
                let _ = this.update(cx, |this, cx| {
                    this.layout_error = result.err();
                    cx.notify();
                });
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn hiding_remote_workspace_preserves_other_owners_and_can_be_reversed() {
        let make_workspace = |id: &str, shell_id: &str| terminal::WorkspaceChoice {
            id: id.into(),
            name: "same-name".into(),
            has_conversations: false,
            agent_count: 0,
            shells: vec![ShellChoice {
                id: shell_id.into(),
                name: "shell".into(),
                workspace_id: id.into(),
                cwd: PathBuf::from("/tmp"),
                status: boomux::protocol::ShellStatus::Pending,
                run_id: None,
                desktop_setup: false,
            }],
        };
        let original = BoomuxOverview {
            workspaces: vec![
                make_workspace("remote:node-a:w", "remote:node-a:s"),
                make_workspace("remote:node-b:w", "remote:node-b:s"),
                make_workspace("w", "s"),
            ],
            agents: vec![],
            focused_shell_id: Some("remote:node-a:s".into()),
        };
        let mut hidden = BTreeMap::from([("remote:node-a:w".into(), "same-name".into())]);
        let mut overview = original.clone();
        filter(&mut overview, &hidden);
        assert_eq!(overview.workspaces.len(), 2);
        assert_eq!(overview.workspaces[0].id, "remote:node-b:w");
        assert_eq!(overview.workspaces[1].id, "w");
        assert!(overview.focused_shell_id.is_none());
        hidden.clear();
        let mut overview = original.clone();
        filter(&mut overview, &hidden);
        assert_eq!(overview, original);
    }
}
