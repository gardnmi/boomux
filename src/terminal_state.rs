const SCROLLBACK_ROWS: usize = 2_000;
const MAX_RECONSTRUCTION_BYTES: usize = 1024 * 1024;

pub(crate) struct TerminalState {
    parser: vt100::Parser,
    primary_before_alternate: Option<vt100::Screen>,
}

impl TerminalState {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        Self {
            parser,
            primary_before_alternate: None,
        }
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) {
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
        self.parser.screen_mut().set_size(rows, cols);
        if let Some(primary) = self.primary_before_alternate.as_mut() {
            primary.set_size(rows, cols);
        }
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
        if output.len() <= MAX_RECONSTRUCTION_BYTES {
            return output;
        }

        let mut fallback = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[H\x1b[2J".to_vec();
        if screen.alternate_screen() {
            fallback.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        }
        let suffix = state_suffix(screen);
        let text_limit = MAX_RECONSTRUCTION_BYTES.saturating_sub(suffix.len());
        append_terminal_text_bounded(&mut fallback, &screen.contents(), text_limit);
        fallback.extend(suffix);
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
    let scrollback = scrollback_text(screen);
    if scrollback.is_empty() {
        return;
    }
    append_terminal_text_bounded(output, &scrollback, MAX_RECONSTRUCTION_BYTES);
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
    fn plain_text_includes_bounded_scrollback() {
        let mut state = TerminalState::new(2, 20);
        state.process(b"one\r\ntwo\r\nthree");

        assert_eq!(state.plain_text(), "one\ntwo\nthree");
        let mut restored = TerminalState::new(2, 20);
        restored.process(&state.reconstruction());
        assert_eq!(restored.plain_text(), "one\ntwo\nthree");
    }
}
