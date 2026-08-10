const ENABLE_FOCUS_REPORTING: &[u8] = b"\x1b[?1004h";

#[derive(Default)]
pub(crate) struct FocusMode {
    enabled: bool,
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

impl FocusMode {
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    pub(crate) fn process(&mut self, bytes: &[u8]) -> bool {
        let mut disabled = false;
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
                    if self
                        .parameters
                        .split(|parameter| *parameter == b';')
                        .any(|parameter| parameter == b"1004")
                    {
                        self.enabled = *byte == b'h';
                        disabled |= !self.enabled;
                    }
                    ParseState::Normal
                }
                ParseState::Private if *byte == 0x1b => ParseState::Escape,
                ParseState::Private => ParseState::Normal,
            };
        }
        disabled
    }

    pub(crate) fn restore_sequence(&self) -> &'static [u8] {
        if self.enabled {
            ENABLE_FOCUS_REPORTING
        } else {
            &[]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_split_and_combined_focus_mode_changes() {
        let mut mode = FocusMode::default();

        assert!(!mode.process(b"\x1b[?10"));
        assert!(!mode.process(b"04;2004h"));
        assert!(mode.enabled());
        assert!(mode.process(b"\x1b[?1004;2004l"));
        assert!(!mode.enabled());
    }
}
