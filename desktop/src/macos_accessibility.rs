//! A bounded, on-demand accessibility projection for the macOS terminal canvas.
use crate::*;

impl Workspace {
    pub(crate) fn accessible_terminal(
        &self,
        element: Stateful<Div>,
        id: usize,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        #[cfg(target_os = "macos")]
        {
            self.decorate_accessible_terminal(element, id, cx)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (id, cx);
            element
        }
    }
}

#[cfg(any(target_os = "macos", test))]
pub(crate) use native::Cache;

#[cfg(any(target_os = "macos", test))]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod native {
    use super::*;
    use crate::terminal_accessibility::{Cell, Projection};
    use gpui::accesskit::{
        Node as AccessibleNode, Rect as AccessibleRect, TextDirection, TextPosition, TextSelection,
    };
    use std::sync::Weak;

    #[derive(Default)]
    pub(crate) struct Cache {
        screen: Weak<TerminalScreen>,
        projection: Option<Arc<Projection>>,
    }
    impl Cache {
        fn get(&mut self, screen: &Arc<TerminalScreen>) -> Arc<Projection> {
            let weak = Arc::downgrade(screen);
            if !self.screen.ptr_eq(&weak) || self.projection.is_none() {
                let cols = usize::from(screen.cols);
                self.projection = Some(Arc::new(Projection::build(
                    usize::from(screen.rows),
                    cols,
                    |row, col| {
                        screen.cells.get(row * cols + col).map_or(
                            Cell {
                                text: "",
                                continuation: false,
                                wide: false,
                            },
                            |cell| Cell {
                                text: &cell.text,
                                continuation: cell.continuation,
                                wide: cell.wide,
                            },
                        )
                    },
                )));
                self.screen = weak;
            }
            Arc::clone(
                self.projection
                    .as_ref()
                    .expect("initialized accessibility projection"),
            )
        }
    }

    impl Workspace {
        pub(super) fn decorate_accessible_terminal(
            &self,
            element: Stateful<Div>,
            id: usize,
            cx: &mut Context<Self>,
        ) -> Stateful<Div> {
            let Some(pane) = self.terminals.get(&id) else {
                return element;
            };
            if self.native_input.rendering_frozen {
                return element;
            }
            let title = pane.shell.as_ref().map_or("Terminal", |s| s.name.as_str());
            let title: String = title.chars().take(128).collect();
            let active = self.native_clipboard_recipient().is_some_and(|recipient| matches!(recipient, crate::clipboard_routing::Recipient::Terminal { pane, .. } if pane == id));
            let selection = pane.selection;
            let screen = pane.screen.clone();
            let cache = std::rc::Rc::clone(&pane.accessibility);
            let view = cx.entity();
            let element = element
                .role(gpui::Role::Terminal)
                .aria_label(format!("Terminal: {title}"))
                .aria_description("Visible terminal output. Control-Space enters pane navigation. Cut does not remove terminal output.")
                .when(active, |e| e.aria_active_descendant())
                .on_a11y_action(gpui::AccessibleAction::Focus, move |_, window, cx| {
                    view.update(cx, |this, cx| {
                        if window.is_window_active() && this.keyboard_input_target() == InputTarget::Workspace
                            && !this.settings_open && this.theme_candidate.is_none() && !this.layout_frozen
                        { this.focus_terminal_pane(id, window, cx); }
                    });
                });
            element.a11y_synthetic_children(move |builder| {
                let Some(screen) = screen else { return; };
                let projection = cache.borrow_mut().get(&screen);
                builder.parent_node().set_read_only();
                let bounds = builder.parent_node().bounds();
                let mut ids = Vec::with_capacity(projection.lines.len());
                for (row, line) in projection.lines.iter().enumerate() {
                    let node_id = builder.synthetic_node_id(("terminal-line", screen.scroll_offset.saturating_add(row as u64)));
                    ids.push(node_id);
                    let mut node = AccessibleNode::new(gpui::Role::TextRun);
                    let mut text = line.text.clone();
                    let mut lengths = line.lengths.clone();
                    let mut positions = line.columns.iter().map(|col| *col as f32 * TERMINAL_CELL_WIDTH).collect::<Vec<_>>();
                    let mut widths = line.widths.iter().map(|width| f32::from(*width) * TERMINAL_CELL_WIDTH).collect::<Vec<_>>();
                    if row + 1 < projection.lines.len() {
                        text.push('\n'); lengths.push(1);
                        positions.push(line.column_offsets.len().saturating_sub(1) as f32 * TERMINAL_CELL_WIDTH);
                        widths.push(0.);
                    }
                    node.set_value(text);
                    node.set_character_lengths(lengths);
                    node.set_character_positions(positions);
                    node.set_character_widths(widths);
                    node.set_text_direction(TextDirection::LeftToRight);
                    if let Some(bounds) = bounds {
                        let y = (bounds.y0 + 8. + row as f64 * f64::from(TERMINAL_CELL_HEIGHT)).min(bounds.y1);
                        node.set_bounds(AccessibleRect { x0: (bounds.x0 + 8.).min(bounds.x1), y0: y, x1: bounds.x1, y1: (y + f64::from(TERMINAL_CELL_HEIGHT)).min(bounds.y1) });
                    }
                    builder.push_child(node_id, node);
                }
                let offset = usize::try_from(screen.scroll_offset).unwrap_or(usize::MAX);
                let positions = selection.and_then(|selection| projection.selection(offset, selection.anchor, selection.head)).or_else(|| {
                    if selection.is_some() { return None; }
                    let (col, row) = screen.input_cursor?;
                    let row = usize::from(row);
                    if row >= projection.lines.len() || usize::from(col) >= projection.lines[row].column_offsets.len() { return None; }
                    let position = projection.position(row, usize::from(col))?;
                    Some((position, position))
                });
                if let Some((anchor, focus)) = positions {
                    builder.parent_node().set_text_selection(TextSelection {
                        anchor: TextPosition { node: ids[anchor.0], character_index: anchor.1 },
                        focus: TextPosition { node: ids[focus.0], character_index: focus.1 },
                    });
                }
                if projection.truncated {
                    builder.parent_node().set_description("Visible terminal text is truncated to 32 KiB, 128 rows and 512 columns. Scrollback is not part of this accessibility view.");
                }
            })
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn projection_cache_reuses_content_without_retaining_the_screen() {
            let screen = Arc::new(TerminalScreen {
                background: 0,
                rows: 1,
                cols: 1,
                cells: Vec::new(),
                input_cursor: None,
                scroll_total: 1,
                scroll_offset: 0,
                scroll_len: 1,
                images: Vec::new(),
                image_placements: Vec::new(),
            });
            let mut cache = Cache::default();
            let first = cache.get(&screen);
            let second = cache.get(&screen);
            assert!(Arc::ptr_eq(&first, &second));
            assert_eq!(Arc::strong_count(&screen), 1);
            drop(screen);
            assert!(cache.screen.upgrade().is_none());
        }
    }
}
