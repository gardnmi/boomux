//! Platform-independent, bounded text state. All external offsets are UTF-16;
//! internal ranges always lie on UTF-8 scalar boundaries. No preedit is sent to a
//! terminal or written to an editable field until an explicit commit.
use std::ops::Range;

pub(crate) const MAX_COMMIT_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub bytes: usize,
    pub chars: usize,
    pub multiline: bool,
}

impl Limits {
    pub const TERMINAL: Self = Self {
        bytes: MAX_COMMIT_BYTES,
        chars: MAX_COMMIT_BYTES,
        multiline: false,
    };
    pub const SEARCH: Self = Self {
        bytes: 256,
        chars: 256,
        multiline: false,
    };
    pub const NAME: Self = Self {
        bytes: 512,
        chars: 128,
        multiline: false,
    };
    pub const SETTING: Self = Self {
        bytes: MAX_COMMIT_BYTES,
        chars: MAX_COMMIT_BYTES,
        multiline: true,
    };
}

#[derive(Default, Debug)]
pub(crate) struct TextBuffer {
    pub text: String,
    pub selection: Range<usize>,
    pub marked: Option<Range<usize>>,
    pub reversed: bool,
    committed: String,
}

impl TextBuffer {
    pub fn reset(&mut self, text: &str) {
        self.text = text.into();
        self.committed = text.into();
        self.selection = text.len()..text.len();
        self.marked = None;
        self.reversed = false;
    }

    pub fn matches_committed(&self, text: &str) -> bool {
        self.committed == text
    }

    pub fn cancel(&mut self) {
        self.text.clone_from(&self.committed);
        self.selection = self.text.len()..self.text.len();
        self.marked = None;
        self.reversed = false;
    }

    pub fn select_utf16(&mut self, range: Range<usize>) {
        self.reversed = range.start > range.end;
        self.selection = from_utf16(&self.text, range);
    }

    pub fn replace(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
        marking: bool,
        limits: Limits,
    ) {
        let range = range
            .map(|range| from_utf16(&self.text, range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let remaining_bytes = self.text.len() - range.len();
        let remaining_chars =
            self.text[..range.start].chars().count() + self.text[range.end..].chars().count();
        let mut inserted = String::new();
        for (inserted_chars, ch) in text
            .chars()
            .filter(|ch| {
                (!ch.is_control() || (limits.multiline && *ch == '\n'))
                    && !('\u{f700}'..='\u{f8ff}').contains(ch)
            })
            .enumerate()
        {
            if remaining_bytes + inserted.len() + ch.len_utf8() > limits.bytes
                || remaining_chars + inserted_chars >= limits.chars
            {
                break;
            }
            inserted.push(ch);
        }
        self.text.replace_range(range.clone(), &inserted);
        let end = range.start + inserted.len();
        self.selection = if marking {
            let relative = selected
                .map(|r| from_utf16(&inserted, r))
                .unwrap_or(inserted.len()..inserted.len());
            range.start + relative.start..range.start + relative.end
        } else {
            end..end
        };
        self.reversed = false;
        self.marked = (marking && !inserted.is_empty()).then_some(range.start..end);
        if !marking {
            self.committed.clone_from(&self.text);
        }
    }

    pub fn delete(&mut self, backwards: bool, limits: Limits) {
        if self.selection.is_empty() {
            let cursor = self.selection.start;
            if backwards {
                self.selection.start = self.text[..cursor]
                    .char_indices()
                    .next_back()
                    .map_or(0, |(i, _)| i);
            } else {
                self.selection.end = self.text[cursor..]
                    .chars()
                    .next()
                    .map_or(cursor, |ch| cursor + ch.len_utf8());
            }
        }
        self.replace(None, "", None, false, limits);
    }

    pub fn move_cursor(&mut self, key: &str, extend: bool) {
        let cursor = if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        };
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        let next = match key {
            "home" => 0,
            "end" => self.text.len(),
            "left" if !extend && !self.selection.is_empty() => self.selection.start,
            "right" if !extend && !self.selection.is_empty() => self.selection.end,
            "left" => self.text[..cursor]
                .char_indices()
                .next_back()
                .map_or(0, |(i, _)| i),
            "right" => self.text[cursor..]
                .chars()
                .next()
                .map_or(cursor, |ch| cursor + ch.len_utf8()),
            _ => cursor,
        };
        self.selection = if extend {
            anchor.min(next)..anchor.max(next)
        } else {
            next..next
        };
        self.reversed = extend && next < anchor;
    }
}

/// Widen nonempty ranges around a surrogate pair; a caret inside a pair snaps
/// before it. Reversed and out-of-range native ranges are harmless.
pub(crate) fn from_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    let start = range.start.min(range.end);
    let end = range.start.max(range.end);
    let mut units = 0;
    let mut byte_start = text.len();
    let mut byte_end = text.len();
    for (byte, ch) in text.char_indices() {
        let next = units + ch.len_utf16();
        if (units..next).contains(&start) {
            byte_start = byte;
        }
        if (units..next).contains(&end) {
            byte_end = if end == units || start == end {
                byte
            } else {
                byte + ch.len_utf8()
            };
        }
        units = next;
    }
    byte_start..byte_end
}

pub(crate) fn to_utf16(text: &str, range: Range<usize>) -> Range<usize> {
    text[..range.start].encode_utf16().count()..text[..range.end].encode_utf16().count()
}

/// A handler painted for one recipient cannot write into a later recipient.
/// A cancelled preedit also rejects an unsolicited late commit until a fresh
/// key or a new marked sequence starts.
pub(crate) struct Session<T> {
    pub owner: Option<T>,
    pub generation: u64,
    pub buffer: TextBuffer,
    pub reject_commit: bool,
}
impl<T> Default for Session<T> {
    fn default() -> Self {
        Self {
            owner: None,
            generation: 0,
            buffer: TextBuffer::default(),
            reject_commit: false,
        }
    }
}
impl<T: Eq> Session<T> {
    pub fn sync(&mut self, owner: Option<T>, committed: &str) {
        if self.owner != owner || !self.buffer.matches_committed(committed) {
            self.reject_commit |= self.buffer.marked.is_some();
            self.owner = owner;
            self.generation = self.generation.wrapping_add(1);
            self.buffer.reset(committed);
        }
    }
    pub fn cancel(&mut self) {
        self.reject_commit |= self.buffer.marked.is_some();
        self.buffer.cancel();
        self.generation = self.generation.wrapping_add(1);
    }
    pub fn accepts(&self, owner: &T, generation: u64) -> bool {
        self.owner.as_ref() == Some(owner) && self.generation == generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyRoute {
    Raw,
    Native,
}

/// Printable keys go through AppKit so unmodified dead keys on international
/// layouts can compose too. Ordinary unchanged commits can retain the original
/// physical key separately; terminal control and explicit Alt shortcuts stay raw.
pub(crate) fn key_route(
    terminal: bool,
    option_as_alt: bool,
    control: bool,
    command: bool,
    function: bool,
    alt: bool,
    printable: bool,
) -> KeyRoute {
    if !control && !command && !function && printable && (!terminal || !alt || !option_as_alt) {
        KeyRoute::Native
    } else {
        KeyRoute::Raw
    }
}

/// Only an unchanged, unmarked native commit belongs to the pending physical
/// key. Dead keys, IMEs and replacement edits are pure text, never phantom keys.
pub(crate) fn preserves_physical_key(
    marked: bool,
    replacement: bool,
    expected: Option<&str>,
    committed: &str,
) -> bool {
    !marked && !replacement && expected == Some(committed) && !committed.is_empty()
}

/// An active non-ASCII input source still gets unmodified keys first. Explicit
/// terminal Option-as-Alt shortcuts must not be consumed by that input source.
pub(crate) fn ime_first(terminal: bool, option_as_alt: bool, alt: bool) -> bool {
    !(terminal && option_as_alt && alt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_ranges_do_not_split_emoji_or_panic() {
        let text = "a😀é中";
        assert_eq!(from_utf16(text, 1..3), 1..5);
        assert_eq!(from_utf16(text, 2..2), 1..1);
        assert_eq!(from_utf16(text, 2..3), 1..5);
        assert_eq!(from_utf16(text, 9..100), text.len()..text.len());
        assert_eq!(from_utf16(text, Range { start: 4, end: 1 }), 1..7);
        assert_eq!(to_utf16(text, 1..7), 1..4);
    }

    #[test]
    fn composition_is_local_until_single_commit() {
        let mut state = TextBuffer::default();
        state.reset("prefix ");
        state.replace(None, "に", Some(1..1), true, Limits::SEARCH);
        assert!(state.matches_committed("prefix "));
        state.replace(None, "日本", Some(0..2), true, Limits::SEARCH);
        assert_eq!(state.text, "prefix 日本");
        assert_eq!(state.selection, 7..13);
        state.replace(None, "日本語", None, false, Limits::SEARCH);
        assert_eq!(state.text, "prefix 日本語");
        assert!(state.marked.is_none());
        assert!(state.matches_committed("prefix 日本語"));
    }

    #[test]
    fn cancellation_restores_selected_text_and_surrogate_selection() {
        let mut state = TextBuffer::default();
        state.reset("a😀z");
        state.select_utf16(1..3);
        state.replace(None, "仮😀", Some(1..3), true, Limits::SEARCH);
        assert_eq!(to_utf16(&state.text, state.selection.clone()), 2..4);
        state.cancel();
        assert_eq!(state.text, "a😀z");
        assert!(state.marked.is_none());
    }

    #[test]
    fn explicit_replacement_and_deletion_are_scalar_safe() {
        let mut state = TextBuffer::default();
        state.reset("a😀z");
        state.replace(Some(1..3), "é", None, false, Limits::SEARCH);
        assert_eq!(state.text, "aéz");
        state.delete(true, Limits::SEARCH);
        assert_eq!(state.text, "az");
        state.delete(false, Limits::SEARCH);
        assert_eq!(state.text, "a");
    }

    #[test]
    fn input_is_bounded_without_splitting_utf8_and_filters_controls() {
        let mut state = TextBuffer::default();
        state.replace(None, &"😀".repeat(100), None, false, Limits::SEARCH);
        assert_eq!(state.text.len(), 256);
        state.reset("");
        state.replace(None, &"é".repeat(200), None, false, Limits::NAME);
        assert_eq!(state.text.chars().count(), 128);
        state.reset("");
        state.replace(None, "a\n\r\x1b\t\u{f700}b", None, false, Limits::TERMINAL);
        assert_eq!(state.text, "ab");
        state.reset("");
        state.replace(None, "a\nb", None, false, Limits::SETTING);
        assert_eq!(state.text, "a\nb");
    }

    #[test]
    fn recipient_changes_reject_old_handlers_and_late_commits() {
        let mut session = Session::default();
        session.sync(Some(1), "");
        let first = session.generation;
        session
            .buffer
            .replace(None, "仮", None, true, Limits::TERMINAL);
        session.sync(Some(2), "");
        assert!(!session.accepts(&1, first));
        assert!(!session.accepts(&2, first));
        assert!(session.reject_commit);
        assert_eq!(session.buffer.text, "");
        let second = session.generation;
        session.cancel();
        assert!(!session.accepts(&2, second));
    }

    #[test]
    fn shift_selection_reverses_and_contracts_without_splitting_scalars() {
        let mut state = TextBuffer::default();
        state.reset("a😀z");
        state.move_cursor("left", true);
        state.move_cursor("left", true);
        assert_eq!(state.selection, 1..6);
        assert!(state.reversed);
        state.move_cursor("right", true);
        assert_eq!(state.selection, 5..6);
    }

    #[test]
    fn ordinary_commits_keep_key_identity_but_dead_keys_do_not() {
        assert!(preserves_physical_key(false, false, Some("A"), "A"));
        assert!(!preserves_physical_key(false, false, Some("e"), "é"));
        assert!(!preserves_physical_key(true, false, Some("e"), "e"));
        assert!(!preserves_physical_key(false, true, Some("a"), "a"));
        assert!(!preserves_physical_key(false, false, None, "日本"));
        assert!(!preserves_physical_key(false, false, Some(""), ""));
    }

    #[test]
    fn terminal_control_and_kitty_keys_keep_the_existing_route() {
        assert!(ime_first(true, true, false));
        assert!(!ime_first(true, true, true));
        assert!(ime_first(true, false, true));
        assert!(ime_first(false, true, true));
        assert_eq!(
            key_route(true, false, false, false, false, false, true),
            KeyRoute::Native
        );
        assert_eq!(
            key_route(true, false, true, false, false, false, true),
            KeyRoute::Raw
        );
        assert_eq!(
            key_route(true, false, false, true, false, false, true),
            KeyRoute::Raw
        );
        assert_eq!(
            key_route(true, false, false, false, true, false, true),
            KeyRoute::Raw
        );
        assert_eq!(
            key_route(true, false, false, false, false, true, true),
            KeyRoute::Native
        );
        assert_eq!(
            key_route(true, true, false, false, false, true, true),
            KeyRoute::Raw
        );
        assert_eq!(
            key_route(false, true, false, false, false, true, true),
            KeyRoute::Native
        );
        assert_eq!(
            key_route(false, false, false, false, false, false, true),
            KeyRoute::Native
        );
        assert_eq!(
            key_route(false, false, false, false, false, false, false),
            KeyRoute::Raw
        );
    }
}
