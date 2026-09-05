use std::borrow::Cow;

use crate::protocol::{
    TerminalColor, TerminalPreview, TerminalPreviewLine, TerminalPreviewSpan, TerminalStyle,
};
use crate::terminal_modes::TerminalModes;

const SCROLLBACK_ROWS: usize = 2_000;
const MAX_RECONSTRUCTION_BYTES: usize = 1024 * 1024;

pub(crate) struct TerminalState {
    parser: vt100::Parser,
    terminal_modes: TerminalModes,
}

pub(crate) struct TerminalSnapshot {
    screen: vt100::Screen,
}

impl TerminalState {
    pub(crate) fn new(rows: u16, cols: u16) -> Self {
        let parser = vt100::Parser::new(rows, cols, SCROLLBACK_ROWS);
        Self {
            parser,
            terminal_modes: TerminalModes::default(),
        }
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) {
        self.terminal_modes.process(bytes);
        self.parser.process(bytes);
    }

    fn primary_screen(&self) -> Cow<'_, vt100::Screen> {
        let screen = self.parser.screen();
        if !screen.alternate_screen() {
            return Cow::Borrowed(screen);
        }
        // Screen already retains both grids, including the saved primary cursor.
        // Switch a temporary copy through the parser instead of recognizing raw
        // escape sequences, whose boundaries need not match PTY reads.
        let mut primary = vt100::Parser::new(1, 1, 0);
        let mut screen = screen.clone();
        std::mem::swap(primary.screen_mut(), &mut screen);
        // Mode 47 does not save the cursor. Keep the primary grid's actual
        // position even if an earlier DECSC saved a different one. Mode 1049
        // also restores drawing attributes; cursor restoration can redraw the
        // last cell of a wrapped row, so restore those attributes afterwards.
        primary.process(b"\x1b[?47l");
        let cursor = primary.screen().cursor_state_formatted();
        primary.process(b"\x1b[?1049l");
        let attributes = primary.screen().attributes_formatted();
        // cursor_state_formatted uses physical coordinates, even when the
        // primary grid had origin mode enabled within a scrolling region.
        primary.process(b"\x1b[?6l");
        primary.process(&cursor);
        primary.process(&attributes);
        std::mem::swap(primary.screen_mut(), &mut screen);
        Cow::Owned(screen)
    }

    pub(crate) fn resize(&mut self, rows: u16, cols: u16) {
        if self.parser.screen().size() == (rows, cols) {
            return;
        }

        let screen = self.parser.screen();
        let primary = self.primary_screen();
        let mut reconstruction = reflowable_contents_formatted(&primary);
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
        self.terminal_modes
            .append_restore_sequence(&mut reconstruction);

        let mut resized = Self::new(rows, cols);
        resized.process(&reconstruction);
        *self = resized;
    }

    pub(crate) fn reconstruction(&self) -> Vec<u8> {
        let screen = self.parser.screen();
        let primary = self.primary_screen();
        let mut output = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[H\x1b[2J".to_vec();
        append_scrollback(&mut output, &primary);
        output.extend_from_slice(b"\x1b[H\x1b[2J");
        output.extend(primary.contents_formatted());
        if screen.alternate_screen() {
            output.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
            output.extend(screen.contents_formatted());
        }
        output.extend(screen.input_mode_formatted());
        self.terminal_modes.append_restore_sequence(&mut output);
        if output.len() <= MAX_RECONSTRUCTION_BYTES {
            return output;
        }

        let mut fallback = b"\x1b[?1049l\x1b[0m\x1b[?25h\x1b[H\x1b[2J".to_vec();
        if screen.alternate_screen() {
            fallback.extend_from_slice(b"\x1b[?1049h\x1b[H\x1b[2J");
        }
        let suffix = state_suffix(screen);
        let terminal_modes_len = self.terminal_modes.restore_sequence_len();
        let text_limit = MAX_RECONSTRUCTION_BYTES
            .saturating_sub(suffix.len())
            .saturating_sub(terminal_modes_len);
        append_terminal_text_bounded(&mut fallback, &screen.contents(), text_limit);
        fallback.extend(suffix);
        self.terminal_modes.append_restore_sequence(&mut fallback);
        fallback
    }

    #[cfg(test)]
    pub(crate) fn plain_text(&self) -> String {
        let screen = self.parser.screen();
        let mut text = scrollback_text(screen);
        text.push_str(&screen.contents());
        text.chars()
            .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
            .collect()
    }

    pub(crate) fn snapshot(&self) -> TerminalSnapshot {
        TerminalSnapshot {
            screen: self.parser.screen().clone(),
        }
    }

    #[cfg(test)]
    pub(crate) fn cold_history(&self, max_bytes: usize) -> String {
        self.snapshot().plain_text_suffix(max_bytes)
    }

    #[cfg(test)]
    pub(crate) fn preview(
        &self,
        max_bytes: usize,
        max_lines: usize,
        max_spans: usize,
    ) -> TerminalPreview {
        self.snapshot().preview(max_bytes, max_lines, max_spans)
    }
}

impl TerminalSnapshot {
    pub(crate) fn plain_text_suffix(mut self, max_bytes: usize) -> String {
        if max_bytes == 0 {
            return String::new();
        }

        self.screen.set_scrollback(usize::MAX);
        let scrollback_rows = self.screen.scrollback();
        self.screen.set_scrollback(0);
        let (rows, cols) = self.screen.size();
        let mut pieces = Vec::new();
        let mut bytes = 0usize;
        let mut trimming_end = true;

        for row in (0..rows).rev() {
            let wrapping = if row > 0 {
                self.screen.row_wrapped(row - 1)
            } else if scrollback_rows > 0 {
                self.screen.set_scrollback(1);
                let wrapped = self.screen.row_wrapped(0);
                self.screen.set_scrollback(0);
                wrapped
            } else {
                false
            };
            let mut piece = plain_row(&self.screen, row, cols, wrapping);
            if !self.screen.row_wrapped(row) {
                piece.push('\n');
            }
            if trimming_end {
                piece.truncate(piece.trim_end_matches('\n').len());
                trimming_end = piece.is_empty();
            }
            bytes = bytes.saturating_add(piece.len());
            pieces.push(piece);
            if bytes >= max_bytes {
                return join_utf8_suffix(pieces, max_bytes);
            }
        }

        for offset in 1..=scrollback_rows {
            self.screen.set_scrollback(offset);
            let wrapping = if offset < scrollback_rows {
                self.screen.set_scrollback(offset + 1);
                let wrapped = self.screen.row_wrapped(0);
                self.screen.set_scrollback(offset);
                wrapped
            } else {
                false
            };
            let mut piece = plain_row(&self.screen, 0, cols, wrapping);
            if !self.screen.row_wrapped(0) {
                piece.push('\n');
            }
            bytes = bytes.saturating_add(piece.len());
            pieces.push(piece);
            if bytes >= max_bytes {
                break;
            }
        }

        join_utf8_suffix(pieces, max_bytes)
    }

    pub(crate) fn preview(
        mut self,
        max_bytes: usize,
        max_lines: usize,
        max_spans: usize,
    ) -> TerminalPreview {
        if max_lines == 0 {
            return TerminalPreview::default();
        }

        self.screen.set_scrollback(usize::MAX);
        let scrollback_rows = self.screen.scrollback();
        self.screen.set_scrollback(0);
        let (rows, _) = self.screen.size();
        let mut selected = Vec::new();
        let mut pending_blanks = Vec::new();
        let mut current = TerminalPreviewLine::default();
        let mut bytes = 0usize;
        let mut spans = 0usize;
        let mut saw_content = false;

        for row in (0..rows).rev() {
            prepend_preview_row(&mut current, &self.screen, row);
            let previous_wrapped = if row > 0 {
                self.screen.row_wrapped(row - 1)
            } else if scrollback_rows > 0 {
                self.screen.set_scrollback(1);
                let wrapped = self.screen.row_wrapped(0);
                self.screen.set_scrollback(0);
                wrapped
            } else {
                false
            };
            if !previous_wrapped
                && select_preview_line(
                    &mut selected,
                    &mut pending_blanks,
                    &mut current,
                    &mut bytes,
                    &mut spans,
                    &mut saw_content,
                    max_bytes,
                    max_lines,
                    max_spans,
                )
            {
                selected.reverse();
                return TerminalPreview { lines: selected };
            }
        }

        for offset in 1..=scrollback_rows {
            self.screen.set_scrollback(offset);
            prepend_preview_row(&mut current, &self.screen, 0);
            let previous_wrapped = if offset < scrollback_rows {
                self.screen.set_scrollback(offset + 1);
                let wrapped = self.screen.row_wrapped(0);
                self.screen.set_scrollback(offset);
                wrapped
            } else {
                false
            };
            if !previous_wrapped
                && select_preview_line(
                    &mut selected,
                    &mut pending_blanks,
                    &mut current,
                    &mut bytes,
                    &mut spans,
                    &mut saw_content,
                    max_bytes,
                    max_lines,
                    max_spans,
                )
            {
                break;
            }
        }

        selected.reverse();
        TerminalPreview { lines: selected }
    }
}

#[cfg(any(test, feature = "benchmark-internals"))]
pub mod benchmark_support {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct TerminalSummary {
        pub preview_lines: usize,
        pub preview_spans: usize,
        pub preview_bytes: usize,
        pub preview_checksum: u64,
        pub reconstruction_bytes: usize,
        pub reconstruction_checksum: u64,
    }

    pub struct TerminalFixture {
        state: TerminalState,
    }

    impl TerminalFixture {
        pub fn empty(rows: u16, cols: u16) -> Self {
            Self {
                state: TerminalState::new(rows, cols),
            }
        }

        pub fn from_transcript(rows: u16, cols: u16, transcript: &[u8]) -> Self {
            let mut fixture = Self::empty(rows, cols);
            fixture.process(transcript);
            fixture
        }

        pub fn from_chunked_transcript(
            rows: u16,
            cols: u16,
            transcript: &[u8],
            chunk_bytes: usize,
        ) -> Self {
            assert!(chunk_bytes > 0);
            let mut fixture = Self::empty(rows, cols);
            for chunk in transcript.chunks(chunk_bytes) {
                fixture.process(chunk);
            }
            fixture
        }

        pub fn process(&mut self, bytes: &[u8]) {
            self.state.process(bytes);
        }

        pub fn preview(
            &self,
            max_bytes: usize,
            max_lines: usize,
            max_spans: usize,
        ) -> TerminalPreview {
            self.state
                .snapshot()
                .preview(max_bytes, max_lines, max_spans)
        }

        pub fn reconstruction(&self) -> Vec<u8> {
            self.state.reconstruction()
        }

        pub fn summary(&self) -> TerminalSummary {
            let preview = self.preview(1024 * 1024, 16, 20_000);
            let reconstruction = self.reconstruction();
            TerminalSummary {
                preview_lines: preview.lines.len(),
                preview_spans: preview.lines.iter().map(|line| line.spans.len()).sum(),
                preview_bytes: preview
                    .lines
                    .iter()
                    .flat_map(|line| &line.spans)
                    .map(|span| span.text.len())
                    .sum(),
                preview_checksum: checksum_preview(&preview),
                reconstruction_bytes: reconstruction.len(),
                reconstruction_checksum: checksum_bytes(&reconstruction),
            }
        }
    }

    pub fn terminal_transcript(lines: usize, line_bytes: usize) -> Vec<u8> {
        let mut transcript =
            Vec::with_capacity(lines.saturating_mul(line_bytes.saturating_add(24)));
        for line in 0..lines {
            transcript.extend_from_slice(format!("\x1b[3{}mline-{line:05} ", line % 8).as_bytes());
            let payload = line_bytes.saturating_sub(12);
            transcript.extend(std::iter::repeat_n(b'a' + (line % 26) as u8, payload));
            transcript.extend_from_slice(b"\x1b[0m\r\n");
        }
        transcript
    }

    fn checksum_bytes(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0, |checksum, byte| {
            checksum_value(checksum, u64::from(*byte))
        })
    }

    fn checksum_preview(preview: &TerminalPreview) -> u64 {
        let mut checksum = checksum_value(0, preview.lines.len() as u64);
        for line in &preview.lines {
            checksum = checksum_value(checksum, line.spans.len() as u64);
            for span in &line.spans {
                checksum = checksum_value(checksum, span.text.len() as u64);
                for byte in span.text.bytes() {
                    checksum = checksum_value(checksum, u64::from(byte));
                }
                checksum = checksum_color(checksum, span.style.foreground);
                checksum = checksum_color(checksum, span.style.background);
                for value in [
                    span.style.bold,
                    span.style.dim,
                    span.style.italic,
                    span.style.underline,
                    span.style.inverse,
                ] {
                    checksum = checksum_value(checksum, u64::from(value));
                }
            }
        }
        checksum
    }

    fn checksum_color(checksum: u64, color: TerminalColor) -> u64 {
        match color {
            TerminalColor::Default => checksum_value(checksum, 0),
            TerminalColor::Indexed(index) => {
                checksum_value(checksum_value(checksum, 1), u64::from(index))
            }
            TerminalColor::Rgb { red, green, blue } => {
                let checksum = checksum_value(checksum, 2);
                let checksum = checksum_value(checksum, u64::from(red));
                let checksum = checksum_value(checksum, u64::from(green));
                checksum_value(checksum, u64::from(blue))
            }
        }
    }

    fn checksum_value(checksum: u64, value: u64) -> u64 {
        checksum.wrapping_mul(0x100_0000_01b3).wrapping_add(value)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn benchmark_terminal_fixture_has_a_stable_bounded_summary() {
            let transcript = terminal_transcript(2_048, 128);
            let terminal = TerminalFixture::from_transcript(24, 80, &transcript);
            let summary = terminal.summary();
            assert_eq!(summary.preview_lines, 16);
            assert!(summary.preview_spans >= summary.preview_lines);
            assert!(summary.preview_bytes > 0);
            assert_ne!(summary.preview_checksum, 0);
            assert_eq!(summary.preview_checksum, 10_375_648_925_095_483_392);
            assert!(summary.reconstruction_bytes <= 1024 * 1024);
            assert_ne!(summary.reconstruction_checksum, 0);
            assert_eq!(summary.reconstruction_checksum, 12_069_107_278_816_405_535);
            assert_eq!(terminal.summary(), summary);

            let chunked = TerminalFixture::from_chunked_transcript(24, 80, &transcript, 16 * 1024);
            assert_eq!(chunked.summary(), summary);
        }
    }
}

fn plain_row(screen: &vt100::Screen, row: u16, cols: u16, wrapping: bool) -> String {
    let mut text = String::new();
    let mut next_col = 0;
    for col in 0..cols {
        let Some(cell) = screen.cell(row, col) else {
            continue;
        };
        if cell.is_wide_continuation() || !cell.has_contents() {
            continue;
        }
        for _ in next_col..col {
            text.push(' ');
        }
        text.extend(
            cell.contents()
                .chars()
                .filter(|character| !character.is_control() || matches!(character, '\n' | '\t')),
        );
        next_col = col.saturating_add(if cell.is_wide() { 2 } else { 1 });
    }
    if next_col == 0 && wrapping {
        text.push('\n');
    }
    text
}

fn join_utf8_suffix(mut pieces: Vec<String>, max_bytes: usize) -> String {
    pieces.reverse();
    let text = pieces.concat();
    let mut start = text.len().saturating_sub(max_bytes);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

#[allow(clippy::too_many_arguments)]
fn select_preview_line(
    selected: &mut Vec<TerminalPreviewLine>,
    pending_blanks: &mut Vec<TerminalPreviewLine>,
    current: &mut TerminalPreviewLine,
    bytes: &mut usize,
    spans: &mut usize,
    saw_content: &mut bool,
    max_bytes: usize,
    max_lines: usize,
    max_spans: usize,
) -> bool {
    let line = std::mem::take(current);
    if !*saw_content && preview_line_is_blank(&line) {
        return false;
    }
    if preview_line_is_blank(&line) {
        pending_blanks.push(line);
        return false;
    }
    *saw_content = true;
    for blank in pending_blanks.drain(..) {
        if push_preview_line(
            selected, blank, bytes, spans, max_bytes, max_lines, max_spans,
        ) {
            return true;
        }
    }
    push_preview_line(
        selected, line, bytes, spans, max_bytes, max_lines, max_spans,
    )
}

fn push_preview_line(
    selected: &mut Vec<TerminalPreviewLine>,
    line: TerminalPreviewLine,
    bytes: &mut usize,
    spans: &mut usize,
    max_bytes: usize,
    max_lines: usize,
    max_spans: usize,
) -> bool {
    let line_bytes = line.spans.iter().map(|span| span.text.len()).sum::<usize>();
    let line_spans = line.spans.len();
    if !selected.is_empty()
        && (bytes.saturating_add(line_bytes) > max_bytes
            || spans.saturating_add(line_spans) > max_spans)
    {
        return true;
    }
    *bytes = bytes.saturating_add(line_bytes);
    *spans = spans.saturating_add(line_spans);
    selected.push(line);
    selected.len() >= max_lines
}

fn prepend_preview_row(current: &mut TerminalPreviewLine, screen: &vt100::Screen, row: u16) {
    let mut prefix = TerminalPreviewLine::default();
    append_preview_row_contents(&mut prefix, screen, row);
    for span in std::mem::take(&mut current.spans) {
        append_preview_span(&mut prefix, &span.text, span.style);
    }
    *current = prefix;
}

fn append_preview_row_contents(
    current: &mut TerminalPreviewLine,
    screen: &vt100::Screen,
    row: u16,
) {
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

    if let Some(last_col) = last_col {
        for col in 0..=last_col {
            let Some(cell) = screen.cell(row, col) else {
                continue;
            };
            if cell.is_wide_continuation() {
                continue;
            }
            let text = if cell.has_contents() {
                cell.contents()
            } else {
                " "
            };
            append_preview_span(current, text, CellStyle::from_cell(cell).into());
        }
    }
}

fn append_preview_span(line: &mut TerminalPreviewLine, text: &str, style: TerminalStyle) {
    if let Some(span) = line.spans.last_mut().filter(|span| span.style == style) {
        span.text.push_str(text);
    } else {
        line.spans.push(TerminalPreviewSpan {
            text: text.to_owned(),
            style,
        });
    }
}

fn preview_line_is_blank(line: &TerminalPreviewLine) -> bool {
    line.spans.iter().all(|span| span.text.trim().is_empty())
}

#[cfg(test)]
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

impl From<CellStyle> for TerminalStyle {
    fn from(style: CellStyle) -> Self {
        Self {
            foreground: style.foreground.into(),
            background: style.background.into(),
            bold: style.bold,
            dim: style.dim,
            italic: style.italic,
            underline: style.underline,
            inverse: style.inverse,
        }
    }
}

impl From<vt100::Color> for TerminalColor {
    fn from(color: vt100::Color) -> Self {
        match color {
            vt100::Color::Default => Self::Default,
            vt100::Color::Idx(index) => Self::Indexed(index),
            vt100::Color::Rgb(red, green, blue) => Self::Rgb { red, green, blue },
        }
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
    fn reconstruction_restores_child_color_scheme_reporting_mode() {
        let mut state = TerminalState::new(4, 40);
        state.process(b"\x1b[?2031h");
        assert!(state.reconstruction().ends_with(b"\x1b[?2031h"));

        state.process(b"\x1b[?1004;2031l");
        assert!(
            !state
                .reconstruction()
                .windows(8)
                .any(|bytes| bytes == b"\x1b[?2031h")
        );
    }

    #[test]
    fn bounded_reconstruction_retains_terminal_reporting_modes() {
        let mut state = TerminalState::new(4, 600);
        let line = vec![b'x'; 600];
        for _ in 0..SCROLLBACK_ROWS + 10 {
            state.process(&line);
            state.process(b"\r\n");
        }
        state.process(b"\x1b[?1004;2031h");

        let reconstruction = state.reconstruction();

        assert!(reconstruction.len() <= MAX_RECONSTRUCTION_BYTES);
        assert!(reconstruction.ends_with(b"\x1b[?1004h\x1b[?2031h"));
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
    fn alternate_screen_reconstruction_is_independent_of_chunk_boundaries() {
        for sequence in [b"\x1b[?1049h".as_slice(), b"\x1b[?25;1049h", b"\x1b[?47h"] {
            for split in 0..=sequence.len() {
                for resize in [false, true] {
                    let mut source = TerminalState::new(4, 20);
                    source.process(b"primary");
                    source.process(&sequence[..split]);
                    source.process(&sequence[split..]);
                    source.process(b"alt");
                    if resize {
                        source.resize(5, 25);
                    }
                    let mut restored = TerminalState::new(5, 25);
                    restored.process(&source.reconstruction());
                    assert_eq!(restored.plain_text(), "alt");
                    restored.process(b"\x1b[?1049l");
                    assert_eq!(
                        restored.plain_text(),
                        "primary",
                        "sequence={sequence:?}, split={split}, resize={resize}"
                    );
                }
            }
        }
    }

    #[test]
    fn alternate_reconstruction_preserves_primary_cursor_and_saved_attributes() {
        let mut source = TerminalState::new(4, 30);
        source.process(b"\x1b7primary\x1b[?47halt");
        let mut restored = TerminalState::new(4, 30);
        restored.process(&source.reconstruction());
        restored.process(b"\x1b[?47l!");
        assert_eq!(restored.plain_text(), "primary!");

        let mut source = TerminalState::new(4, 30);
        source.process(b"\x1b[31mprimary\x1b[?1049h\x1b[32malt");
        let mut restored = TerminalState::new(4, 30);
        restored.process(&source.reconstruction());
        restored.process(b"\x1b[?1049l!");
        assert_eq!(restored.plain_text(), "primary!");
        assert_eq!(
            restored.parser.screen().cell(0, 7).unwrap().fgcolor(),
            vt100::Color::Idx(1)
        );
    }

    #[test]
    fn alternate_reconstruction_preserves_cursor_with_primary_origin_mode() {
        let mut source = TerminalState::new(6, 20);
        source.process(b"\x1b[2;5r\x1b[?6hprimary\x1b[?1049halt");
        let mut restored = TerminalState::new(6, 20);
        restored.process(&source.reconstruction());
        restored.process(b"\x1b[?1049l!");
        source.process(b"\x1b[?1049l!");
        assert_eq!(restored.plain_text(), source.plain_text());
    }

    #[test]
    fn repeated_alternate_transitions_capture_the_latest_primary() {
        let mut source = TerminalState::new(4, 30);
        source.process(b"first\x1b[?1049hone\x1b[?1049l second\x1b[?1049htwo");
        let mut restored = TerminalState::new(4, 30);
        restored.process(&source.reconstruction());
        assert_eq!(restored.plain_text(), "two");
        restored.process(b"\x1b[?1049l third");
        assert_eq!(restored.plain_text(), "first second third");
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
    fn resize_preserves_color_scheme_reporting_mode() {
        let mut state = TerminalState::new(4, 20);
        state.process(b"\x1b[?2031h");
        state.resize(10, 30);

        assert!(state.reconstruction().ends_with(b"\x1b[?2031h"));
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

    #[test]
    fn cold_history_keeps_a_utf8_safe_bounded_suffix() {
        let mut state = TerminalState::new(2, 20);
        state.process("prefix-abcdé".as_bytes());

        let history = state.cold_history(3);

        assert_eq!(history, "dé");
        assert!(history.len() <= 3);
    }

    #[test]
    fn bounded_plain_text_matches_full_rendering_suffixes() {
        let mut state = TerminalState::new(3, 8);
        state.process("one\r\nwide界\r\n\r\nfour\r\nfive é".as_bytes());
        let full = state.plain_text();

        for max_bytes in 0..=full.len() + 2 {
            let mut start = full.len().saturating_sub(max_bytes);
            while !full.is_char_boundary(start) {
                start += 1;
            }
            assert_eq!(state.snapshot().plain_text_suffix(max_bytes), full[start..]);
        }
    }

    #[test]
    fn bounded_plain_text_matches_wrapping_across_scrollback_boundary() {
        let mut state = TerminalState::new(2, 4);
        state.process(b"abcd\r\nnext");
        let full = state.plain_text();

        for max_bytes in 0..=full.len() {
            let mut start = full.len().saturating_sub(max_bytes);
            while !full.is_char_boundary(start) {
                start += 1;
            }
            assert_eq!(state.snapshot().plain_text_suffix(max_bytes), full[start..]);
        }
    }

    #[test]
    fn preview_preserves_styles_and_matches_plain_text() {
        let mut state = TerminalState::new(3, 30);
        state.process(b"plain \x1b[1;31mred\x1b[0m\r\n\x1b[38;2;1;2;3;48;2;4;5;6mcolor\x1b[0m");

        let preview = state.preview(1024, 100, 100);
        let text = preview
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(text, state.plain_text());
        assert_eq!(preview.lines[0].spans.len(), 2);
        assert_eq!(
            preview.lines[0].spans[1].style.foreground,
            TerminalColor::Indexed(1)
        );
        assert!(preview.lines[0].spans[1].style.bold);
        assert_eq!(
            preview.lines[1].spans[0].style.foreground,
            TerminalColor::Rgb {
                red: 1,
                green: 2,
                blue: 3
            }
        );
        assert_eq!(
            preview.lines[1].spans[0].style.background,
            TerminalColor::Rgb {
                red: 4,
                green: 5,
                blue: 6
            }
        );
    }

    #[test]
    fn preview_keeps_the_newest_complete_lines_within_bounds() {
        let mut state = TerminalState::new(2, 20);
        state.process(b"one\r\ntwo\r\nthree");

        let preview = state.preview(1024, 2, 100);
        let text = preview
            .lines
            .iter()
            .map(|line| line.spans[0].text.as_str())
            .collect::<Vec<_>>();

        assert_eq!(text, ["two", "three"]);
    }

    #[test]
    fn preview_preserves_interior_blanks_before_applying_line_limit() {
        let mut state = TerminalState::new(4, 20);
        state.process(b"one\r\n\r\nthree");

        let preview = state.preview(1024, 2, 100);

        assert_eq!(preview.lines.len(), 2);
        assert!(preview_line_is_blank(&preview.lines[0]));
        assert_eq!(preview.lines[1].spans[0].text, "three");
    }

    #[test]
    fn bounded_preview_stops_at_newest_styled_scrollback_lines() {
        let mut state = TerminalState::new(2, 20);
        for line in 0..SCROLLBACK_ROWS + 20 {
            state.process(format!("\x1b[3{}mline-{line:04}\x1b[0m\r\n", line % 8).as_bytes());
        }

        let preview = state.snapshot().preview(32, 2, 4);
        let text = preview
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.text.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();

        assert_eq!(text, ["line-2018", "line-2019"]);
        assert_eq!(
            preview
                .lines
                .iter()
                .map(|line| line.spans.len())
                .sum::<usize>(),
            2
        );
        assert_ne!(
            preview.lines[0].spans[0].style.foreground,
            TerminalColor::Default
        );
    }
}
