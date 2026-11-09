//! Desktop notifications from a pane (spec D30): what libghostty reports
//! for an OSC 9 that is not a `ConEmu` command or an `OSC 777;notify`
//! (`show_desktop_notification`, `osc/parsers/osc9.zig` and
//! `rxvt_extension.zig`; the `desktop_notification` effect,
//! `stream_terminal.zig:780, 916`), and the sequence that gives it to an
//! outer Ghostty.

/// One notification a pane's program asked for. Both fields are the
/// program's bytes as libghostty collected them; the title is empty for
/// OSC 9.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notification {
    pub title: Vec<u8>,
    pub body: Vec<u8>,
}

impl Notification {
    /// The OSC that shows this notification in Ghostty, in the form the
    /// program used. libghostty reports a title and a body, not the form:
    /// OSC 9 has no title, so a title means `OSC 777;notify;title;body`.
    /// With no title, OSC 9 and an `OSC 777;notify;;body` are the same
    /// notification to Ghostty (both parsers make `show_desktop_notification`
    /// with an empty title and that body), so it goes as OSC 9, unless
    /// libghostty would read `OSC 9;body` as something else (a `ConEmu`
    /// command such as `9;4`, which only a 777 could have carried).
    /// Ended by ST. Bytes below 0x20, which no OSC libghostty collected
    /// can hold, are left out, so nothing the outer terminal reads as a
    /// control escapes.
    #[must_use]
    pub fn osc(&self) -> Vec<u8> {
        let clean = |s: &[u8]| -> Vec<u8> { s.iter().copied().filter(|&b| b >= 0x20).collect() };
        let (title, body) = (clean(&self.title), clean(&self.body));
        let mut out = b"\x1b]".to_vec();
        if title.is_empty() && reads_as_notification_osc_9(&body) {
            out.extend_from_slice(b"9;");
        } else {
            out.extend_from_slice(b"777;notify;");
            out.extend_from_slice(&title);
            out.push(b';');
        }
        out.extend_from_slice(&body);
        out.extend_from_slice(b"\x1b\\");
        out
    }
}

/// Whether libghostty's OSC parser takes `OSC 9;body` for a notification.
fn reads_as_notification_osc_9(body: &[u8]) -> bool {
    use libghostty_vt::osc::{CommandType, Parser};
    let Ok(mut parser) = Parser::new() else {
        return false;
    };
    for &b in b"9;".iter().chain(body) {
        parser.next_byte(b);
    }
    matches!(
        parser.end(0x1b).command_type(),
        CommandType::ShowDesktopNotification
    )
}

#[cfg(test)]
mod tests {
    use super::Notification;

    fn osc(title: &[u8], body: &[u8]) -> Vec<u8> {
        Notification {
            title: title.to_vec(),
            body: body.to_vec(),
        }
        .osc()
    }

    #[test]
    fn a_notification_goes_in_its_own_form() {
        assert_eq!(osc(b"", b"done"), b"\x1b]9;done\x1b\\");
        assert_eq!(
            osc(b"Build", b"ok; 3 tests"),
            b"\x1b]777;notify;Build;ok; 3 tests\x1b\\"
        );
        assert_eq!(
            osc("é".as_bytes(), b""),
            "\x1b]777;notify;é;\x1b\\".as_bytes()
        );
    }

    /// An empty-title 777 whose body OSC 9 would take for a `ConEmu`
    /// command (progress, sleep, a tab title) stays a 777.
    #[test]
    fn a_body_osc_9_reads_otherwise_stays_777() {
        assert_eq!(osc(b"", b"4;1;50"), b"\x1b]777;notify;;4;1;50\x1b\\");
        assert_eq!(osc(b"", b"1;100"), b"\x1b]777;notify;;1;100\x1b\\");
        assert_eq!(osc(b"", b"1 file left"), b"\x1b]9;1 file left\x1b\\");
    }

    #[test]
    fn control_bytes_are_left_out() {
        assert_eq!(osc(b"", b"a\x07b\x1bc"), b"\x1b]9;abc\x1b\\");
    }
}
