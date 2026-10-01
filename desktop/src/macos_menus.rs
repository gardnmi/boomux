//! macOS menu actions share the same recipient-aware edit methods as shortcuts.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]
use crate::clipboard_routing::{EditAction, MAX_CLIPBOARD_BYTES, Recipient};
use crate::*;
use gpui::{Menu, MenuItem, OsAction};

gpui::actions!(
    native_menu,
    [
        Copy,
        Cut,
        Paste,
        SelectAll,
        Settings,
        Hide,
        HideOthers,
        ShowAll,
        Minimize,
        Zoom,
        WindowFullscreen
    ]
);

pub(crate) fn menus() -> Vec<Menu> {
    vec![
        Menu::new("Boomux Desktop").items([
            MenuItem::action("Settings…", Settings),
            MenuItem::separator(),
            MenuItem::action("Hide Boomux Desktop", Hide),
            MenuItem::action("Hide Others", HideOthers),
            MenuItem::action("Show All", ShowAll),
            MenuItem::separator(),
            MenuItem::action("Quit Boomux Desktop", Quit),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::separator(),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebarDrawer),
            MenuItem::action("Keyboard Shortcuts", ToggleHelp),
            MenuItem::separator(),
            MenuItem::action("Toggle Window Full Screen", WindowFullscreen),
        ]),
        Menu::new("Window").items([
            MenuItem::action("Minimize", Minimize),
            MenuItem::action("Zoom", Zoom),
        ]),
    ]
}

pub(crate) fn bindings() -> Vec<KeyBinding> {
    vec![
        KeyBinding::new("cmd-x", Cut, None),
        KeyBinding::new("cmd-c", Copy, None),
        KeyBinding::new("cmd-v", Paste, None),
        KeyBinding::new("cmd-a", SelectAll, None),
        KeyBinding::new("cmd-,", Settings, None),
        KeyBinding::new("cmd-h", Hide, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("ctrl-cmd-f", WindowFullscreen, None),
        KeyBinding::new("cmd-q", Quit, None),
    ]
}

/// Disabled menu actions must also consume their standard shortcut rather than
/// leaking Command-C/X/A/V into an enhanced terminal keyboard protocol.
pub(crate) fn edit_shortcut(key: &gpui::Keystroke) -> Option<EditAction> {
    let modifiers = key.modifiers;
    if !modifiers.platform || modifiers.control || modifiers.alt || modifiers.function {
        return None;
    }
    match key.key.as_str() {
        "c" => Some(EditAction::Copy),
        "x" => Some(EditAction::Cut),
        "v" => Some(EditAction::Paste),
        "a" => Some(EditAction::SelectAll),
        _ => None,
    }
}

pub(crate) fn install(cx: &mut App) {
    cx.bind_keys(bindings());
    cx.on_action(|_: &Hide, cx| cx.hide());
    cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
    cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
    cx.set_menus(menus());
}

impl Workspace {
    pub(crate) fn native_menu_actions(
        &self,
        element: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if self.native_input.rendering_frozen {
            return element;
        }
        element
            .when(self.native_edit_available(EditAction::Copy), |e| {
                e.on_action(cx.listener(|this, _: &Copy, window, cx| {
                    this.native_menu_edit(EditAction::Copy, window, cx)
                }))
            })
            .when(self.native_edit_available(EditAction::Cut), |e| {
                e.on_action(cx.listener(|this, _: &Cut, window, cx| {
                    this.native_menu_edit(EditAction::Cut, window, cx)
                }))
            })
            .when(self.native_edit_available(EditAction::Paste), |e| {
                e.on_action(cx.listener(|this, _: &Paste, window, cx| {
                    this.native_menu_edit(EditAction::Paste, window, cx)
                }))
            })
            .when(self.native_edit_available(EditAction::SelectAll), |e| {
                e.on_action(cx.listener(|this, _: &SelectAll, window, cx| {
                    this.native_menu_edit(EditAction::SelectAll, window, cx)
                }))
            })
            .when(
                self.resource_dialog.is_none() && self.theme_candidate.is_none(),
                |e| {
                    e.on_action(cx.listener(|this, _: &Settings, window, cx| {
                        if this.resource_dialog.is_some() || this.theme_candidate.is_some() {
                            return;
                        }
                        this.cancel_native_input();
                        this.flush_native_discard(window, cx);
                        if !this.settings_open {
                            this.toggle_settings(cx);
                        }
                        window.focus(&this.focus_handle, cx);
                        cx.stop_propagation();
                    }))
                },
            )
            .on_action(cx.listener(|this, _: &Minimize, window, cx| {
                this.cancel_native_input();
                this.flush_native_discard(window, cx);
                window.minimize_window();
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_, _: &Zoom, window, cx| {
                window.zoom_window();
                cx.stop_propagation();
            }))
            .on_action(cx.listener(|_, _: &WindowFullscreen, window, cx| {
                window.toggle_fullscreen();
                cx.stop_propagation();
            }))
    }

    fn native_edit_available(&self, action: EditAction) -> bool {
        match self.native_clipboard_recipient() {
            Some(Recipient::Field(_)) => match action {
                EditAction::Copy | EditAction::Cut => self.native_field_selection(),
                EditAction::Paste | EditAction::SelectAll => true,
            },
            Some(Recipient::Terminal { pane, .. })
                if self.keyboard_input_target().allows_terminal_paste() =>
            {
                match action {
                    EditAction::Cut => false,
                    EditAction::Copy => self
                        .terminals
                        .get(&pane)
                        .is_some_and(|p| p.selection.is_some()),
                    EditAction::Paste => true,
                    EditAction::SelectAll => self
                        .terminals
                        .get(&pane)
                        .and_then(|p| p.screen.as_ref())
                        .is_some_and(|s| s.rows > 0 && s.cols > 0),
                }
            }
            _ => false,
        }
    }

    fn native_clipboard_error(&mut self, message: String, cx: &mut Context<Self>) {
        if let Some(dialog) = self.resource_dialog.as_mut() {
            dialog.error = Some(message);
        } else if self.boomux_setting_input.is_some() {
            self.boomux_settings_message = Some(message);
        } else {
            self.boomux_error = Some(message);
        }
        cx.notify();
    }

    pub(crate) fn native_menu_edit(
        &mut self,
        action: EditAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.sync_native_input();
        let Some(recipient) = self.native_clipboard_recipient() else {
            cx.stop_propagation();
            return;
        };
        let ticket = self.native_clipboard_requests.start(recipient);
        // Replace/cancel a prior asynchronous copy before beginning any new edit.
        self.selection_copy_task = None;
        if action == EditAction::Paste {
            if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                // Reading the platform clipboard may materialize its original
                // allocation; this bound prevents further large encoding/copies.
                if !crate::clipboard_routing::acceptable_paste(text.len()) {
                    self.native_clipboard_error(
                        "Paste exceeds the 4 MiB clipboard limit.".into(),
                        cx,
                    );
                } else if self
                    .native_clipboard_requests
                    .accepts(ticket, self.native_clipboard_recipient())
                {
                    if matches!(recipient, Recipient::Field(_)) {
                        self.paste_native_field(&text, cx);
                    } else if let Recipient::Terminal { pane, .. } = recipient {
                        self.cancel_native_input();
                        let result = self
                            .terminals
                            .get(&pane)
                            .and_then(|p| p.session.as_ref())
                            .map(|terminal| terminal.paste_from_native_clipboard(&text));
                        match result {
                            Some(Ok(())) => {
                                if let Some(pane) = self.terminals.get_mut(&pane) {
                                    pane.selection = None;
                                }
                                cx.notify();
                            }
                            Some(Err(error)) => self.native_clipboard_error(error, cx),
                            None => {}
                        }
                    }
                }
            }
        } else if matches!(recipient, Recipient::Field(_)) {
            self.native_field_edit(action, cx);
        } else if let Recipient::Terminal { pane: pane_id, .. } = recipient {
            match action {
                EditAction::SelectAll => {
                    if let Some(pane) = self.terminals.get_mut(&pane_id)
                        && let Some(screen) = &pane.screen
                        && screen.cols > 0
                        && screen.scroll_total > 0
                    {
                        pane.selection = Some(TerminalSelection {
                            anchor: (0, 0),
                            head: (
                                usize::try_from(screen.scroll_total.saturating_sub(1))
                                    .unwrap_or(usize::MAX),
                                usize::from(screen.cols - 1),
                            ),
                        });
                        cx.notify();
                    }
                }
                EditAction::Copy => {
                    let Some(pane) = self.terminals.get(&pane_id) else {
                        return;
                    };
                    let (Some(selection), Some(session)) = (pane.selection, pane.session.as_ref())
                    else {
                        return;
                    };
                    // The worker owns terminal selection formatting and already
                    // enforces the same 4 MiB limit, including offscreen history.
                    match session
                        .selected_text_from_native_clipboard(selection.anchor, selection.head)
                    {
                        Ok(receiver) => {
                            self.selection_copy_task = Some(cx.spawn(async move |this, cx| {
                                if let Ok(result) = receiver.recv().await {
                                    let _ = this.update(cx, |this, cx| {
                                        this.sync_native_input();
                                        if !this
                                            .native_clipboard_requests
                                            .accepts(ticket, this.native_clipboard_recipient())
                                            || this
                                                .terminals
                                                .get(&pane_id)
                                                .and_then(|pane| pane.selection)
                                                != Some(selection)
                                        {
                                            return;
                                        }
                                        match result {
                                            Ok(text)
                                                if !text.is_empty()
                                                    && text.len() <= MAX_CLIPBOARD_BYTES =>
                                            {
                                                this.copy_terminal_text(pane_id, text, cx)
                                            }
                                            Ok(_) => {}
                                            Err(error) => this.native_clipboard_error(error, cx),
                                        }
                                    });
                                }
                            }));
                        }
                        Err(error) => self.native_clipboard_error(error, cx),
                    }
                }
                EditAction::Cut | EditAction::Paste => {}
            }
        }
        self.flush_native_discard(window, cx);
        cx.stop_propagation();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn menu_groups_and_system_edit_selectors_are_explicit() {
        let menus = menus();
        assert_eq!(
            menus.iter().map(|m| m.name.as_ref()).collect::<Vec<_>>(),
            ["Boomux Desktop", "Edit", "View", "Window"]
        );
        let edit = &menus[1];
        let selectors = edit
            .items
            .iter()
            .filter_map(|item| match item {
                MenuItem::Action { os_action, .. } => *os_action,
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            selectors
                == [
                    OsAction::Cut,
                    OsAction::Copy,
                    OsAction::Paste,
                    OsAction::SelectAll
                ]
        );
    }
    #[test]
    fn disabled_edit_shortcuts_cannot_leak_into_the_terminal() {
        assert_eq!(
            edit_shortcut(&gpui::Keystroke::parse("cmd-c").unwrap()),
            Some(EditAction::Copy)
        );
        assert_eq!(
            edit_shortcut(&gpui::Keystroke::parse("cmd-x").unwrap()),
            Some(EditAction::Cut)
        );
        assert_eq!(
            edit_shortcut(&gpui::Keystroke::parse("ctrl-c").unwrap()),
            None
        );
        assert_eq!(
            edit_shortcut(&gpui::Keystroke::parse("cmd-alt-c").unwrap()),
            None
        );
    }

    #[test]
    fn native_reserved_bindings_are_unique_and_leave_control_c_alone() {
        let bindings = bindings();
        let mut keys = HashSet::new();
        for binding in bindings {
            let strokes = binding.keystrokes();
            assert_eq!(strokes.len(), 1);
            let key = strokes[0].inner();
            assert!(key.modifiers.platform);
            assert!(keys.insert(key.unparse()));
        }
    }
}
