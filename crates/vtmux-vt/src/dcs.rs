//! tmux's DCS parser (input.c `input_state_dcs_*`), run ahead of Ghostty.
//!
//! Ghostty ends a DCS at any ESC and interprets what follows, so a
//! `DCS tmux; ESC ESC ... ST` passthrough would leak into the pane grid and
//! title. tmux keeps every DCS to itself: the payload is collected (a doubled
//! ESC is one ESC) and dispatched at ST (`input_dcs_dispatch`), where the
//! only thing it does with a `tmux;` payload is write the rest to the
//! clients (and it answers `DCS $ q`). This module collects the DCS the same
//! way; the pane's terminal never sees it.

/// `INPUT_BUF_DEFAULT_SIZE` (tmux.h), the `input-buffer-size` default.
pub(crate) const DEFAULT_BUFFER_SIZE: usize = 1_048_576;
/// `INPUT_BUF_START` (input.c:118).
const BUF_START: usize = 32;
/// `sizeof ictx->interm_buf` and `param_buf` (input.c:112, 115).
const INTERM_MAX: usize = 3;
const PARAM_MAX: usize = 63;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Enter,
    Param,
    Interm,
    Ignore,
    Handler,
    /// ESC in the handler state: `\` ends the string, anything else is data.
    HandlerEsc,
}

/// What a byte did to the DCS.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Still inside the string.
    Stay,
    /// ST: the DCS is over, and tmux dispatches it.
    End(Dispatch),
    /// CAN or SUB before the string started: back to ground.
    Cancel,
    /// ESC before the string started: an escape sequence begins.
    Escape,
}

/// What tmux does with a finished DCS (`input_dcs_dispatch`, input.c:2641).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// Nothing: an unknown or discarded string.
    Ignore,
    /// `tmux;` payload, unescaped, to pass through to the clients.
    Passthrough(Vec<u8>),
    /// `DCS $ q`: a DECRQSS; true when the request starts `SP q`
    /// (`input_handle_decrqss`, input.c:2564), which tmux answers with the
    /// cursor style.
    Decrqss(bool),
}

#[derive(Debug, Default)]
pub(crate) struct Dcs {
    phase: Phase,
    interm: Vec<u8>,
    params: usize,
    buf: Vec<u8>,
    /// tmux's `input_space`: the buffer's allocation, doubled as it fills.
    space: usize,
    discard: bool,
}

impl Dcs {
    /// `input_enter_dcs`: `input_clear`, and the buffer back to its start
    /// size (`input_ground`).
    pub(crate) fn begin(&mut self) {
        self.phase = Phase::Enter;
        self.interm.clear();
        self.params = 0;
        self.buf.clear();
        self.space = BUF_START;
        self.discard = false;
    }

    /// One byte of the DCS; `limit` is `input-buffer-size`.
    pub(crate) fn push(&mut self, b: u8, limit: usize) -> Step {
        if self.phase == Phase::Handler {
            if b == 0x1b {
                self.phase = Phase::HandlerEsc;
            } else {
                self.input(b, limit);
            }
            return Step::Stay;
        }
        if self.phase == Phase::HandlerEsc {
            if b == b'\\' {
                return Step::End(self.dispatch());
            }
            self.phase = Phase::Handler;
            self.input(b, limit);
            return Step::Stay;
        }
        match b {
            0x18 | 0x1a => return Step::Cancel,
            0x1b => return Step::Escape,
            _ => {}
        }
        match (self.phase, b) {
            (Phase::Enter | Phase::Param, 0x20..=0x2f) => {
                self.intermediate(b);
                self.phase = Phase::Interm;
            }
            (Phase::Interm, 0x20..=0x2f) => self.intermediate(b),
            (Phase::Enter, 0x30..=0x39 | 0x3b) => {
                self.parameter();
                self.phase = Phase::Param;
            }
            (Phase::Param, 0x30..=0x39 | 0x3b) => self.parameter(),
            (Phase::Enter, 0x3c..=0x3f) => {
                self.intermediate(b);
                self.phase = Phase::Param;
            }
            (Phase::Enter | Phase::Param, 0x3a) | (Phase::Param, 0x3c..=0x3f) => {
                self.phase = Phase::Ignore;
            }
            (Phase::Interm, 0x30..=0x3f) => self.phase = Phase::Ignore,
            (Phase::Enter | Phase::Param | Phase::Interm, 0x40..=0x7e) => {
                self.input(b, limit);
                self.phase = Phase::Handler;
            }
            _ => {}
        }
        Step::Stay
    }

    /// `input_intermediate`.
    fn intermediate(&mut self, b: u8) {
        if self.interm.len() == INTERM_MAX {
            self.discard = true;
        } else {
            self.interm.push(b);
        }
    }

    /// `input_parameter`: only the length matters here.
    fn parameter(&mut self) {
        if self.params == PARAM_MAX {
            self.discard = true;
        } else {
            self.params += 1;
        }
    }

    /// `input_input`: collect, doubling the allocation up to `limit`.
    fn input(&mut self, b: u8, limit: usize) {
        let mut available = self.space;
        while self.buf.len() + 1 >= available {
            available *= 2;
            if available > limit {
                self.discard = true;
                return;
            }
            self.space = available;
        }
        self.buf.push(b);
    }

    /// `input_dcs_dispatch`: what a finished DCS hands on.
    fn dispatch(&mut self) -> Dispatch {
        if self.discard {
            return Dispatch::Ignore;
        }
        if let Some(payload) = self.buf.strip_prefix(b"tmux;") {
            return if payload.is_empty() {
                Dispatch::Ignore
            } else {
                Dispatch::Passthrough(payload.to_vec())
            };
        }
        if self.interm == b"$" && self.buf.first() == Some(&b'q') {
            return Dispatch::Decrqss(self.buf.get(1..3) == Some(b" q"));
        }
        Dispatch::Ignore
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(bytes: &[u8], limit: usize) -> Vec<Step> {
        let mut d = Dcs::default();
        d.begin();
        bytes.iter().map(|b| d.push(*b, limit)).collect()
    }

    fn payload(bytes: &[u8]) -> Option<Vec<u8>> {
        match run(bytes, DEFAULT_BUFFER_SIZE).pop() {
            Some(Step::End(Dispatch::Passthrough(p))) => Some(p),
            Some(Step::End(_)) => None,
            other => panic!("not ended: {other:?}"),
        }
    }

    #[test]
    fn doubled_escape_is_one_escape() {
        assert_eq!(
            payload(b"tmux;\x1b\x1b]52;c;aA==\x07\x1b\\").as_deref(),
            Some(&b"\x1b]52;c;aA==\x07"[..])
        );
        // A lone ESC before another byte is dropped.
        assert_eq!(payload(b"tmux;a\x1bb\x1b\\").as_deref(), Some(&b"ab"[..]));
    }

    #[test]
    fn parameters_before_the_payload_are_skipped() {
        assert_eq!(payload(b"1;2tmux;x\x1b\\").as_deref(), Some(&b"x"[..]));
        // `:` ignores the string; its ESC starts a new sequence.
        assert_eq!(run(b"1:2tmux;x\x1b\\", 64).pop(), Some(Step::Stay));
        assert_eq!(run(b"1:2tmux;x\x1b", 64).pop(), Some(Step::Escape));
        assert_eq!(payload(b"$qm\x1b\\"), None);
        assert_eq!(
            run(b"$q q\x1b\\", 64).pop(),
            Some(Step::End(Dispatch::Decrqss(true)))
        );
        assert_eq!(
            run(b"$qm\x1b\\", 64).pop(),
            Some(Step::End(Dispatch::Decrqss(false)))
        );
        assert_eq!(payload(b"xtmux;x\x1b\\"), None);
    }

    #[test]
    fn can_is_data_inside_the_string_and_cancels_before_it() {
        assert_eq!(
            payload(b"tmux;a\x18b\x1b\\").as_deref(),
            Some(&b"a\x18b"[..])
        );
        assert_eq!(run(b"1\x18", 64).pop(), Some(Step::Cancel));
        assert_eq!(run(b"1\x1b", 64).pop(), Some(Step::Escape));
    }

    #[test]
    fn payload_over_the_buffer_size_is_dropped() {
        // limit 64: the buffer holds up to 63 bytes (input_input).
        let ok = [b"tmux;".as_slice(), &[b'x'; 58], b"\x1b\\"].concat();
        assert_eq!(run(&ok, 64).pop(), Some(Step::End(Dispatch::Passthrough(vec![b'x'; 58]))));
        let big = [b"tmux;".as_slice(), &[b'x'; 59], b"\x1b\\"].concat();
        assert_eq!(run(&big, 64).pop(), Some(Step::End(Dispatch::Ignore)));
    }
}
