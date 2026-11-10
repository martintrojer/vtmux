//! Raw tty lifetime. Terminal escapes belong to the server (D14).
use rustix::termios::{self, OptionalActions, Termios};
use std::io;

/// The terminal's saved modes and file status flags; raw while it lives.
pub struct RawTty(Termios, rustix::fs::OFlags);

impl RawTty {
    pub fn enter() -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = termios::tcgetattr(&stdin)?;
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        let guard = Self(saved, flags);
        guard.resume()?;
        Ok(guard)
    }

    /// Raw and non-blocking again after [`RawTty::suspend`].
    pub fn resume(&self) -> io::Result<()> {
        let stdin = io::stdin();
        termios::tcsetattr(&stdin, OptionalActions::Now, &raw_modes(&self.0))?;
        rustix::fs::fcntl_setfl(&stdin, self.1 | rustix::fs::OFlags::NONBLOCK)?;
        Ok(())
    }

    /// The modes and flags the terminal had before [`RawTty::enter`], for
    /// whoever has it while the client is stopped.
    pub fn suspend(&self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
        let _ = rustix::fs::fcntl_setfl(io::stdin(), self.1);
    }
}

/// `saved` as tmux's `tty_start_tty` (tty.c:346-352) sets a terminal: only
/// the bits it names change, so a BREAK is ignored (`IGNBRK`) and the
/// character size, parity and the rest of `c_cflag` stay the user's.
#[must_use]
fn raw_modes(saved: &Termios) -> Termios {
    use termios::{InputModes as I, LocalModes as L, OutputModes as O, SpecialCodeIndex as S};
    let mut raw = saved.clone();
    raw.input_modes
        .remove(I::IXON | I::IXOFF | I::ICRNL | I::INLCR | I::IGNCR | I::IMAXBEL | I::ISTRIP);
    raw.input_modes.insert(I::IGNBRK);
    raw.output_modes
        .remove(O::OPOST | O::ONLCR | O::OCRNL | O::ONLRET);
    raw.local_modes.remove(
        L::IEXTEN
            | L::ICANON
            | L::ECHO
            | L::ECHOE
            | L::ECHONL
            | L::ECHOCTL
            | L::ECHOPRT
            | L::ECHOKE
            | L::ISIG,
    );
    raw.special_codes[S::VMIN] = 1;
    raw.special_codes[S::VTIME] = 0;
    raw
}

impl Drop for RawTty {
    fn drop(&mut self) {
        self.suspend();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use termios::{ControlModes, InputModes, LocalModes, OutputModes};

    /// A terminal as a shell leaves it, with the settings `cfmakeraw`
    /// would reset: a BREAK that interrupts, parity checking, 7 bits.
    fn cooked() -> Termios {
        let pty =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
                .unwrap();
        let mut t = termios::tcgetattr(&pty).unwrap();
        t.input_modes |= InputModes::BRKINT | InputModes::INPCK | InputModes::ICRNL;
        t.input_modes.remove(InputModes::IGNBRK);
        t.control_modes |= ControlModes::PARENB;
        t.control_modes.remove(ControlModes::CSIZE);
        t.control_modes |= ControlModes::CS7;
        t.local_modes |= LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG;
        t.output_modes |= OutputModes::OPOST;
        t
    }

    /// tty.c `tty_start_tty`: BREAK ignored, the editing and signal keys
    /// off, output untranslated; the control modes (parity, bits per
    /// character) and `BRKINT` are not touched.
    #[test]
    fn raw_mode_changes_only_the_bits_tmux_does() {
        let saved = cooked();
        let raw = raw_modes(&saved);
        assert!(raw.input_modes.contains(InputModes::IGNBRK));
        assert!(
            !raw.input_modes
                .intersects(InputModes::ICRNL | InputModes::IXON)
        );
        assert!(
            raw.input_modes
                .contains(InputModes::BRKINT | InputModes::INPCK)
        );
        assert!(!raw.output_modes.contains(OutputModes::OPOST));
        assert!(!raw.local_modes.intersects(
            LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG | LocalModes::IEXTEN
        ));
        assert_eq!(raw.control_modes, saved.control_modes);
        assert!(
            raw.control_modes
                .contains(ControlModes::PARENB | ControlModes::CS7)
        );
        assert_eq!(raw.special_codes[termios::SpecialCodeIndex::VMIN], 1);
        assert_eq!(raw.special_codes[termios::SpecialCodeIndex::VTIME], 0);
    }
}
