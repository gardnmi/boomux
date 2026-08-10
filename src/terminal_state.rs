use crate::terminal_focus::FocusMode;

const SCROLLBACK_ROWS: usize = 2_000;
const MAX_RECONSTRUCTION_BYTES: usize = 1024 * 1024;

pub(crate) struct TerminalState {
    parser: vt100::Parser,
    primary_before_alternate: Option<vt100::Screen>,
    focus_mode: FocusMode,
}

impl TerminalState {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        Self {
            parser,
            primary_before_alternate: None,
            focus_mode: FocusMode::default(),
        }
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.focus_mode.process(bytes);
        let was_alternate = self.parser.screen().alternate_screen();
        if !was_alternate && let Some(offset) = alternate_screen_start(bytes) {
            self.parser.process(&bytes[..offset]);
            self.primary_before_alternate = Some(self.parser.screen().clone());
            self.parser.process(&bytes[offset..]);
            if !self.parser.screen().alternate_screen() {
                self.primary_before_alternate = None;
            }
            return;
        }
        self.parser.process(bytes);
        if was_alternate && !self.parser.screen().alternate_screen() {
            self.primary_before_alternate = None;
        }
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        if self.parser.screen().size() == (rows, cols) {
            return;
        }

        let screen = self.parser.screen();
        let primary = self.primary_before_alternate.as_ref().unwrap_or(screen);
        let mut reconstruction = reflowable_contents_formatted(primary);
        if screen.alternate_screen() {
            reconstruction.extend_from_slice(b"\x1b[?1049h");
            reconstruction.extend(screen.contents_formatted());
        } else {
            CellStyle::from_screen(screen).write_escape_code(&mut reconstruction);
            reconstruction.extend_from_slice(if screen.hide_cursor() {
                b"\x1b[?25l"
            } else {
                b"\x1b[?25h"
            });
        }
        reconstruction.extend(screen.input_mode_formatted());
        reconstruction.extend(self.focus_mode.restore_sequence());

        let mut resized = Self::new(rows, cols);
        resized.process(&reconstruction);
        *self = resized;
    }

    pub(crate) fn reconstruction(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let primary = self.primary_before_alternate.as_ref().unwrap_or(screen);
        let mut output = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[H\x1b[2J".to_vec();
        append_scrollback(&mut output, primary);
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        output.extend(primary.contents_formatted());
        if screen.alternate_screen() {
            output.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
            output.extend(screen.contents_formatted());
        }
        output.extend(screen.input_mode_formatted());
        output.extend(self.focus_mode.restore_sequence());
        if output.len() <= MAX_RECONSTRUCTION_BYTES {
            return output;
        }

        let mut fallback = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[H\x1b[2J".to_vec();
        if screen.alternate_screen() {
            fallback.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        }
        let suffix = state_suffix(screen);
        let focus_mode = self.focus_mode.restore_sequence();
        let text_limit = MAX_RECONSTRUCTION_BYTES
            .saturating_sub(suffix.len())
            .saturating_sub(focus_mode.len());
        append_terminal_text_bounded(&mut fallback, &screen.contents(), text_limit);
        fallback.extend(suffix);
        fallback.extend(focus_mode);
        fallback
    }

    pub(crate) fn plain_text(&self) -> String {
        let screen = self.parser.screen();
        let mut text = scrollback_text(screen);
        text.push_str(&screen.contents());
        text.chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect()
    }
}

fn alternate_screen_start(bytes: &[u8]) -> Option<usize> {
    [b"\x1b[?1049h".as_slice(), b"\x1b[?1047h", b"\x1b[?47h"]
        .into_iter()
        .filter_map(|sequence| {
            bytes
                .windows(sequence.len())
                .position(|window| window == sequence)
        })
        .min()
}

fn scrollback_text(screen: &vt100::Screen) -> String {
    let mut screen = screen.clone();
    screen.set_scrollback(usize::MAX);
    let rows = screen.scrollback();
    let (_, cols) = screen.size();
    let mut contents = String::new();

    for offset in (1..=rows).rev() {
        screen.set_scrollback(offset);
        if let Some(row) = screen.rows(0, cols).next() {
            contents.push_str(&row);
            if !screen.row_wrapped(0) {
                contents.push('\n');
            }
        }
    }
    contents
}

fn append_scrollback(output: &mut Vec<u8>, screen: &vt100::Screen) {
    let mut scrolled = screen.clone();
    scrolled.set_scrollback(usize::MAX);
    let scrollback_rows = scrolled.scrollback();
    if scrollback_rows == 0 {
        return;
    }
    for offset in (1..=scrollback_rows).rev() {
        scrolled.set_scrollback(offset);
        append_reflowable_row(output, &scrolled, 0, true);
    }
    for _ in 0..screen.size().0.saturating_sub(1) {
        output.extend_from_slice(b"\r\n");
    }
}

fn append_terminal_text_bounded(output: &mut Vec<u8>, text: &str, limit: usize) {
    for character in text
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
    {
        if character == '\n' {
            if output.len() + 2 > limit {
                break;
            }
            output.extend_from_slice(b"\r\n");
            continue;
        }
        let mut bytes = [0; 4];
        let encoded = character.encode_utf8(&mut bytes).as_bytes();
        if output.len() + encoded.len() > limit {
            break;
        }
        output.extend_from_slice(encoded);
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct CellStyle {
    foreground: vt100::Color,
    background: vt100::Color,
    bold: bool,
    dim: bool,
    italic: bool,
    underline: bool,
    inverse: bool,
}

impl CellStyle {
    fn from_cell(cell: &vt100::Cell) -> Self {
        Self {
            foreground: cell.fgcolor(),
            background: cell.bgcolor(),
            bold: cell.bold(),
            dim: cell.dim(),
            italic: cell.italic(),
            underline: cell.underline(),
            inverse: cell.inverse(),
        }
    }

    fn from_screen(screen: &vt100::Screen) -> Self {
        Self {
            foreground: screen.fgcolor(),
            background: screen.bgcolor(),
            bold: screen.bold(),
            dim: screen.dim(),
            italic: screen.italic(),
            underline: screen.underline(),
            inverse: screen.inverse(),
        }
    }

    fn write_escape_code(self, output: &mut Vec<u8>) {
        output.extend_from_slice(b"\x1b[0");
        if self.bold {
            output.extend_from_slice(b";1");
        } else if self.dim {
            output.extend_from_slice(b";2");
        }
        if self.italic {
            output.extend_from_slice(b";3");
        }
        if self.underline {
            output.extend_from_slice(b";4");
        }
        if self.inverse {
            output.extend_from_slice(b";7");
        }
        write_color(output, self.foreground, true);
        write_color(output, self.background, false);
        output.push(b'm');
    }
}

fn write_color(output: &mut Vec<u8>, color: vt100::Color, foreground: bool) {
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(index) => {
            output.extend_from_slice(if foreground { b";38;5;" } else { b";48;5;" });
            output.extend_from_slice(index.to_string().as_bytes());
        }
        vt100::Color::Rgb(red, green, blue) => {
            output.extend_from_slice(if foreground { b";38;2;" } else { b";48;2;" });
            output.extend_from_slice(red.to_string().as_bytes());
            output.push(b';');
            output.extend_from_slice(green.to_string().as_bytes());
            output.push(b';');
            output.extend_from_slice(blue.to_string().as_bytes());
        }
    }
}

fn reflowable_contents_formatted(screen: &vt100::Screen) -> Vec<u8> {
    let mut output = Vec::new();
    let mut scrolled = screen.clone();
    scrolled.set_scrollback(usize::MAX);
    let scrollback_rows = scrolled.scrollback();

    for offset in (1..=scrollback_rows).rev() {
        scrolled.set_scrollback(offset);
        append_reflowable_row(&mut output, &scrolled, 0, true);
    }

    scrolled.set_scrollback(0);
    let (cursor_row, _) = scrolled.cursor_position();
    let (rows, cols) = scrolled.size();
    let last_content_row = (0..rows).rfind(|row| {
        (0..cols).any(|col| {
            scrolled.cell(*row, col).is_some_and(|cell| {
                cell.has_contents() || CellStyle::from_cell(cell) != CellStyle::default()
            })
        })
    });
    let last_row = last_content_row.unwrap_or(0).max(cursor_row);
    for row in 0..=last_row {
        append_reflowable_row(&mut output, &scrolled, row, row < last_row);
    }

    output
}

fn append_reflowable_row(output: &mut Vec<u8>, screen: &vt100::Screen, row: u16, terminate: bool) {
    let (_, cols) = screen.size();
    let wrapped = screen.row_wrapped(row);
    let last_col = if wrapped {
        cols.checked_sub(1)
    } else {
        (0..cols).rfind(|col| {
            screen.cell(row, *col).is_some_and(|cell| {
                cell.has_contents() || CellStyle::from_cell(cell) != CellStyle::default()
            })
        })
    };
    let Some(last_col) = last_col else {
        if terminate {
            output.extend_from_slice(b"\r\n");
        }
        return;
    };

    let mut style = CellStyle::default();
    for col in 0..=last_col {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let next_style = CellStyle::from_cell(cell);
        if next_style != style {
            next_style.write_escape_code(output);
            style = next_style;
        }
        if cell.has_contents() {
            output.extend_from_slice(cell.contents().as_bytes());
        } else {
            output.push(b' ');
        }
    }
    if style != CellStyle::default() {
        output.extend_from_slice(b"\x1b[0m");
    }
    if terminate && !wrapped {
        output.extend_from_slice(b"\r\n");
    }
}

fn state_suffix(screen: &vt100::Screen) -> Vec<u8> {
    let (row, col) = screen.cursor_position();
    let mut suffix = format!("\x1b[{};{}H", row + 1, col + 1).into_bytes();
    suffix.extend_from_slice(if screen.hide_cursor() {
        b"\x1b[?25l"
    } else {
        b"\x1b[?25h"
    });
    suffix.extend(screen.input_mode_formatted());
    suffix
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn carriage_return_rewrites_rendered_line() {
        let mut state = TerminalState::new(4, 20);
        state.process(b"abc\rXY");

        assert_eq!(state.plain_text(), "XYc");
    }

    #[test]
    fn reconstruction_omits_osc_side_effects() {
        let mut state = TerminalState::new(4, 40);
        state.process(b"before\x1b]52;c;Y2xpcGJvYXJk\x07after");

        let reconstruction = state.reconstruction();
        assert!(!reconstruction.windows(2).any(|bytes| bytes == b"\x1b]"));
        assert!(!String::from_utf8_lossy(&reconstruction).contains("Y2xpcGJvYXJk"));
        assert_eq!(state.plain_text(), "beforeafter");
    }

    #[test]
    fn reconstruction_restores_child_focus_mode() {
        let mut state = TerminalState::new(4, 40);
        state.process(b"\x1b[?1004h");
        assert!(state.reconstruction().ends_with(b"\x1b[?1004h"));

        state.process(b"\x1b[?1004;2004l");
        assert!(
            !state
                .reconstruction()
                .windows(8)
                .any(|bytes| bytes == b"\x1b[?1004h")
        );
    }

    #[test]
    fn reconstruction_preserves_scrollback_styles() {
        let mut source = TerminalState::new(2, 20);
        source.process(b"\x1b[31mRRRR\x1b[0m\r\nnext\r\ncurrent");

        let mut restored = TerminalState::new(2, 20);
        restored.process(&source.reconstruction());
        let mut scrolled = restored.parser.screen().clone();
        scrolled.set_scrollback(usize::MAX);
        let red_cells = (0..scrolled.size().0)
            .flat_map(|row| (0..scrolled.size().1).map(move |col| (row, col)))
            .filter_map(|(row, col)| scrolled.cell(row, col))
            .filter(|cell| cell.contents() == "R")
            .collect::<Vec<_>>();
        assert_eq!(red_cells.len(), 4);
        assert!(
            red_cells
                .iter()
                .all(|cell| cell.fgcolor() == vt100::Color::Idx(1))
        );
    }

    #[test]
    fn alternate_screen_is_reconstructed() {
        let mut source = TerminalState::new(4, 20);
        source.process(b"primary\x1b[?1049halt");
        let reconstruction = source.reconstruction();
        let mut restored = TerminalState::new(4, 20);
        restored.process(&reconstruction);

        assert!(restored.parser.screen().alternate_screen());
        assert_eq!(restored.plain_text(), "alt");
        restored.process(b"\x1b[?1049l");
        assert_eq!(restored.plain_text(), "primary");
    }

    #[test]
    fn primary_screen_keeps_updating_after_alternate_screen_exits() {
        let mut source = TerminalState::new(4, 20);
        source.process(b"before\x1b[?1049halt\x1b[?1049l after");
        let reconstruction = source.reconstruction();
        let mut restored = TerminalState::new(4, 20);
        restored.process(&reconstruction);

        assert!(!restored.parser.screen().alternate_screen());
        assert_eq!(restored.plain_text(), "before after");
    }

    #[test]
    fn resize_changes_shadow_dimensions() {
        let mut state = TerminalState::new(4, 20);
        state.resize(10, 30);

        assert_eq!(state.parser.screen().size(), (10, 30));
    }

    #[test]
    fn resize_reflows_retained_output_without_truncating_it() {
        let mut state = TerminalState::new(3, 40);
        state.process(
            b"one: a description that reaches the edge\r\n\
              two: another description that reaches the edge\r\n\
              three: final description",
        );

        state.resize(8, 20);

        let rendered = state.plain_text();
        assert!(
            rendered.contains("one: a description that reaches the edge"),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("two: another description that reaches the edge"),
            "{rendered:?}"
        );
        assert!(
            rendered.contains("three: final description"),
            "{rendered:?}"
        );
    }

    #[test]
    fn resize_preserves_rendered_styles_and_reflows_the_cursor() {
        let mut state = TerminalState::new(3, 20);
        state.process(b"prompt> \x1b[31mRRRRRR\x1b[0m");

        state.resize(5, 10);

        let screen = state.parser.screen();
        let red_cells = (0..screen.size().0)
            .flat_map(|row| (0..screen.size().1).map(move |col| (row, col)))
            .filter_map(|(row, col)| screen.cell(row, col))
            .filter(|cell| cell.contents() == "R")
            .collect::<Vec<_>>();
        assert_eq!(red_cells.len(), 6);
        assert!(
            red_cells
                .iter()
                .all(|cell| cell.fgcolor() == vt100::Color::Idx(1))
        );
        assert_eq!(screen.cursor_position(), (1, 4));
    }

    #[test]
    fn resize_preserves_active_attributes_and_cursor_visibility() {
        let mut state = TerminalState::new(3, 20);
        state.process(b"text\x1b[32m\x1b[?25l");

        state.resize(5, 10);

        let screen = state.parser.screen();
        assert_eq!(screen.fgcolor(), vt100::Color::Idx(2));
        assert!(screen.hide_cursor());
    }

    #[test]
    fn plain_text_includes_bounded_scrollback() {
        let mut state = TerminalState::new(2, 20);
        state.process(b"one\r\ntwo\r\nthree");

        assert_eq!(state.plain_text(), "one\ntwo\nthree");
        let mut restored = TerminalState::new(2, 20);
        restored.process(&state.reconstruction());
        assert_eq!(restored.plain_text(), "one\ntwo\nthree");
    }
}
