//! Pure ownership and size rules for macOS menu/keyboard clipboard actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditAction {
    Copy,
    Cut,
    Paste,
    SelectAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Recipient {
    Field(u64),
    Terminal {
        pane: usize,
        attachment: u64,
        focus: u64,
    },
}

pub(crate) const MAX_CLIPBOARD_BYTES: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub(crate) struct Requests {
    serial: u64,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ticket {
    serial: u64,
    recipient: Recipient,
}
impl Requests {
    pub fn start(&mut self, recipient: Recipient) -> Ticket {
        self.serial = self.serial.wrapping_add(1);
        Ticket {
            serial: self.serial,
            recipient,
        }
    }
    pub fn accepts(&self, ticket: Ticket, recipient: Option<Recipient>) -> bool {
        self.serial == ticket.serial && recipient == Some(ticket.recipient)
    }
}

pub(crate) fn acceptable_paste(bytes: usize) -> bool {
    bytes <= MAX_CLIPBOARD_BYTES
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn copy_completion_keeps_the_original_recipient_and_attachment() {
        let mut requests = Requests::default();
        let target = Recipient::Terminal {
            pane: 7,
            attachment: 1,
            focus: 1,
        };
        let ticket = requests.start(target);
        assert!(requests.accepts(ticket, Some(target)));
        assert!(!requests.accepts(
            ticket,
            Some(Recipient::Terminal {
                pane: 8,
                attachment: 1,
                focus: 1
            })
        ));
        assert!(!requests.accepts(
            ticket,
            Some(Recipient::Terminal {
                pane: 7,
                attachment: 2,
                focus: 1
            })
        ));
        assert!(!requests.accepts(
            ticket,
            Some(Recipient::Terminal {
                pane: 7,
                attachment: 1,
                focus: 2
            })
        ));
        assert!(!requests.accepts(ticket, Some(Recipient::Field(1))));
        assert!(!requests.accepts(ticket, None));
    }
    #[test]
    fn a_new_copy_or_paste_invalidates_an_older_result() {
        let mut requests = Requests::default();
        let target = Recipient::Field(3);
        let old = requests.start(target);
        let new = requests.start(target);
        assert!(!requests.accepts(old, Some(target)));
        assert!(requests.accepts(new, Some(target)));
        assert!(!requests.accepts(new, Some(Recipient::Field(4))));
    }
    #[test]
    fn terminal_clipboard_size_has_an_explicit_boundary() {
        assert!(acceptable_paste(0));
        assert!(acceptable_paste(MAX_CLIPBOARD_BYTES));
        assert!(!acceptable_paste(MAX_CLIPBOARD_BYTES + 1));
        assert!(!acceptable_paste(usize::MAX));
    }
}
