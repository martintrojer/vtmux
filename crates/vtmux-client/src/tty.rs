//! Raw tty lifetime. Terminal escapes belong to the server (D14).
use rustix::termios::{self, OptionalActions, Termios};
use std::io;

/// The terminal's saved modes and file status flags; raw while it lives.
pub struct RawTty(Termios, rustix::fs::OFlags);

impl RawTty {
    /// Raw from here on; `handed` is the terminal's own settings when the
    /// image before this one left it raw across a live upgrade's re-exec
    /// ([`Handed`]), which are put back on the way out.
    pub fn enter(handed: Option<&Termios>) -> io::Result<Self> {
        let stdin = io::stdin();
        let saved = match handed {
            Some(saved) => saved.clone(),
            None => termios::tcgetattr(&stdin)?,
        };
        let flags = rustix::fs::fcntl_getfl(&stdin)?;
        let guard = Self(saved, flags);
        guard.resume()?;
        Ok(guard)
    }

    /// The terminal's own settings as the next image takes them in
    /// `VTMUX_REATTACH_TTY` ([`Handed::parse`]): the termios stay raw
    /// across the exec, only the file status flags go back (`O_NONBLOCK`
    /// is shared by every process on the terminal), so a byte typed
    /// while the client re-executes is input as typed. Restored, the line
    /// discipline cooked it (`ICRNL`, erase, signals, the line limit).
    #[must_use]
    pub fn hand_over(&self) -> String {
        let _ = rustix::fs::fcntl_setfl(io::stdin(), self.1);
        handover_text(&self.0)
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

/// The terminal's own settings, handed over raw by the image before a
/// live upgrade's re-exec ([`RawTty::hand_over`]): put back when this
/// process leaves before it attaches (refused, the server gone), as the
/// attach does when it ends ([`RawTty::enter`]).
pub struct Handed(Termios);

impl Handed {
    /// Takes the terminal over: its own settings are those in `text`
    /// (what raw mode changes, `c_iflag`, `c_oflag` and `c_lflag` whole,
    /// `VMIN`, `VTIME`, over what the terminal has now, the rest of which
    /// raw mode left as it was), or sane cooked ones for a text this build
    /// does not read ([`sane`]), never the raw ones it has now. Its output
    /// modes go back at once, its input staying raw: what this process
    /// prints before it attaches (why it could not, the server's `Exit`)
    /// gets its carriage returns. `None` with no terminal.
    #[must_use]
    pub fn take_over(text: &str) -> Option<Self> {
        let now = termios::tcgetattr(io::stdin()).ok()?;
        let saved = settings_over(text, now.clone()).unwrap_or_else(|| sane(now.clone()));
        let mut output = now;
        output.output_modes = saved.output_modes;
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &output);
        Some(Self(saved))
    }

    #[must_use]
    pub fn settings(&self) -> &Termios {
        &self.0
    }
}

impl Drop for Handed {
    fn drop(&mut self) {
        let _ = termios::tcsetattr(io::stdin(), OptionalActions::Now, &self.0);
    }
}

/// `VTMUX_REATTACH_TTY`'s text for `saved` (protocol module docs). The
/// flag words go as `u64`: rustix's `bits()` is `u32` on Linux and `u64`
/// on macOS (`tcflag_t`).
#[allow(clippy::useless_conversion)] // u64 -> u64 on macOS
fn handover_text(saved: &Termios) -> String {
    format!(
        "{:x},{:x},{:x},{},{}",
        u64::from(saved.input_modes.bits()),
        u64::from(saved.output_modes.bits()),
        u64::from(saved.local_modes.bits()),
        saved.special_codes[termios::SpecialCodeIndex::VMIN],
        saved.special_codes[termios::SpecialCodeIndex::VTIME],
    )
}

/// `now` (raw) made cooked as `stty sane` makes a terminal, for the bits
/// raw mode changes: line editing, echo, signals and output processing on,
/// CR read as NL, a BREAK interrupting, `VMIN` 1 and `VTIME` 0; and XON/XOFF
/// flow control (`IXON`) on, which `stty sane` leaves as it finds it but a
/// fresh terminal has and raw mode cleared.
fn sane(mut now: Termios) -> Termios {
    use termios::{InputModes as I, LocalModes as L, OutputModes as O, SpecialCodeIndex as S};
    now.input_modes
        .remove(I::IGNBRK | I::INLCR | I::IGNCR | I::IXOFF | I::ISTRIP);
    now.input_modes
        .insert(I::BRKINT | I::ICRNL | I::IXON | I::IMAXBEL);
    now.output_modes.remove(O::OCRNL | O::ONLRET);
    now.output_modes.insert(O::OPOST | O::ONLCR);
    now.local_modes.remove(L::ECHONL | L::ECHOPRT);
    now.local_modes.insert(
        L::ISIG | L::ICANON | L::IEXTEN | L::ECHO | L::ECHOE | L::ECHOK | L::ECHOCTL | L::ECHOKE,
    );
    now.special_codes[S::VMIN] = 1;
    now.special_codes[S::VTIME] = 0;
    now
}

/// The settings [`handover_text`] wrote, over `now` (the terminal raw).
#[allow(clippy::unnecessary_fallible_conversions, clippy::useless_conversion)] // u64 -> u64 on macOS
fn settings_over(text: &str, mut now: Termios) -> Option<Termios> {
    let words: Vec<&str> = text.split(',').collect();
    let [i, o, l, vmin, vtime] = words.as_slice() else {
        return None;
    };
    let hex = |w: &str| u64::from_str_radix(w, 16).ok();
    now.input_modes = termios::InputModes::from_bits_retain(hex(i)?.try_into().ok()?);
    now.output_modes = termios::OutputModes::from_bits_retain(hex(o)?.try_into().ok()?);
    now.local_modes = termios::LocalModes::from_bits_retain(hex(l)?.try_into().ok()?);
    now.special_codes[termios::SpecialCodeIndex::VMIN] = vmin.parse().ok()?;
    now.special_codes[termios::SpecialCodeIndex::VTIME] = vtime.parse().ok()?;
    Some(now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use termios::{ControlModes, InputModes, LocalModes, OutputModes};

    /// A terminal as a shell leaves it, with the settings `cfmakeraw`
    /// would reset: a BREAK that interrupts, parity checking, 7 bits.
    fn cooked() -> Termios {
        // The slave side: macOS refuses `tcgetattr` on a master that has
        // no slave open (ENOTTY), Linux does not.
        let master =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)
                .unwrap();
        rustix::pty::grantpt(&master).unwrap();
        rustix::pty::unlockpt(&master).unwrap();
        let name = rustix::pty::ptsname(&master, Vec::new()).unwrap();
        let slave = rustix::fs::open(
            name.as_c_str(),
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let mut t = termios::tcgetattr(&slave).unwrap();
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

    /// A terminal handed over raw across a live upgrade's re-exec gets its
    /// own settings back from the text the image before wrote: every bit
    /// raw mode changed, and nothing else touched. A text this build does
    /// not read is no settings at all, never raw ones taken for the user's.
    #[test]
    fn a_handed_over_terminal_gets_its_own_settings_back() {
        let mut saved = cooked();
        saved.special_codes[termios::SpecialCodeIndex::VMIN] = 4;
        saved.special_codes[termios::SpecialCodeIndex::VTIME] = 2;
        let raw = raw_modes(&saved);
        let back = settings_over(&handover_text(&saved), raw.clone()).unwrap();
        assert_eq!(back.input_modes, saved.input_modes);
        assert_eq!(back.output_modes, saved.output_modes);
        assert_eq!(back.local_modes, saved.local_modes);
        assert_eq!(back.control_modes, saved.control_modes);
        for code in [
            termios::SpecialCodeIndex::VMIN,
            termios::SpecialCodeIndex::VTIME,
            termios::SpecialCodeIndex::VERASE,
            termios::SpecialCodeIndex::VINTR,
        ] {
            assert_eq!(back.special_codes[code], saved.special_codes[code]);
        }
        assert_eq!(raw_modes(&back).local_modes, raw.local_modes);
        for bad in ["", "1,2,3,4", "1,2,3,4,5,6", "x,2,3,4,5", "1,2,3,256,0"] {
            assert!(settings_over(bad, raw.clone()).is_none(), "{bad:?}");
        }
        // What `take_over` puts back for those: cooked, whatever raw left.
        let cooked = sane(raw.clone());
        assert!(cooked.local_modes.contains(
            LocalModes::ICANON | LocalModes::ECHO | LocalModes::ISIG | LocalModes::IEXTEN
        ));
        assert!(
            cooked
                .input_modes
                .contains(InputModes::ICRNL | InputModes::IXON)
        );
        assert!(!cooked.input_modes.contains(InputModes::IGNBRK));
        assert!(
            cooked
                .output_modes
                .contains(OutputModes::OPOST | OutputModes::ONLCR)
        );
        assert_eq!(cooked.control_modes, raw.control_modes);
    }
}
