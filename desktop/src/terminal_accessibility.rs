//! Bounded visible text projection. Never reads or copies terminal scrollback.
//! Character offsets are Unicode scalar indices, as expected by AccessKit's
//! character_lengths vectors, rather than UTF-16 offsets used by AppKit input.
pub(crate) const MAX_BYTES: usize = 32 * 1024;
pub(crate) const MAX_ROWS: usize = 128;
pub(crate) const MAX_COLUMNS: usize = 512;

pub(crate) struct Cell<'a> {
    pub text: &'a str,
    pub continuation: bool,
    pub wide: bool,
}
#[derive(Debug)]
pub(crate) struct Line {
    pub text: String,
    pub lengths: Vec<u8>,
    pub columns: Vec<usize>,
    pub widths: Vec<u8>,
    pub column_offsets: Vec<usize>,
}
#[derive(Debug)]
pub(crate) struct Projection {
    pub lines: Vec<Line>,
    pub truncated: bool,
}
impl Projection {
    pub fn build<'a>(rows: usize, cols: usize, cell: impl Fn(usize, usize) -> Cell<'a>) -> Self {
        let mut remaining = MAX_BYTES;
        let mut lines = Vec::new();
        let mut truncated = rows > MAX_ROWS || cols > MAX_COLUMNS;
        for row in 0..rows.min(MAX_ROWS) {
            if remaining == 0 {
                truncated = true;
                break;
            }
            // Reserve the separator added by the accessibility adapter.
            if row + 1 < rows.min(MAX_ROWS) {
                remaining = remaining.saturating_sub(1);
            }
            let mut line = Line {
                text: String::new(),
                lengths: Vec::new(),
                columns: Vec::new(),
                widths: Vec::new(),
                column_offsets: Vec::new(),
            };
            for col in 0..cols.min(MAX_COLUMNS) {
                line.column_offsets.push(line.lengths.len());
                let cell = cell(row, col);
                if cell.continuation {
                    continue;
                }
                for (index, ch) in cell.text.chars().filter(|ch| !ch.is_control()).enumerate() {
                    if ch.len_utf8() > remaining {
                        truncated = true;
                        break;
                    }
                    line.text.push(ch);
                    line.lengths.push(ch.len_utf8() as u8);
                    line.columns.push(col);
                    line.widths.push(if index > 0 {
                        0
                    } else if cell.wide {
                        2
                    } else {
                        1
                    });
                    remaining -= ch.len_utf8();
                }
                if remaining == 0 {
                    truncated = true;
                    break;
                }
            }
            line.column_offsets.push(line.lengths.len());
            // Avoid announcing padding at the right edge. Clamp column maps
            // to the retained text, so cursor/selection offsets stay valid.
            let trimmed = line.text.trim_end().len();
            line.text.truncate(trimmed);
            let chars = line.text.chars().count();
            line.lengths.truncate(chars);
            line.columns.truncate(chars);
            line.widths.truncate(chars);
            for offset in &mut line.column_offsets {
                *offset = (*offset).min(chars);
            }
            lines.push(line);
        }
        Self { lines, truncated }
    }

    pub fn position(&self, row: usize, col: usize) -> Option<(usize, usize)> {
        let row = row.min(self.lines.len().checked_sub(1)?);
        let line = &self.lines[row];
        let offset = line
            .column_offsets
            .get(col)
            .copied()
            .unwrap_or(line.lengths.len());
        Some((row, offset))
    }

    /// Project an inclusive terminal selection into the retained viewport. An
    /// entirely offscreen selection is omitted, not misreported as a caret.
    pub fn selection(
        &self,
        first_row: usize,
        anchor: (usize, usize),
        head: (usize, usize),
    ) -> Option<((usize, usize), (usize, usize))> {
        let last_row = first_row.saturating_add(self.lines.len().checked_sub(1)?);
        let (lo, hi) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        if hi.0 < first_row || lo.0 > last_row {
            return None;
        }
        let start = self.position(
            lo.0.saturating_sub(first_row),
            if lo.0 < first_row { 0 } else { lo.1 },
        )?;
        let end = self.position(
            hi.0.saturating_sub(first_row),
            if hi.0 > last_row {
                usize::MAX
            } else {
                hi.1.saturating_add(1)
            },
        )?;
        Some(if anchor <= head {
            (start, end)
        } else {
            (end, start)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unicode_and_wide_continuations_preserve_character_offsets() {
        let cells = [
            ("a", false),
            ("😀", false),
            ("", true),
            ("e\u{301}", false),
            (" ", false),
        ];
        let projection = Projection::build(1, cells.len(), |_, col| Cell {
            text: cells[col].0,
            continuation: cells[col].1,
            wide: col == 1,
        });
        let line = &projection.lines[0];
        assert_eq!(line.text, "a😀e\u{301}");
        assert_eq!(line.lengths, [1, 4, 1, 2]);
        assert_eq!(projection.position(0, 3), Some((0, 2)));
        assert_eq!(projection.position(0, 5), Some((0, 4)));
    }
    #[test]
    fn output_and_metadata_are_bounded_for_large_grids() {
        let projection = Projection::build(usize::MAX, usize::MAX, |_, _| Cell {
            text: "😀",
            continuation: false,
            wide: false,
        });
        assert!(projection.truncated);
        assert!(projection.lines.len() <= MAX_ROWS);
        assert!(projection.lines.iter().map(|l| l.text.len()).sum::<usize>() <= MAX_BYTES);
        assert!(
            projection
                .lines
                .iter()
                .all(|l| l.column_offsets.len() <= MAX_COLUMNS + 1)
        );
    }
    #[test]
    fn malicious_cell_text_is_bounded_and_control_free() {
        let large = format!("\x1b\n{}", "é".repeat(MAX_BYTES));
        let projection = Projection::build(1, 1, |_, _| Cell {
            text: &large,
            continuation: false,
            wide: false,
        });
        assert!(projection.truncated);
        assert_eq!(projection.lines[0].text.len(), MAX_BYTES);
        assert!(!projection.lines[0].text.chars().any(char::is_control));
    }
    #[test]
    fn selection_is_clipped_to_visible_text_and_keeps_direction() {
        let projection = Projection::build(2, 3, |_, _| Cell {
            text: "x",
            continuation: false,
            wide: false,
        });
        assert_eq!(projection.selection(10, (0, 0), (9, 2)), None);
        assert_eq!(projection.selection(10, (12, 0), (15, 2)), None);
        assert_eq!(
            projection.selection(10, (0, 0), (99, 2)),
            Some(((0, 0), (1, 3)))
        );
        assert_eq!(
            projection.selection(10, (11, 1), (10, 1)),
            Some(((1, 2), (0, 1)))
        );
    }
    #[test]
    fn empty_views_and_clipped_positions_are_safe() {
        let empty = Projection::build(0, 0, |_, _| unreachable!());
        assert_eq!(empty.position(0, 0), None);
        let one = Projection::build(1, 1, |_, _| Cell {
            text: "x",
            continuation: false,
            wide: false,
        });
        assert_eq!(one.position(usize::MAX, usize::MAX), Some((0, 1)));
    }
}
