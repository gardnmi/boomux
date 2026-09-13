use crate::*;
use boomux::conversations::Conversation;

#[derive(Default)]
pub(crate) struct Panel {
    pub open: bool,
    pub search_focused: bool,
    search: String,
    archived: bool,
    filtered: Vec<usize>,
    pinned: HashSet<usize>,
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

fn conversation_button(id: impl Into<gpui::ElementId>, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .role(gpui::Role::Button)
        .aria_label(label)
        .text_xs()
        .px_2()
        .py_1()
        .rounded(px(3.0))
        .border_1()
        .border_color(rgb(0x45475a))
        .bg(rgb(0x242432))
        .text_color(rgb(0xcdd6f4))
        .cursor_pointer()
        .hover(|button| {
            button
                .bg(rgb(0x45475a))
                .border_color(rgb(0x89b4fa))
                .text_color(rgb(0xffffff))
        })
        .child(label)
}

fn filtered_entries(
    entries: &[Conversation],
    preferences: &[layout_state::ConversationPreference],
    workspace: &str,
    search: &str,
    archived: bool,
) -> Vec<usize> {
    let query = search.trim().to_lowercase();
    let flags: HashMap<_, _> = preferences
        .iter()
        .filter(|p| p.workspace == workspace)
        .map(|p| {
            (
                (p.integration.as_str(), p.session.as_str()),
                (p.pinned, p.archived),
            )
        })
        .collect();
    let mut rows: Vec<_> = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            let (pinned, hidden) = flags
                .get(&(
                    entry.integration.as_str(),
                    entry.external_session_id.as_str(),
                ))
                .copied()
                .unwrap_or_default();
            let harness = boomux::integrations::by_key(&entry.integration)
                .map_or(entry.integration.as_str(), |i| i.display_name);
            (hidden == archived
                && (query.is_empty()
                    || entry.title.to_lowercase().contains(&query)
                    || harness.to_lowercase().contains(&query)))
            .then_some((index, pinned, entry.updated_at_ms))
        })
        .collect();
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1).then(b.2.cmp(&a.2)).then_with(|| {
            entries[a.0]
                .external_session_id
                .cmp(&entries[b.0].external_session_id)
        })
    });
    rows.into_iter().map(|row| row.0).collect()
}

impl Workspace {
    fn filter_conversations(&mut self) {
        let workspace = self.conversations.workspace.as_deref().unwrap_or_default();
        let pinned: HashSet<_> = self
            .layout_document
            .conversations
            .iter()
            .filter(|p| p.workspace == workspace && p.pinned)
            .map(|p| (p.integration.as_str(), p.session.as_str()))
            .collect();
        self.conversations.pinned = self
            .conversations
            .entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| {
                pinned
                    .contains(&(
                        entry.integration.as_str(),
                        entry.external_session_id.as_str(),
                    ))
                    .then_some(index)
            })
            .collect();
        self.conversations.filtered = filtered_entries(
            &self.conversations.entries,
            &self.layout_document.conversations,
            self.conversations.workspace.as_deref().unwrap_or_default(),
            &self.conversations.search,
            self.conversations.archived,
        );
    }

    pub(crate) fn conversation_search_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        let modifiers = event.keystroke.modifiers;
        match event.keystroke.key.as_str() {
            "escape" | "enter" => self.conversations.search_focused = false,
            "backspace" => {
                self.conversations.search.pop();
            }
            "u" if modifiers.control => self.conversations.search.clear(),
            "v" if modifiers.platform || modifiers.control => {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    project_search::append(&mut self.conversations.search, &text);
                }
            }
            _ if !modifiers.control && !modifiers.platform && !modifiers.alt => {
                if let Some(text) = &event.keystroke.key_char {
                    project_search::append(&mut self.conversations.search, text);
                }
            }
            _ => {}
        }
        self.conversations.visible = 50;
        self.filter_conversations();
        cx.stop_propagation();
        cx.notify();
    }

    fn change_conversation_preference(
        &mut self,
        workspace: &str,
        entry: &Conversation,
        archive: bool,
        cx: &mut Context<Self>,
    ) {
        if self.layout_frozen || self.layout_restoring || self.layout_writer.is_none() {
            self.conversations.error =
                Some("Conversation preferences cannot be saved in this window.".into());
            cx.notify();
            return;
        }
        let preferences = &mut self.layout_document.conversations;
        let index = preferences.iter().position(|p| {
            p.workspace == workspace
                && p.integration == entry.integration
                && p.session == entry.external_session_id
        });
        let index = match index {
            Some(index) => index,
            None if preferences.len() < 4096 => {
                preferences.push(layout_state::ConversationPreference {
                    workspace: workspace.into(),
                    integration: entry.integration.clone(),
                    session: entry.external_session_id.clone(),
                    pinned: false,
                    archived: false,
                });
                preferences.len() - 1
            }
            None => {
                self.conversations.error = Some(
                    "Conversation preference limit reached. Unpin or restore older entries first."
                        .into(),
                );
                cx.notify();
                return;
            }
        };
        if archive {
            preferences[index].archived = !preferences[index].archived;
        } else {
            preferences[index].pinned = !preferences[index].pinned;
        }
        preferences.retain(|p| p.pinned || p.archived);
        self.filter_conversations();
        self.save_presentation_preferences(cx);
        cx.notify();
    }

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
        self.conversations.search_focused = false;
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
            self.conversations.filtered.clear();
            self.conversations.search.clear();
            self.conversations.archived = false;
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
                this.filter_conversations();
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
        self.conversations.search_focused = false;
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
        let mut list = div()
            .id("workspace-conversations")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.conversations.scroll)
            .px_3()
            .pb_3();
        if selected.is_none() {
            list = list.child(div().py_2().text_sm().child("Select a Workspace"));
        }
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
            for index in self
                .conversations
                .filtered
                .iter()
                .take(self.conversations.visible.max(50))
            {
                let entry = self.conversations.entries[*index].clone();
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
                let pinned = self.conversations.pinned.contains(index);
                let archived = self.conversations.archived;
                let open_entry = entry.clone();
                let open_key = key.clone();
                let can_open = entry.running_shell.is_some() || entry.resumable;
                let pin_entry = entry.clone();
                let archive_entry = entry.clone();
                let pin_key = key.clone();
                let archive_key = key.clone();
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
                        .child(div().text_sm().child(if pinned {
                            format!("★ {}", entry.title)
                        } else {
                            entry.title.clone()
                        }))
                        .child(div().text_xs().text_color(rgb(0x7f849c)).child(format!(
                            "{harness} · {label}{} · {}",
                            if entry.running_shell.is_some() {
                                " running conversation"
                            } else if entry.resumable {
                                " in original harness"
                            } else {
                                ""
                            },
                            relative_time(entry.updated_at_ms)
                        )))
                        .child(
                            div()
                                .flex()
                                .gap_3()
                                .pt_2()
                                .when(can_open, |actions| {
                                    actions.child(
                                        conversation_button(
                                            SharedString::from(format!("open-{}", entry.agent_id)),
                                            label,
                                        )
                                        .w(px(80.0))
                                        .h(px(28.0))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .button_chrome()
                                        .on_click(
                                            cx.listener(move |this, _, window, cx| {
                                                cx.stop_propagation();
                                                this.open_conversation(
                                                    open_key.clone(),
                                                    open_entry.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }),
                                        ),
                                    )
                                })
                                .child(
                                    conversation_button(
                                        SharedString::from(format!("pin-{}", entry.agent_id)),
                                        if pinned { "Unpin" } else { "Pin" },
                                    )
                                    .w(px(80.0))
                                    .h(px(28.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .button_chrome()
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.change_conversation_preference(
                                                &pin_key, &pin_entry, false, cx,
                                            );
                                        },
                                    )),
                                )
                                .child(
                                    conversation_button(
                                        SharedString::from(format!("archive-{}", entry.agent_id)),
                                        if archived { "Restore" } else { "Archive" },
                                    )
                                    .w(px(80.0))
                                    .h(px(28.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .button_chrome()
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            cx.stop_propagation();
                                            this.change_conversation_preference(
                                                &archive_key,
                                                &archive_entry,
                                                true,
                                                cx,
                                            );
                                        },
                                    )),
                                ),
                        )
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if can_open {
                                this.open_conversation(key.clone(), entry.clone(), window, cx);
                            }
                        })),
                );
            }
            if self.conversations.filtered.is_empty() && !self.conversations.entries.is_empty() {
                list = list.child(
                    div()
                        .py_2()
                        .text_sm()
                        .child(if self.conversations.archived {
                            "No archived conversations match."
                        } else {
                            "No conversations match. Check Archived or clear search."
                        }),
                );
            }
            if self.conversations.filtered.len() > self.conversations.visible.max(50) {
                list = list.child(
                    conversation_button("more-conversations", "Show more")
                        .button_chrome()
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
                            conversation_button("close-conversations", "Close")
                                .button_chrome()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.conversations.open = false;
                                    this.conversations.search_focused = false;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    div()
                        .id("conversation-search")
                        .role(gpui::Role::SearchInput)
                        .mx_3()
                        .mb_2()
                        .p_2()
                        .border_1()
                        .rounded_md()
                        .border_color(rgb(if self.conversations.search_focused {
                            0x89b4fa
                        } else {
                            0x45475a
                        }))
                        .text_sm()
                        .child(if self.conversations.search.is_empty() {
                            "Search title or harness…".into()
                        } else {
                            self.conversations.search.clone()
                        })
                        .button_chrome()
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.conversations.search_focused = true;
                            window.focus(&this.focus_handle, cx);
                            cx.stop_propagation();
                            cx.notify();
                        })),
                )
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .px_3()
                        .pb_2()
                        .children([false, true].map(|archived| {
                            div()
                                .id(if archived {
                                    "conversations-archived"
                                } else {
                                    "conversations-recent"
                                })
                                .role(gpui::Role::Button)
                                .text_sm()
                                .cursor_pointer()
                                .px_2()
                                .py_1()
                                .rounded_md()
                                .hover(|tab| tab.bg(rgb(0x313244)).text_color(rgb(0xffffff)))
                                .border_b_2()
                                .border_color(rgb(if self.conversations.archived == archived {
                                    0x89b4fa
                                } else {
                                    0x181825
                                }))
                                .child(if archived {
                                    "Archived"
                                } else {
                                    "Recent · pinned first"
                                })
                                .button_chrome()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.conversations.archived = archived;
                                    this.conversations.visible = 50;
                                    this.filter_conversations();
                                    cx.notify();
                                }))
                        })),
                )
                .child(list)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn entry(id: &str, title: &str, time: u64) -> Conversation {
        Conversation {
            agent_id: id.into(),
            integration: "codex".into(),
            external_session_id: id.into(),
            title: title.into(),
            updated_at_ms: time,
            running_shell: None,
            resumable: true,
        }
    }
    #[test]
    fn conversation_filters_preserve_workspace_scope_pins_archive_and_recency() {
        let entries = vec![
            entry("old", "Investigate retries", 1),
            entry("new", "Review PR", 3),
            entry("hidden", "Deploy", 2),
        ];
        let preferences = vec![
            layout_state::ConversationPreference {
                workspace: "w".into(),
                integration: "codex".into(),
                session: "old".into(),
                pinned: true,
                archived: false,
            },
            layout_state::ConversationPreference {
                workspace: "w".into(),
                integration: "codex".into(),
                session: "hidden".into(),
                pinned: false,
                archived: true,
            },
        ];
        assert_eq!(
            filtered_entries(&entries, &preferences, "w", "", false),
            [0, 1]
        );
        assert_eq!(filtered_entries(&entries, &preferences, "w", "", true), [2]);
        assert_eq!(
            filtered_entries(&entries, &preferences, "w", "REVIEW", false),
            [1]
        );
        assert_eq!(
            filtered_entries(&entries, &preferences, "w", "codex", false),
            [0, 1]
        );
        assert_eq!(
            filtered_entries(&entries, &preferences, "recreated-w", "", false),
            [1, 2, 0]
        );
        let mut other_harness = entries.clone();
        other_harness[0].integration = "claude".into();
        assert_eq!(
            filtered_entries(&other_harness, &preferences, "w", "", false),
            [1, 0]
        );
        // A new Agent run of the same conversation retains its pin.
        let mut resumed = entries.clone();
        resumed[0].agent_id = "new-run-agent".into();
        assert_eq!(
            filtered_entries(&resumed, &preferences, "w", "", false),
            [0, 1]
        );
        assert!(filtered_entries(&entries, &preferences, "w", "missing", false).is_empty());
    }
}
