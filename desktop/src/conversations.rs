use crate::*;
use boomux::conversations::Conversation;

#[derive(Default)]
pub(crate) struct Panel {
    pub open: bool,
    workspace: Option<String>,
    entries: Vec<Conversation>,
    error: Option<String>,
    loading: bool,
    opening: bool,
    next_refresh: Option<Instant>,
    scroll: ScrollHandle,
    visible: usize,
    attempt: Option<(String, String, String)>,
}

impl Workspace {
    fn conversation_workspace(&self) -> Option<String> {
        let selected = if self.navigation_region == NavigationRegion::Sidebar {
            match &self.sidebar_item {
                Some(SidebarItem::Workspace(id))
                | Some(SidebarItem::Shell {
                    workspace_id: id, ..
                }) => Some(id.clone()),
                _ => None,
            }
        } else {
            None
        };
        selected
            .or_else(|| {
                self.terminals
                    .get(&self.focused)
                    .and_then(|pane| pane.shell.as_ref())
                    .map(|shell| shell.workspace_id.clone())
            })
            .or_else(|| {
                (self.expanded_workspaces.len() == 1)
                    .then(|| self.expanded_workspaces.iter().next().cloned())
                    .flatten()
            })
            .filter(|id| self.boomux_overview.workspaces.iter().any(|w| &w.id == id))
    }

    pub(crate) fn open_conversations(&mut self, cx: &mut Context<Self>) {
        self.conversations.open = !self.conversations.open;
        self.conversations.next_refresh = None;
        self.refresh_conversations(cx);
        cx.notify();
    }

    pub(crate) fn refresh_conversations(&mut self, cx: &mut Context<Self>) {
        if !self.conversations.open || self.settings_open {
            return;
        }
        let workspace = self.conversation_workspace();
        if self.conversations.workspace != workspace {
            self.conversations.workspace = workspace.clone();
            self.conversations.entries.clear();
            self.conversations.error = None;
            self.conversations.next_refresh = None;
            self.conversations.visible = 50;
            cx.notify();
        }
        let Some(key) = workspace else {
            return;
        };
        if self.conversations.loading
            || self
                .conversations
                .next_refresh
                .is_some_and(|time| time > Instant::now())
        {
            return;
        }
        self.conversations.loading = true;
        self.conversations.next_refresh = Some(Instant::now() + Duration::from_secs(3));
        cx.spawn(async move |this, cx| {
            let requested = key.clone();
            let result = cx.background_spawn(async move {
                let client = boomux::client::connect_if_running().map_err(|e| e.to_string())?.ok_or("Boomux is not running")?;
                let owner = remote::identity(&requested);
                let operation = boomux::protocol::HostServiceOperation::ListWorkspaceConversations {
                    workspace_id: owner.as_ref().map_or(requested.as_str(), |id| id.inner_id.as_str()).to_owned(),
                };
                let result = match owner {
                    Some(owner) => client.route_node_host_service(owner.node_id, operation),
                    None => client.host_service(operation),
                }.map_err(|error| error.to_string())?;
                match result {
                    boomux::protocol::HostServiceResult::WorkspaceConversations { conversations } => Ok(conversations),
                    _ => Err("Unexpected conversation response".to_owned()),
                }
            }).await;
            this.update(cx, |this, cx| {
                this.conversations.loading = false;
                if this.conversations.workspace.as_ref() != Some(&key) { return; }
                match result {
                    Ok(entries) => { this.conversations.entries = entries; this.conversations.error = None; }
                    Err(error) => { this.conversations.entries.clear(); this.conversations.error = Some(format!("Could not load conversations: {error}. For an unavailable remote, connect from Remotes.")); }
                }
                cx.notify();
            }).ok();
        }).detach();
    }

    fn open_conversation(
        &mut self,
        workspace: String,
        entry: Conversation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.conversations.opening {
            return;
        }
        let attempt = self.conversations.attempt.get_or_insert_with(|| {
            (
                workspace.clone(),
                entry.agent_id.clone(),
                uuid::Uuid::new_v4().to_string(),
            )
        });
        if attempt.0 != workspace || attempt.1 != entry.agent_id {
            *attempt = (
                workspace.clone(),
                entry.agent_id.clone(),
                uuid::Uuid::new_v4().to_string(),
            );
        }
        let shell_id = attempt.2.clone();
        self.conversations.opening = true;
        self.conversations.error = None;
        let window_handle = window.window_handle();
        cx.spawn(async move |this, cx| {
            let key = workspace.clone();
            let result = cx.background_spawn(async move {
                let client = boomux::client::connect_if_running().map_err(|e| e.to_string())?.ok_or("Boomux is not running")?;
                let owner = remote::identity(&key);
                let mut shell = client.open_workspace_conversation(owner.as_ref().map(|id| id.node_id.as_str()), owner.as_ref().map_or(key.as_str(), |id| id.inner_id.as_str()), &entry.agent_id, &shell_id).map_err(|e| e.to_string())?;
                if let Some(owner) = owner { remote::qualify_shell(&owner.node_id, &mut shell); }
                Ok::<_, String>(terminal::shell_choice(shell))
            }).await;
            window_handle.update(cx, |_, window, cx| {
                this.update(cx, |this, cx| {
                    this.conversations.opening = false;
                    match result {
                        Ok(shell) => {
                            this.conversations.attempt = None;
                            if this.conversation_workspace().as_ref() != Some(&workspace) {
                                this.conversations.error = Some("Conversation is ready in its original Workspace. Select that Workspace to open it.".into());
                            } else if let Some(id) = this.terminals.iter().find_map(|(id, pane)| pane.shell.as_ref().filter(|s| s.id == shell.id && (pane.attaching || pane.session.as_ref().is_some_and(|session| session.run_id == shell.run_id && !session.update_events().is_closed()))).map(|_| *id)) {
                                this.focused = id;
                                this.fullscreen = this.fullscreen.map(|_| id);
                                this.navigation_region = NavigationRegion::Terminal;
                                if let Some(session) = this.terminals[&id].session.as_ref() { session.focus(); }
                                window.focus(&this.focus_handle, cx);
                            } else {
                                this.fullscreen = None;
                                this.layout_animation = None;
                                let id = this.insert_pane();
                                this.focused = id;
                                this.minimized_shells.remove(&shell.id);
                                this.navigation_region = NavigationRegion::Terminal;
                                let size = this.terminal_grid_size(id, window);
                                this.start_terminal_attachment(id, shell, size, cx);
                                window.focus(&this.focus_handle, cx);
                            }
                        }
                        Err(error) => this.conversations.error = Some(format!("Could not open conversation: {error}")),
                    }
                    this.conversations.next_refresh = None;
                    this.layout_changed(cx);
                    cx.notify();
                }).ok();
            }).ok();
        }).detach();
        cx.notify();
    }

    pub(crate) fn conversations_panel(&self, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        if !self.conversations.open {
            return None;
        }
        let selected = self.conversation_workspace();
        let current = selected == self.conversations.workspace;
        let name = selected
            .as_ref()
            .and_then(|id| {
                self.boomux_overview
                    .workspaces
                    .iter()
                    .find(|workspace| &workspace.id == id)
            })
            .map(|w| w.name.clone());
        let mut list = div()
            .id("workspace-conversations")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.conversations.scroll)
            .px_3()
            .pb_3()
            .child(
                div()
                    .py_2()
                    .text_sm()
                    .child(name.unwrap_or_else(|| "Select a Workspace".into())),
            );
        if self.conversations.opening {
            list = list.child(div().text_sm().child("Opening conversation…"));
        }
        if current {
            if let Some(error) = &self.conversations.error {
                list = list.child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xf38ba8))
                        .child(error.clone()),
                );
            }
            if selected.is_some()
                && self.conversations.entries.is_empty()
                && self.conversations.error.is_none()
            {
                list = list.child(div().text_sm().text_color(rgb(0x7f849c)).child(
                    if self.conversations.loading {
                        "Loading…"
                    } else {
                        "No conversations yet. Start a supported harness in this Workspace."
                    },
                ));
            }
            for entry in self
                .conversations
                .entries
                .iter()
                .take(self.conversations.visible.max(50))
            {
                let entry = entry.clone();
                let key = selected.clone().unwrap();
                let label = if entry.running_shell.is_some() {
                    "Open"
                } else if entry.resumable {
                    "Resume"
                } else {
                    "Resume unavailable"
                };
                let harness = boomux::integrations::by_key(&entry.integration)
                    .map_or(entry.integration.as_str(), |i| i.display_name);
                list = list.child(
                    div()
                        .id(SharedString::from(format!(
                            "conversation-{}",
                            entry.agent_id
                        )))
                        .py_2()
                        .cursor_pointer()
                        .rounded_md()
                        .hover(|row| row.bg(rgb(0x313244)))
                        .child(div().text_sm().child(entry.title.clone()))
                        .child(div().text_xs().text_color(rgb(0x7f849c)).child(format!(
                            "{harness} · {label} · {}",
                            relative_time(entry.updated_at_ms)
                        )))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_conversation(key.clone(), entry.clone(), window, cx)
                        })),
                );
            }
            if self.conversations.entries.len() > self.conversations.visible.max(50) {
                list = list.child(
                    div()
                        .id("more-conversations")
                        .py_2()
                        .cursor_pointer()
                        .child("Show more")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.conversations.visible = this.conversations.visible.max(50) + 50;
                            cx.notify();
                        })),
                );
            }
        } else if selected.is_some() {
            list = list.child(div().text_sm().child("Loading…"));
        }
        Some(
            div()
                .id("conversations-drawer")
                .absolute()
                .right_0()
                .top_0()
                .bottom_0()
                .w(px(400.0))
                .max_w_full()
                .flex()
                .flex_col()
                .occlude()
                .bg(rgb(0x181825))
                .border_l_1()
                .border_color(rgb(0x313244))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px_3()
                        .py_3()
                        .child("Conversations")
                        .child(
                            div()
                                .id("close-conversations")
                                .cursor_pointer()
                                .child("Close")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.conversations.open = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(list)
                .into_any_element(),
        )
    }
}
