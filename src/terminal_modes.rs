const ENABLE_FOCUS_REPORTING: &[u8] = b"\x1b[?1004h";
const ENABLE_COLOR_SCHEME_REPORTING: &[u8] = b"\x1b[?2031h";

#[derive(Default)]
pub(crate) struct TerminalModes {
    focus_reporting: bool,
    color_scheme_reporting: bool,
    state: ParseState,
    parameters: Vec<u8>,
}

#[derive(Clone, Copy, Default)]
enum ParseState {
    #[default]
    Normal,
    Escape,
    Csi,
    Private,
}

impl TerminalModes {
    pub(crate) fn focus_reporting(&self) -> bool {
        self.focus_reporting
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) -> bool {
        let mut focus_disabled = false;
        for byte in bytes {
            self.state = match self.state {
                ParseState::Normal if *byte == 0x1b => ParseState::Escape,
                ParseState::Normal => ParseState::Normal,
                ParseState::Escape if *byte == b'[' => ParseState::Csi,
                ParseState::Escape if *byte == 0x1b => ParseState::Escape,
                ParseState::Escape => ParseState::Normal,
                ParseState::Csi if *byte == b'?' => {
                    self.parameters.clear();
                    ParseState::Private
                }
                ParseState::Csi if *byte == 0x1b => ParseState::Escape,
                ParseState::Csi => ParseState::Normal,
                ParseState::Private if byte.is_ascii_digit() || *byte == b';' => {
                    if self.parameters.len() < 128 {
                        self.parameters.push(*byte);
                        ParseState::Private
                    } else {
                        ParseState::Normal
                    }
                }
                ParseState::Private if matches!(*byte, b'h' | b'l') => {
                    let enabled = *byte == b'h';
                    for parameter in self.parameters.split(|parameter| *parameter == b';') {
                        match parameter {
                            b"1004" => {
                                self.focus_reporting = enabled;
                                focus_disabled |= !enabled;
                            }
                            b"2031" => self.color_scheme_reporting = enabled,
                            _ => {}
                        }
                    }
                    ParseState::Normal
                }
                ParseState::Private if *byte == 0x1b => ParseState::Escape,
                ParseState::Private => ParseState::Normal,
            };
        }
        focus_disabled
    }

    pub(crate) fn append_restore_sequence(&self, output: &mut Vec<u8>) {
        if self.focus_reporting {
            output.extend_from_slice(ENABLE_FOCUS_REPORTING);
        }
        if self.color_scheme_reporting {
            output.extend_from_slice(ENABLE_COLOR_SCHEME_REPORTING);
        }
    }

    pub(crate) fn restore_sequence_len(&self) -> usize {
        usize::from(self.focus_reporting) * ENABLE_FOCUS_REPORTING.len()
            + usize::from(self.color_scheme_reporting) * ENABLE_COLOR_SCHEME_REPORTING.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_split_and_combined_terminal_mode_changes() {
        let mut modes = TerminalModes::default();

        assert!(!modes.process(b"\x1b[?10"));
        assert!(!modes.process(b"04;2031h"));
        assert!(modes.focus_reporting());
        let mut restoration = Vec::new();
        modes.append_restore_sequence(&mut restoration);
        assert_eq!(restoration, b"\x1b[?1004h\x1b[?2031h");

        assert!(modes.process(b"\x1b[?1004;2004l"));
        assert!(!modes.focus_reporting());
        modes.process(b"\x1b[?2031l");
        restoration.clear();
        modes.append_restore_sequence(&mut restoration);
        assert!(restoration.is_empty());
    }
}
