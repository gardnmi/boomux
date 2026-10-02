//! macOS text input adapter. Linux keeps its existing raw-key path and elements.
use crate::*;

impl Workspace {
    pub(crate) fn editable_text(
        &self,
        target: InputTarget,
        text: String,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        #[cfg(target_os = "macos")]
        if !self.native_input.rendering_frozen
            && let Some(owner) = self.native_owner().filter(|owner| owner.target() == target)
        {
            return self.native_text_element(owner, text, None, cx);
        }
        let _ = (target, cx);
        text.into_any_element()
    }

    pub(crate) fn native_terminal_overlay(
        &self,
        pane_id: usize,
        cx: &mut Context<Self>,
    ) -> Option<gpui::AnyElement> {
        #[cfg(target_os = "macos")]
        if !self.native_input.rendering_frozen
            && let Some(owner @ native::Owner::Terminal { .. }) = self.native_owner()
            && pane_id == self.focused
        {
            let screen = self.terminals.get(&pane_id)?.screen.as_ref()?;
            let (col, row) = screen
                .input_cursor
                .unwrap_or((0, screen.rows.saturating_sub(1)));
            return Some(self.native_text_element(
                owner,
                String::new(),
                Some((usize::from(col), usize::from(row))),
                cx,
            ));
        }
        let _ = (pane_id, cx);
        None
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) use native::State;

#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod native {
    use super::*;
    use crate::text_input::{self, KeyRoute, Limits, Session, from_utf16, to_utf16};
    use gpui::{
        ElementInputHandler, Entity, EntityInputHandler, InputHandler, Pixels, Point,
        UTF16Selection,
    };
    use std::ops::Range;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) enum Owner {
        Terminal { pane: usize, attachment: u64 },
        Rename(SidebarResource),
        Project,
        Git,
        Conversation,
        Setting(usize),
    }
    impl Owner {
        pub fn target(&self) -> InputTarget {
            match self {
                Self::Terminal { .. } => InputTarget::Workspace,
                Self::Rename(_) => InputTarget::ResourceDialog,
                Self::Project => InputTarget::ProjectSearch,
                Self::Git => InputTarget::GitSearch,
                Self::Conversation => InputTarget::ConversationSearch,
                Self::Setting(_) => InputTarget::SettingsInput,
            }
        }
        fn terminal(&self) -> bool {
            matches!(self, Self::Terminal { .. })
        }
        fn limits(&self) -> Limits {
            match self {
                Self::Terminal { .. } => Limits::TERMINAL,
                Self::Rename(_) => Limits::NAME,
                Self::Setting(_) => Limits::SETTING,
                _ => Limits::SEARCH,
            }
        }
    }

    struct PaintedLine {
        start: usize,
        origin: Point<Pixels>,
        line: ShapedLine,
    }

    #[derive(Default)]
    pub(crate) struct State {
        pub(crate) rendering_frozen: bool,
        pending_key: Option<KeyDownEvent>,
        discard_pending: bool,
        discard_scheduled: bool,
        alt_pressed: HashMap<String, Option<String>>,
        session: Session<Owner>,
        lines: Vec<PaintedLine>,
        bounds: Option<Bounds<Pixels>>,
    }

    impl State {
        fn terminal_key(
            &mut self,
            source: &gpui::Keystroke,
            option_as_alt: bool,
            held: bool,
        ) -> gpui::Keystroke {
            let mut key = source.clone();
            if option_as_alt
                && key.modifiers.alt
                && !key.modifiers.control
                && !key.modifiers.platform
            {
                key.key_char = if key.key == "space" {
                    Some(" ".into())
                } else if key.key.chars().count() == 1 {
                    Some(if key.modifiers.shift {
                        key.key.to_ascii_uppercase()
                    } else {
                        key.key.clone()
                    })
                } else {
                    None
                };
                if !held {
                    self.alt_pressed
                        .insert(key.key.clone(), key.key_char.clone());
                }
            }
            key
        }

        fn terminal_release(&mut self, source: &gpui::Keystroke) -> gpui::Keystroke {
            if self
                .pending_key
                .as_ref()
                .is_some_and(|pending| pending.keystroke.key == source.key)
            {
                self.pending_key = None;
            }
            let mut key = source.clone();
            if let Some(text) = self.alt_pressed.remove(&key.key) {
                key.key_char = text;
            }
            key
        }
    }

    impl Workspace {
        pub(super) fn native_owner(&self) -> Option<Owner> {
            if self.layout_frozen
                || self.layout_closing
                || self.layout_restoring
                || self.theme_candidate.is_some()
                || self.git_panel.cleanup.is_some()
            {
                return None;
            }
            match self.keyboard_input_target() {
                InputTarget::ResourceDialog => self
                    .resource_dialog
                    .as_ref()
                    .filter(|d| d.kind == ResourceDialogKind::Rename && !d.busy)
                    .map(|d| Owner::Rename(d.target.clone())),
                InputTarget::ProjectSearch => Some(Owner::Project),
                InputTarget::GitSearch => Some(Owner::Git),
                InputTarget::ConversationSearch => Some(Owner::Conversation),
                InputTarget::SettingsInput => self
                    .boomux_setting_input
                    .as_ref()
                    .map(|(i, _)| Owner::Setting(*i)),
                InputTarget::Workspace
                    if !self.layout_mode
                        && !self.settings_open
                        && self.navigation_region == NavigationRegion::Terminal =>
                {
                    self.terminals
                        .get(&self.focused)
                        .filter(|pane| pane.session.is_some() && !pane.attaching)
                        .map(|pane| Owner::Terminal {
                            pane: self.focused,
                            attachment: pane.attachment_generation,
                        })
                }
                _ => None,
            }
        }

        fn native_committed(&self, owner: &Owner) -> &str {
            match owner {
                Owner::Terminal { .. } => "",
                Owner::Rename(_) => self
                    .resource_dialog
                    .as_ref()
                    .map_or("", |d| d.value.as_str()),
                Owner::Project => &self.project_search,
                Owner::Git => &self.git_panel.search,
                Owner::Conversation => self.conversation_search_text(),
                Owner::Setting(_) => self.boomux_setting_input.as_ref().map_or("", |(_, s)| s),
            }
        }

        pub(crate) fn sync_native_input(&mut self) {
            let owner = self.native_owner();
            let committed = owner
                .as_ref()
                .map_or("", |owner| self.native_committed(owner))
                .to_string();
            let generation = self.native_input.session.generation;
            let had_mark = self.native_input.session.buffer.marked.is_some();
            self.native_input.session.sync(owner, &committed);
            if self.native_input.session.generation != generation {
                self.native_input.discard_pending |= had_mark;
                self.native_input.pending_key = None;
                self.native_input.lines.clear();
                self.native_input.bounds = None;
            }
        }

        pub(crate) fn cancel_native_input(&mut self) {
            self.native_input.pending_key = None;
            self.native_input.discard_pending |= self.native_input.session.buffer.marked.is_some();
            self.native_input.session.cancel();
            self.native_input.lines.clear();
            self.native_input.bounds = None;
        }

        pub(crate) fn flush_native_discard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
            if self.native_input.discard_scheduled
                || !std::mem::take(&mut self.native_input.discard_pending)
            {
                return;
            }
            self.native_input.discard_scheduled = true;
            let view = cx.weak_entity();
            // AppKit may call its text client synchronously. Do not hold a
            // mutable Workspace borrow while discarding the OS composition.
            window.defer(cx, move |window, cx| {
                #[cfg(target_os = "macos")]
                crate::macos_text_input::discard_marked_text(window);
                #[cfg(not(target_os = "macos"))]
                let _ = window;
                let _ = view.update(cx, |this, cx| {
                    this.native_input.discard_scheduled = false;
                    this.native_input.session.reject_commit = false;
                    cx.notify();
                });
            });
        }

        fn native_ready(&self, window: &Window) -> bool {
            !self.native_input.discard_pending
                && !self.native_input.discard_scheduled
                && window.is_window_active()
                && self.focus_handle.is_focused(window)
                && self.native_input.session.owner.is_some()
                && self.native_input.session.owner == self.native_owner()
        }

        fn publish_native_text(&mut self, cx: &mut Context<Self>) {
            let Some(owner) = self.native_input.session.owner.clone() else {
                return;
            };
            let text = self.native_input.session.buffer.text.clone();
            match owner {
                Owner::Terminal { pane, .. } => {
                    if let Some(terminal) = self
                        .terminals
                        .get(&pane)
                        .and_then(|pane| pane.session.as_ref())
                    {
                        terminal.commit_text(&text);
                    }
                    if let Some(pane) = self.terminals.get_mut(&pane) {
                        pane.selection = None;
                    }
                    self.native_input.session.buffer.reset("");
                }
                Owner::Rename(_) => {
                    if let Some(dialog) = self.resource_dialog.as_mut() {
                        dialog.value = text;
                        dialog.error = None;
                    }
                }
                Owner::Project => self.project_search = text,
                Owner::Git => self.git_panel.search = text,
                Owner::Conversation => self.set_conversation_search_text(text),
                Owner::Setting(_) => {
                    if let Some((_, value)) = self.boomux_setting_input.as_mut() {
                        *value = text;
                    }
                }
            }
            cx.notify();
        }

        /// Return true when the native route owns the event. Printable field
        /// keys propagate to AppKit; editing keys are consumed locally.
        pub(crate) fn native_key_down(
            &mut self,
            event: &KeyDownEvent,
            cx: &mut Context<Self>,
        ) -> bool {
            self.sync_native_input();
            self.native_input.pending_key = None;
            self.native_input.session.reject_commit = false;
            let Some(owner) = self.native_input.session.owner.clone() else {
                return false;
            };
            let key = &event.keystroke;
            let mods = key.modifiers;
            if self.native_input.session.buffer.marked.is_some() {
                if key.key == "escape" || mods.control || mods.platform {
                    self.cancel_native_input();
                    if key.key == "escape" {
                        cx.stop_propagation();
                        cx.notify();
                        return true;
                    }
                } else {
                    // AppKit already had first refusal. Never turn an IME's
                    // unhandled Return into a shell command or save a preedit.
                    cx.stop_propagation();
                    return true;
                }
            }
            if !owner.terminal() {
                if mods.platform {
                    let action = match key.key.as_str() {
                        "a" => Some(crate::clipboard_routing::EditAction::SelectAll),
                        "c" => Some(crate::clipboard_routing::EditAction::Copy),
                        "x" => Some(crate::clipboard_routing::EditAction::Cut),
                        _ => None,
                    };
                    if let Some(action) = action {
                        self.native_field_edit(action, cx);
                        cx.stop_propagation();
                        return true;
                    }
                }
                if !mods.control && !mods.platform && !mods.alt && !mods.function {
                    match key.key.as_str() {
                        "backspace" | "delete" => {
                            self.native_input
                                .session
                                .buffer
                                .delete(key.key == "backspace", owner.limits());
                            self.publish_native_text(cx);
                        }
                        "left" | "right" | "home" | "end" => {
                            self.native_input
                                .session
                                .buffer
                                .move_cursor(&key.key, mods.shift);
                            cx.notify();
                        }
                        _ => return self.native_printable_route(event, &owner),
                    }
                    cx.stop_propagation();
                    return true;
                }
            }
            self.native_printable_route(event, &owner)
        }

        fn native_printable_route(&mut self, event: &KeyDownEvent, owner: &Owner) -> bool {
            let key = &event.keystroke;
            let m = key.modifiers;
            let printable = key.key == "space"
                || key.key.chars().count() == 1
                || key
                    .key_char
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.chars().all(|c| !c.is_control()));
            let native = text_input::key_route(
                owner.terminal(),
                self.macos_option_as_alt,
                m.control,
                m.platform,
                m.function,
                m.alt,
                printable,
            ) == KeyRoute::Native;
            if native && owner.terminal() && !m.alt {
                self.native_input.pending_key = Some(event.clone());
            }
            native
        }

        pub(crate) fn native_terminal_key(
            &mut self,
            source: &gpui::Keystroke,
            held: bool,
        ) -> gpui::Keystroke {
            self.native_input
                .terminal_key(source, self.macos_option_as_alt, held)
        }

        pub(crate) fn native_terminal_release(
            &mut self,
            source: &gpui::Keystroke,
        ) -> gpui::Keystroke {
            self.native_input.terminal_release(source)
        }

        pub(crate) fn native_clipboard_recipient(
            &self,
        ) -> Option<crate::clipboard_routing::Recipient> {
            use crate::clipboard_routing::Recipient;
            let owner = self.native_owner()?;
            if self.native_input.session.owner.as_ref() != Some(&owner) {
                return None;
            }
            Some(match owner {
                Owner::Terminal { pane, attachment } => Recipient::Terminal {
                    pane,
                    attachment,
                    focus: self.native_input.session.generation,
                },
                _ => Recipient::Field(self.native_input.session.generation),
            })
        }

        pub(crate) fn native_field_selection(&self) -> bool {
            self.native_clipboard_recipient()
                .is_some_and(|r| matches!(r, crate::clipboard_routing::Recipient::Field(_)))
                && !self.native_input.session.buffer.selection.is_empty()
        }

        pub(crate) fn native_field_edit(
            &mut self,
            action: crate::clipboard_routing::EditAction,
            cx: &mut Context<Self>,
        ) -> bool {
            use crate::clipboard_routing::EditAction;
            self.sync_native_input();
            let Some(owner) = self
                .native_input
                .session
                .owner
                .clone()
                .filter(|o| !o.terminal())
            else {
                return false;
            };
            match action {
                EditAction::Copy | EditAction::Cut => {
                    let buffer = &self.native_input.session.buffer;
                    if !buffer.selection.is_empty() {
                        cx.write_to_clipboard(ClipboardItem::new_string(
                            buffer.text[buffer.selection.clone()].into(),
                        ));
                        if action == EditAction::Cut {
                            let marked = buffer.marked.is_some();
                            let selected = to_utf16(&buffer.text, buffer.selection.clone());
                            self.native_input.session.buffer.replace(
                                Some(selected),
                                "",
                                None,
                                false,
                                owner.limits(),
                            );
                            self.publish_native_text(cx);
                            if marked {
                                self.native_input.discard_pending = true;
                                self.native_input.session.reject_commit = true;
                            }
                        }
                    }
                }
                EditAction::SelectAll => {
                    let buffer = &mut self.native_input.session.buffer;
                    buffer.selection = 0..buffer.text.len();
                    buffer.reversed = false;
                    cx.notify();
                }
                EditAction::Paste => return false,
            }
            true
        }

        pub(crate) fn paste_native_field(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
            self.sync_native_input();
            let Some(owner) = self
                .native_input
                .session
                .owner
                .clone()
                .filter(|o| !o.terminal())
            else {
                return false;
            };
            let was_marked = self.native_input.session.buffer.marked.is_some();
            self.native_input
                .session
                .buffer
                .replace(None, text, None, false, owner.limits());
            self.publish_native_text(cx);
            if was_marked {
                self.native_input.discard_pending = true;
                self.native_input.session.reject_commit = true;
                self.native_input.session.generation =
                    self.native_input.session.generation.wrapping_add(1);
            }
            true
        }

        pub(super) fn native_text_element(
            &self,
            owner: Owner,
            fallback: String,
            terminal_cursor: Option<(usize, usize)>,
            cx: &mut Context<Self>,
        ) -> gpui::AnyElement {
            let session = &self.native_input.session;
            let active = session.owner.as_ref() == Some(&owner);
            let text = if active {
                session.buffer.text.clone()
            } else {
                fallback.clone()
            };
            let marked = active.then(|| session.buffer.marked.clone()).flatten();
            let selection = if active {
                session.buffer.selection.clone()
            } else {
                0..0
            };
            let cursor = if active && session.buffer.reversed {
                selection.start
            } else {
                selection.end
            };
            let generation = session.generation;
            let view = cx.entity();
            let focus = self.focus_handle.clone();
            let terminal = owner.terminal();
            let accessible_text = text.clone();
            let accessible_selection = selection.clone();
            let accessible_reversed = active && session.buffer.reversed;
            let accessible_label = match &owner {
                Owner::Rename(_) => "Resource name",
                Owner::Project => "Search projects",
                Owner::Git => "Search worktrees",
                Owner::Conversation => "Search conversations",
                Owner::Setting(_) => "Setting value",
                Owner::Terminal { .. } => "Terminal input",
            };
            let multiline = matches!(owner, Owner::Setting(_));
            let line_count = text.split('\n').count().clamp(1, 6);
            let element = canvas(
                move |bounds, window, _| {
                    let style = window.text_style();
                    let text_to_shape = if text.is_empty() {
                        fallback.as_str()
                    } else {
                        text.as_str()
                    };
                    let font = if terminal {
                        font("Menlo")
                    } else {
                        style.font()
                    };
                    let font_size = if terminal {
                        px(14.)
                    } else {
                        style.font_size.to_pixels(window.rem_size())
                    };
                    let height = if terminal {
                        px(TERMINAL_CELL_HEIGHT)
                    } else {
                        px(20.)
                    };
                    let mut lines = Vec::new();
                    let mut offset = 0;
                    let cursor_row = text[..cursor.min(text.len())]
                        .bytes()
                        .filter(|c| *c == b'\n')
                        .count();
                    let first_row = cursor_row.saturating_sub(5);
                    let (col, row) = terminal_cursor.unwrap_or((0, 0));
                    let origin = if terminal {
                        point(
                            bounds.left() + px(8. + col as f32 * TERMINAL_CELL_WIDTH),
                            bounds.top() + px(8. + row as f32 * TERMINAL_CELL_HEIGHT),
                        )
                    } else {
                        bounds.origin
                    };
                    for (row, content) in text_to_shape.split('\n').enumerate() {
                        let start = offset;
                        offset += content.len() + 1;
                        if row < first_row || row >= first_row + 6 {
                            continue;
                        }
                        let run = TextRun {
                            len: content.len(),
                            font: font.clone(),
                            color: style.color,
                            ..Default::default()
                        };
                        let line = window.text_system().shape_line(
                            content.to_string().into(),
                            font_size,
                            &[run],
                            None,
                        );
                        let cursor_x = if (start..=start + content.len()).contains(&cursor) {
                            line.x_for_index(cursor - start)
                        } else {
                            px(0.)
                        };
                        let available = (bounds.right() - origin.x).max(px(1.));
                        let scroll = (cursor_x - available + px(2.)).max(px(0.));
                        let point = point(
                            origin.x - scroll,
                            (origin.y + height * (row - first_row) as f32)
                                .min((bounds.bottom() - height).max(bounds.top())),
                        );
                        lines.push(PaintedLine {
                            start,
                            origin: point,
                            line,
                        });
                    }
                    (lines, height, marked, selection, text.is_empty(), owner)
                },
                move |bounds, (lines, height, marked, selection, empty, owner), window, cx| {
                    if terminal && marked.is_some() {
                        for line in &lines {
                            window.paint_quad(fill(
                                Bounds::new(line.origin, size(line.line.width.max(px(1.)), height)),
                                rgb(0x181825),
                            ));
                        }
                    }
                    for painted in &lines {
                        let row_end = painted.start + painted.line.len();
                        if !selection.is_empty() && !terminal {
                            let start =
                                selection.start.max(painted.start).min(row_end) - painted.start;
                            let end = selection.end.max(painted.start).min(row_end) - painted.start;
                            if start < end {
                                let x = painted.line.x_for_index(start);
                                window.paint_quad(fill(
                                    Bounds::new(
                                        point(painted.origin.x + x, painted.origin.y),
                                        size(painted.line.x_for_index(end) - x, height),
                                    ),
                                    rgba(0x89b4fa55),
                                ));
                            }
                        }
                        if !terminal || marked.is_some() {
                            let _ = painted.line.paint(
                                painted.origin,
                                height,
                                gpui::TextAlign::Left,
                                None,
                                window,
                                cx,
                            );
                        }
                        if let Some(marked) = &marked {
                            let start =
                                marked.start.max(painted.start).min(row_end) - painted.start;
                            let end = marked.end.max(painted.start).min(row_end) - painted.start;
                            let x = painted.line.x_for_index(start);
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(painted.origin.x + x, painted.origin.y + height - px(1.)),
                                    size((painted.line.x_for_index(end) - x).max(px(1.)), px(1.)),
                                ),
                                rgb(0x89b4fa),
                            ));
                        }
                        if !terminal
                            && selection.is_empty()
                            && (painted.start..=row_end).contains(&cursor)
                        {
                            let x = if empty {
                                px(0.)
                            } else {
                                painted.line.x_for_index(cursor - painted.start)
                            };
                            window.paint_quad(fill(
                                Bounds::new(
                                    point(painted.origin.x + x, painted.origin.y),
                                    size(px(1.), height),
                                ),
                                rgb(0xcdd6f4),
                            ));
                        }
                    }
                    let valid = view.update(cx, |workspace, _| {
                        if workspace.native_input.session.accepts(&owner, generation)
                            && workspace.native_owner().as_ref() == Some(&owner)
                        {
                            workspace.native_input.lines = lines;
                            workspace.native_input.bounds = Some(bounds);
                            true
                        } else {
                            false
                        }
                    });
                    if valid {
                        window.handle_input(
                            &focus,
                            GuardedHandler {
                                inner: ElementInputHandler::new(bounds, view.clone()),
                                view: view.clone(),
                                owner,
                                generation,
                                terminal,
                            },
                            cx,
                        );
                    }
                },
            )
            .size_full();
            if terminal {
                div()
                    .absolute()
                    .size_full()
                    .overflow_hidden()
                    .child(element)
                    .into_any_element()
            } else {
                div()
                    .id("native-editable-text")
                    .role(if multiline {
                        gpui::Role::MultilineTextInput
                    } else {
                        gpui::Role::TextInput
                    })
                    .aria_label(accessible_label)
                    .aria_active_descendant()
                    .a11y_synthetic_children(move |builder| {
                        let id = builder.synthetic_node_id("value");
                        let mut node = gpui::accesskit::Node::new(gpui::Role::TextRun);
                        node.set_value(accessible_text.clone());
                        node.set_character_lengths(
                            accessible_text
                                .chars()
                                .map(|c| c.len_utf8() as u8)
                                .collect::<Vec<_>>(),
                        );
                        builder.push_child(id, node);
                        let start = accessible_text[..accessible_selection.start]
                            .chars()
                            .count();
                        let end = accessible_text[..accessible_selection.end].chars().count();
                        let (anchor, focus) = if accessible_reversed {
                            (end, start)
                        } else {
                            (start, end)
                        };
                        builder
                            .parent_node()
                            .set_text_selection(gpui::accesskit::TextSelection {
                                anchor: gpui::accesskit::TextPosition {
                                    node: id,
                                    character_index: anchor,
                                },
                                focus: gpui::accesskit::TextPosition {
                                    node: id,
                                    character_index: focus,
                                },
                            });
                    })
                    .w_full()
                    .h(px(20. * line_count as f32))
                    .overflow_hidden()
                    .child(element)
                    .into_any_element()
            }
        }
    }

    impl EntityInputHandler for Workspace {
        fn text_for_range(
            &mut self,
            range: Range<usize>,
            adjusted: &mut Option<Range<usize>>,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<String> {
            if !self.native_ready(window) {
                return None;
            }
            let text = &self.native_input.session.buffer.text;
            let range = from_utf16(text, range);
            *adjusted = Some(to_utf16(text, range.clone()));
            Some(text[range].into())
        }
        fn selected_text_range(
            &mut self,
            _: bool,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<UTF16Selection> {
            if !self.native_ready(window) {
                return None;
            }
            let buffer = &self.native_input.session.buffer;
            Some(UTF16Selection {
                range: to_utf16(&buffer.text, buffer.selection.clone()),
                reversed: buffer.reversed,
            })
        }
        fn marked_text_range(
            &self,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<Range<usize>> {
            if !self.native_ready(window) {
                return None;
            }
            let buffer = &self.native_input.session.buffer;
            buffer
                .marked
                .clone()
                .map(|range| to_utf16(&buffer.text, range))
        }
        fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
            // Cancellation must never execute uncommitted text in a shell.
            if self.native_input.session.buffer.marked.is_some() {
                self.cancel_native_input();
                cx.notify();
            }
        }
        fn replace_text_in_range(
            &mut self,
            range: Option<Range<usize>>,
            text: &str,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if !self.native_ready(window) || self.native_input.session.reject_commit {
                return;
            }
            let Some(owner) = self.native_input.session.owner.clone() else {
                return;
            };
            let pending = self.native_input.pending_key.take();
            if let (Owner::Terminal { pane, .. }, Some(pending)) = (&owner, pending)
                && text_input::preserves_physical_key(
                    self.native_input.session.buffer.marked.is_some(),
                    range.is_some(),
                    pending.keystroke.key_char.as_deref(),
                    text,
                )
            {
                let sent = self
                    .terminals
                    .get(pane)
                    .and_then(|p| p.session.as_ref())
                    .is_some_and(|terminal| {
                        terminal.send_key(
                            &pending.keystroke,
                            if pending.is_held {
                                libghostty_vt::key::Action::Repeat
                            } else {
                                libghostty_vt::key::Action::Press
                            },
                        )
                    });
                if sent {
                    if !pending.is_held {
                        self.terminal_pressed_keys
                            .insert(pending.keystroke.key, *pane);
                    }
                    if let Some(pane) = self.terminals.get_mut(pane) {
                        pane.selection = None;
                    }
                }
                self.native_input.session.buffer.reset("");
                cx.notify();
                return;
            }
            self.native_input
                .session
                .buffer
                .replace(range, text, None, false, owner.limits());
            self.publish_native_text(cx);
        }
        fn replace_and_mark_text_in_range(
            &mut self,
            range: Option<Range<usize>>,
            text: &str,
            selected: Option<Range<usize>>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if !self.native_ready(window) {
                return;
            }
            let Some(owner) = &self.native_input.session.owner else {
                return;
            };
            self.native_input.pending_key = None;
            self.native_input.session.reject_commit = false;
            self.native_input
                .session
                .buffer
                .replace(range, text, selected, true, owner.limits());
            cx.notify();
        }
        fn bounds_for_range(
            &mut self,
            range: Range<usize>,
            _: Bounds<Pixels>,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<Bounds<Pixels>> {
            if !self.native_ready(window) {
                return None;
            }
            let range = from_utf16(&self.native_input.session.buffer.text, range);
            let line = self
                .native_input
                .lines
                .iter()
                .rev()
                .find(|line| line.start <= range.start)?;
            let start = (range.start - line.start).min(line.line.len());
            let end = range.end.saturating_sub(line.start).min(line.line.len());
            let bounds = self.native_input.bounds?;
            let x = (line.origin.x + line.line.x_for_index(start))
                .max(bounds.left())
                .min(bounds.right());
            let height = if self.native_input.session.owner.as_ref()?.terminal() {
                px(TERMINAL_CELL_HEIGHT)
            } else {
                px(20.)
            };
            Some(Bounds::new(
                point(x, line.origin.y),
                size(
                    (line.line.x_for_index(end) - line.line.x_for_index(start))
                        .max(px(1.))
                        .min((bounds.right() - x).max(px(1.))),
                    height,
                ),
            ))
        }
        fn character_index_for_point(
            &mut self,
            point: Point<Pixels>,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<usize> {
            if !self.native_ready(window) || !self.native_input.bounds?.contains(&point) {
                return None;
            }
            let line = self
                .native_input
                .lines
                .iter()
                .rev()
                .find(|line| line.origin.y <= point.y)?;
            let byte = (line.start + line.line.closest_index_for_x(point.x - line.origin.x))
                .min(self.native_input.session.buffer.text.len());
            Some(to_utf16(&self.native_input.session.buffer.text, byte..byte).start)
        }
        fn set_selected_text_range(
            &mut self,
            range: Range<usize>,
            window: &mut Window,
            cx: &mut Context<Self>,
        ) {
            if self.native_ready(window) {
                self.native_input.session.buffer.select_utf16(range);
                cx.notify();
            }
        }
        fn text_length_utf16(
            &mut self,
            window: &mut Window,
            _: &mut Context<Self>,
        ) -> Option<usize> {
            self.native_ready(window)
                .then(|| self.native_input.session.buffer.text.encode_utf16().count())
        }
        fn accepts_text_input(&self, window: &mut Window, _: &mut Context<Self>) -> bool {
            self.native_ready(window)
        }
    }

    /// The exact painted recipient/generation guards all callbacks. Repeat
    /// handling stays raw in terminals, including macOS press-and-hold.
    struct GuardedHandler {
        inner: ElementInputHandler<Workspace>,
        view: Entity<Workspace>,
        owner: Owner,
        generation: u64,
        terminal: bool,
    }
    impl GuardedHandler {
        fn valid(&self, window: &Window, cx: &App) -> bool {
            let view = self.view.read(cx);
            view.native_ready(window)
                && view
                    .native_input
                    .session
                    .accepts(&self.owner, self.generation)
        }
    }
    impl InputHandler for GuardedHandler {
        fn selected_text_range(
            &mut self,
            ignored: bool,
            window: &mut Window,
            cx: &mut App,
        ) -> Option<UTF16Selection> {
            if self.valid(window, cx) {
                self.inner.selected_text_range(ignored, window, cx)
            } else {
                None
            }
        }
        fn marked_text_range(&mut self, window: &mut Window, cx: &mut App) -> Option<Range<usize>> {
            if self.valid(window, cx) {
                self.inner.marked_text_range(window, cx)
            } else {
                None
            }
        }
        fn text_for_range(
            &mut self,
            range: Range<usize>,
            adjusted: &mut Option<Range<usize>>,
            window: &mut Window,
            cx: &mut App,
        ) -> Option<String> {
            if self.valid(window, cx) {
                self.inner.text_for_range(range, adjusted, window, cx)
            } else {
                None
            }
        }
        fn replace_text_in_range(
            &mut self,
            range: Option<Range<usize>>,
            text: &str,
            window: &mut Window,
            cx: &mut App,
        ) {
            if self.valid(window, cx) {
                self.inner.replace_text_in_range(range, text, window, cx);
            }
        }
        fn replace_and_mark_text_in_range(
            &mut self,
            range: Option<Range<usize>>,
            text: &str,
            selected: Option<Range<usize>>,
            window: &mut Window,
            cx: &mut App,
        ) {
            if self.valid(window, cx) {
                self.inner
                    .replace_and_mark_text_in_range(range, text, selected, window, cx);
            }
        }
        fn unmark_text(&mut self, window: &mut Window, cx: &mut App) {
            if self.valid(window, cx) {
                self.inner.unmark_text(window, cx);
            }
        }
        fn bounds_for_range(
            &mut self,
            range: Range<usize>,
            window: &mut Window,
            cx: &mut App,
        ) -> Option<Bounds<Pixels>> {
            if self.valid(window, cx) {
                self.inner.bounds_for_range(range, window, cx)
            } else {
                None
            }
        }
        fn character_index_for_point(
            &mut self,
            point: Point<Pixels>,
            window: &mut Window,
            cx: &mut App,
        ) -> Option<usize> {
            if self.valid(window, cx) {
                self.inner.character_index_for_point(point, window, cx)
            } else {
                None
            }
        }
        fn set_selected_text_range(
            &mut self,
            range: Range<usize>,
            window: &mut Window,
            cx: &mut App,
        ) {
            if self.valid(window, cx) {
                self.inner.set_selected_text_range(range, window, cx);
            }
        }
        fn text_length_utf16(&mut self, window: &mut Window, cx: &mut App) -> Option<usize> {
            if self.valid(window, cx) {
                self.inner.text_length_utf16(window, cx)
            } else {
                None
            }
        }
        fn accepts_text_input(&mut self, window: &mut Window, cx: &mut App) -> bool {
            self.valid(window, cx)
        }
        fn prefers_ime_for_printable_keys(&mut self, window: &mut Window, cx: &mut App) -> bool {
            self.valid(window, cx)
                && text_input::ime_first(
                    self.terminal,
                    self.view.read(cx).macos_option_as_alt,
                    window.modifiers().alt,
                )
        }
        fn apple_press_and_hold_enabled(&mut self) -> bool {
            !self.terminal
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn option_shortcuts_keep_the_same_text_on_release() {
            let mut state = State::default();
            let option_shift_two = gpui::Keystroke {
                key: "@".into(),
                key_char: Some("€".into()),
                modifiers: gpui::Modifiers {
                    alt: true,
                    ..Default::default()
                },
            };
            let press = state.terminal_key(&option_shift_two, true, false);
            assert_eq!(press.key_char.as_deref(), Some("@"));
            // Release keeps the press identity even after the policy changes.
            let release = state.terminal_release(&option_shift_two);
            assert_eq!(release.key_char, press.key_char);
            assert!(state.alt_pressed.is_empty());
            assert_eq!(
                state
                    .terminal_key(&option_shift_two, false, false)
                    .key_char
                    .as_deref(),
                Some("€")
            );
        }

        #[test]
        fn key_up_clears_uncommitted_pending_key_metadata() {
            let mut state = State::default();
            let key = gpui::Keystroke {
                key: "'".into(),
                key_char: Some("'".into()),
                modifiers: Default::default(),
            };
            state.pending_key = Some(KeyDownEvent {
                keystroke: key.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            state.terminal_release(&key);
            assert!(state.pending_key.is_none());
        }
    }
}
