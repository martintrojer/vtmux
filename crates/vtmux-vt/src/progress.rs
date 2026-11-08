//! The OSC 9;4 progress bar as tmux keeps it per pane (`struct
//! progress_bar` in the base screen): read by `#{pane_pb_state}` and
//! `#{pane_pb_progress}`, and passed to the client's terminal as `Spb`
//! (server-client.c `server_client_set_progress_bar`).

/// `enum progress_bar_state` (tmux.h); the number is what OSC 9;4 and
/// `Spb` carry.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ProgressState {
    #[default]
    Hidden,
    Normal,
    Error,
    Indeterminate,
    Paused,
}

impl ProgressState {
    /// The state numbered `n` (0-4), as OSC 9;4 numbers them.
    #[must_use]
    pub fn from_number(n: u8) -> Option<Self> {
        Some(match n {
            0 => Self::Hidden,
            1 => Self::Normal,
            2 => Self::Error,
            3 => Self::Indeterminate,
            4 => Self::Paused,
            _ => return None,
        })
    }

    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::Hidden => 0,
            Self::Normal => 1,
            Self::Error => 2,
            Self::Indeterminate => 3,
            Self::Paused => 4,
        }
    }

    /// `#{pane_pb_state}` (format.c `format_cb_pane_pb_state`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Hidden => "hidden",
            Self::Normal => "normal",
            Self::Error => "error",
            Self::Indeterminate => "indeterminate",
            Self::Paused => "paused",
        }
    }
}

/// A pane's progress bar: hidden at 0 until a program sets one
/// (`screen_reinit`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ProgressBar {
    pub state: ProgressState,
    /// 0-100; kept by a state that comes without one, and by
    /// `indeterminate` (`screen_set_progress_bar`).
    pub progress: u8,
}

impl ProgressBar {
    /// `input_osc_9` (input.c:3028-3062) on the payload after `9;`: true
    /// when it set the bar (`input_set_progress_bar`, which redraws the
    /// window's borders and status whether or not anything changed).
    pub(crate) fn osc_9(&mut self, p: &[u8]) -> bool {
        let Some((state, progress)) = parse_osc_9(p) else {
            return false;
        };
        // screen_set_progress_bar (screen.c:344-349).
        self.state = state;
        if let Some(n) = progress
            && state != ProgressState::Indeterminate
        {
            self.progress = n;
        }
        true
    }
}

/// `4;STATE[;PROGRESS]`: the state and the progress, if one is given. A
/// bare `4` or `4;` sets nothing, nor does anything else malformed.
fn parse_osc_9(p: &[u8]) -> Option<(ProgressState, Option<u8>)> {
    let rest = p.strip_prefix(b"4")?;
    if rest.is_empty() || rest == b";" {
        return None;
    }
    let rest = rest.strip_prefix(b";")?;
    let (&digit, rest) = rest.split_first()?;
    let state = ProgressState::from_number(digit.wrapping_sub(b'0'))?;
    if rest.is_empty() || rest == b";" {
        return Some((state, None));
    }
    let rest = rest.strip_prefix(b";")?;
    // tmux stops at the first number past 100, so no digit string
    // overflows; no digits at all is 0.
    let mut progress: u32 = 0;
    for &b in rest {
        if !b.is_ascii_digit() || progress > 100 {
            return None;
        }
        progress = progress * 10 + u32::from(b - b'0');
    }
    let progress = u8::try_from(progress).ok().filter(|n| *n <= 100)?;
    Some((state, Some(progress)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each payload in turn, as the reference tmux 3.8 left
    /// `#{pane_pb_state}|#{pane_pb_progress}` after it.
    #[test]
    fn payloads_set_the_bar_as_tmux() {
        let mut bar = ProgressBar::default();
        for (payload, want) in [
            ("4;1;50", "normal|50"),
            ("4;3;70", "indeterminate|50"),
            ("4;0", "hidden|50"),
            ("4;1", "normal|50"),
            ("4;2;101", "normal|50"),
            ("4;4;", "paused|50"),
            ("4;5;1", "paused|50"),
            ("4;1;7x", "paused|50"),
            ("4;1;", "normal|50"),
            ("4;2;1000", "normal|50"),
            ("4", "normal|50"),
            ("4;2;0050", "error|50"),
            ("4;0;0", "hidden|0"),
            ("4;12;3", "hidden|0"),
            ("4;1;5;", "hidden|0"),
            ("4;3;", "indeterminate|0"),
            ("4;1;100", "normal|100"),
            ("4;1;-1", "normal|100"),
            ("5;1;3", "normal|100"),
            ("4;3;40", "indeterminate|100"),
            ("4;2;", "error|100"),
            ("4;1;99999999999999999999", "error|100"),
        ] {
            bar.osc_9(payload.as_bytes());
            let got = format!("{}|{}", bar.state.name(), bar.progress);
            assert_eq!(got, want, "after {payload:?}");
        }
    }

    #[test]
    fn only_a_well_formed_payload_redraws() {
        let mut bar = ProgressBar::default();
        assert!(bar.osc_9(b"4;0"));
        assert!(bar.osc_9(b"4;0"), "unchanged still redraws");
        assert!(!bar.osc_9(b"4;"));
        assert!(!bar.osc_9(b"4;9"));
        assert!(!bar.osc_9(b""));
    }

    #[test]
    fn states_number_both_ways() {
        for n in 0..=4 {
            assert_eq!(ProgressState::from_number(n).unwrap().number(), n);
        }
        assert_eq!(ProgressState::from_number(5), None);
    }
}
