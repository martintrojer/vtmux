//! A pane's terminal state ([`TermState`]): both screens with their
//! history, cell for cell in the vtmux row format ([`crate::row`]), and the
//! state Ghostty and `vtmux-vt`'s `PaneTerm` keep beside the grid.
//!
//! The record is typed but opaque to everything except the pane-state
//! conversion (task `lu_pane_state`): the server only moves it between a
//! pane and the snapshot. Deadlines are carried as the milliseconds left,
//! past instants as the milliseconds since; both count from the moment the
//! snapshot was taken.

use crate::row::{HistoryPage, Hyperlink, Osc133, Row, Style};
use crate::wire::{oneof, record, value_enum};

value_enum! {
    /// A designated character set (SCS).
    pub enum Charset {
        0 => Ascii,
        1 => DecSpecial,
        2 => British,
        3 => DecSupplemental,
        4 => DecTechnical,
    }
}

record! {
    /// The G0-G3 designations and which are invoked.
    pub struct Charsets {
        1 => one g0: Charset,
        2 => one g1: Charset,
        3 => one g2: Charset,
        4 => one g3: Charset,
        /// Which of G0-G3 is invoked into GL (SI/SO, LS2, LS3).
        5 => one gl: u32,
        /// Which is invoked into GR.
        6 => one gr: u32,
        /// A pending single shift (SS2/SS3): 2 or 3.
        7 => opt single_shift: Option<u32>,
    }
}

record! {
    /// The cursor and its pen.
    pub struct Cursor {
        1 => one x: u32,
        2 => one y: u32,
        /// tmux's `cx` past the last column (Ghostty's pending wrap).
        3 => one pending_wrap: bool,
        /// The pen new cells are written with.
        4 => one style: Style,
        /// The pen's DECSCA protection.
        5 => one protected: bool,
        /// The pen's OSC 8 hyperlink.
        6 => opt link: Option<Hyperlink>,
        /// The pen's OSC 133 content kind: 0 output, 1 input, 2 prompt.
        7 => one semantic: u32,
    }
}

record! {
    /// What DECSC saved.
    pub struct SavedCursor {
        1 => one x: u32,
        2 => one y: u32,
        3 => one pending_wrap: bool,
        4 => one style: Style,
        5 => one protected: bool,
        6 => one origin: bool,
        7 => one charsets: Charsets,
    }
}

record! {
    /// One screen: its visible rows, its history and its cursor state.
    /// The alternate screen has no history.
    pub struct Screen {
        /// The visible rows, top first.
        1 => many rows: Vec<Row>,
        /// History, oldest page first; each page's rows oldest first.
        2 => many history: Vec<HistoryPage>,
        3 => one cursor: Cursor,
        4 => opt saved_cursor: Option<SavedCursor>,
        5 => one charsets: Charsets,
        /// The kitty keyboard flags stack, oldest first.
        6 => many kitty_keyboard: Vec<u32>,
    }
}

value_enum! {
    /// Which screen is up.
    pub enum ActiveScreen {
        0 => Primary,
        1 => Alternate,
    }
}

record! {
    /// One DEC private (`?n`) or ANSI mode that differs from its default.
    pub struct Mode {
        1 => one number: u32,
        2 => one ansi: bool,
        3 => one on: bool,
    }
}

record! {
    /// The scrolling margins (DECSTBM, DECSLRM), inclusive.
    pub struct ScrollRegion {
        1 => one top: u32,
        2 => one bottom: u32,
        3 => one left: u32,
        4 => one right: u32,
    }
}

record! {
    /// An OSC 4 palette override.
    pub struct PaletteEntry {
        1 => one index: u32,
        /// `0xRRGGBB`.
        2 => one rgb: u32,
    }
}

record! {
    /// tmux's cursor style state (`screen_set_cursor_style`).
    pub struct CursorStyle {
        /// DECSCUSR shape: 0 default, 1 block, 2 underline, 3 bar.
        1 => one shape: u32,
        2 => one blinking: bool,
        3 => one blinking_set: bool,
        /// The pane's `cursor-style` option, 0..=6.
        4 => one option: u32,
    }
}

record! {
    /// tmux's `MODE_MOUSE_*` bits.
    pub struct MouseModes {
        /// 0 none, 1000, 1002 or 1003.
        1 => one tracking: u32,
        2 => one utf8: bool,
        3 => one sgr: bool,
    }
}

record! {
    /// A row OSC 133 marked while the history was frozen for the alternate
    /// screen: its index in [`AltEntry::history`].
    pub struct FrozenMark {
        1 => one row: u64,
        2 => one mark: Osc133Rec,
    }
}

record! {
    /// [`Osc133`] as a record (rows carry it in their compact form).
    pub struct Osc133Rec {
        1 => one flags: u32,
        2 => one prompt_col: u32,
        3 => one cmd_col: u32,
        4 => one out_start_col: u32,
        5 => one out_end_col: u32,
        6 => one exit_status: u32,
    }
}

impl From<Osc133> for Osc133Rec {
    fn from(m: Osc133) -> Self {
        Self {
            flags: m.flags,
            prompt_col: m.prompt_col.into(),
            cmd_col: m.cmd_col.into(),
            out_start_col: m.out_start_col.into(),
            out_end_col: m.out_end_col.into(),
            exit_status: m.exit_status.into(),
        }
    }
}

record! {
    /// What `PaneTerm` copied of the primary screen as the alternate screen
    /// came up (tmux's `saved_grid`), while it is up.
    pub struct AltEntry {
        /// The primary's visible rows then.
        1 => many saved: Vec<Row>,
        /// tmux's primary history then, oldest first.
        2 => many history: Vec<HistoryPage>,
        3 => many marks: Vec<FrozenMark>,
        /// History rows Ghostty kept when the alternate screen came up.
        4 => one ghostty_history: u64,
        /// ED3 under the alternate screen, applied on the way back.
        5 => one clear_primary_history: bool,
        /// Rows `set_history_limit` collected meanwhile.
        6 => one collected: u64,
    }
}

record! {
    /// tmux's `MODE_SYNC`: the screen a pane in synchronized output keeps
    /// showing.
    pub struct SyncHold {
        1 => many rows: Vec<Row>,
        /// Since the last `?2026h` (the 1 s timer re-arms on each).
        2 => one since_ms: u64,
    }
}

oneof! {
    /// A reply the pane is still owed, in input order.
    pub enum HeldReply {
        /// Bytes to write back to the pane.
        1 => Bytes(Vec<u8>),
        /// A colour query (OSC 10/11/12, 4) waiting for the client's
        /// answer: the query as received.
        2 => Colour(Vec<u8>),
    }
}

record! {
    /// History bookkeeping `PaneTerm` models after tmux's grid.
    pub struct HistoryState {
        /// `history-limit`.
        1 => one limit: u64,
        /// tmux's `GRID_HISTORY`: the pane keeps history for good.
        2 => one enabled: bool,
        /// tmux's `hsize` of the primary grid.
        3 => one hsize: u64,
        /// Rows ever scrolled in (`hscrolled`).
        4 => one scrolled: u64,
        /// tmux's `scroll_generation`.
        5 => one generation: u64,
        /// The line limit last given to Ghostty.
        6 => opt line_limit: Option<u64>,
        /// History rows read since the compressor last looked.
        7 => one reads: u64,
    }
}

record! {
    /// A pane's whole terminal state.
    pub struct TermState {
        1 => one cols: u32,
        2 => one rows: u32,
        3 => one primary: Screen,
        4 => opt alternate: Option<Screen>,
        5 => one active: ActiveScreen,
        /// Modes not at their default.
        6 => many modes: Vec<Mode>,
        /// Columns with a tab stop.
        7 => many tab_stops: Vec<u32>,
        8 => one scroll_region: ScrollRegion,
        /// The title the program set (OSC 0/2).
        9 => one title: String,
        /// A title the server set over it (`select-pane -T`).
        10 => opt server_title: Option<String>,
        /// tmux's title stack (`CSI 22 t`), newest first.
        11 => many title_stack: Vec<String>,
        /// OSC 7 working directory.
        12 => one pwd: String,
        13 => many palette: Vec<PaletteEntry>,
        /// OSC 10/11/12 overrides, `0xRRGGBB`.
        14 => opt fg: Option<u32>,
        15 => opt bg: Option<u32>,
        16 => opt cursor_colour: Option<u32>,
        17 => one cursor_style: CursorStyle,
        18 => one mouse: MouseModes,
        /// xterm `modifyOtherKeys`.
        19 => one modify_other_keys: u32,
        20 => one history: HistoryState,
        21 => opt alt_entry: Option<AltEntry>,
        22 => opt sync: Option<SyncHold>,
        23 => many replies: Vec<HeldReply>,
        /// Input since the parser last left the ground state.
        24 => one pending: Vec<u8>,
        /// A DCS payload being read (tmux's own DCS parser).
        25 => one dcs: Vec<u8>,
        26 => opt dcs_since_ms: Option<u64>,
        /// A screen-style window name (`ESC k`) being read.
        27 => opt rename: Option<Vec<u8>>,
        /// `input-buffer-size`.
        28 => one input_buffer_size: u64,
        /// `scroll-on-clear`.
        29 => one scroll_on_clear: bool,
        /// BELs not yet taken.
        30 => one bells: u32,
        /// A full redraw is due (`PANE_REDRAW`).
        31 => one redraw: bool,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::row::tests::sample_row;
    use crate::row::{Color, HistoryPage};
    use crate::wire::{Ctx, Value};

    fn round_trip<T: Value + PartialEq + std::fmt::Debug>(v: &T) {
        let mut out = Vec::new();
        v.put(&mut out);
        assert_eq!(&T::get(&out, &Ctx { version: 1 }).unwrap(), v);
    }

    fn charsets() -> Charsets {
        Charsets {
            g0: Charset::DecSpecial,
            g1: Charset::British,
            g2: Charset::DecSupplemental,
            g3: Charset::DecTechnical,
            gl: 1,
            gr: 2,
            single_shift: Some(3),
        }
    }

    fn screen() -> Screen {
        Screen {
            rows: vec![sample_row(), Row::default(), sample_row()],
            history: vec![
                HistoryPage::from_rows(&[sample_row(), sample_row()], true),
                HistoryPage::from_rows(&[Row::default()], false),
            ],
            cursor: Cursor {
                x: 3,
                y: 2,
                pending_wrap: true,
                style: Style {
                    fg: Color::Palette256(3),
                    attrs: crate::row::attr::BLINK,
                    ..Style::default()
                },
                protected: true,
                link: Some(Hyperlink {
                    id: None,
                    uri: "file:///x".into(),
                }),
                semantic: 1,
            },
            saved_cursor: Some(SavedCursor {
                x: 1,
                y: 1,
                pending_wrap: true,
                style: Style::default(),
                protected: true,
                origin: true,
                charsets: charsets(),
            }),
            charsets: charsets(),
            kitty_keyboard: vec![1, 3],
        }
    }

    /// A terminal state with every field set.
    pub(crate) fn sample_term() -> TermState {
        TermState {
            cols: 20,
            rows: 3,
            primary: screen(),
            alternate: Some(Screen {
                history: Vec::new(),
                ..screen()
            }),
            active: ActiveScreen::Alternate,
            modes: vec![
                Mode {
                    number: 1049,
                    ansi: false,
                    on: true,
                },
                Mode {
                    number: 4,
                    ansi: true,
                    on: true,
                },
                Mode {
                    number: 7,
                    ansi: false,
                    on: false,
                },
            ],
            tab_stops: vec![0, 8, 16],
            scroll_region: ScrollRegion {
                top: 1,
                bottom: 2,
                left: 0,
                right: 19,
            },
            title: "vim".into(),
            server_title: Some("pinned".into()),
            title_stack: vec!["a".into(), "b".into()],
            pwd: "file://h/tmp".into(),
            palette: vec![PaletteEntry {
                index: 1,
                rgb: 0x00ff_8000,
            }],
            fg: Some(0x00ff_ffff),
            bg: Some(0),
            cursor_colour: Some(0x0012_3456),
            cursor_style: CursorStyle {
                shape: 2,
                blinking: true,
                blinking_set: true,
                option: 4,
            },
            mouse: MouseModes {
                tracking: 1002,
                utf8: true,
                sgr: true,
            },
            modify_other_keys: 2,
            history: HistoryState {
                limit: 2000,
                enabled: true,
                hsize: 3,
                scrolled: 10,
                generation: 2,
                line_limit: Some(0),
                reads: 4,
            },
            alt_entry: Some(AltEntry {
                saved: vec![sample_row()],
                history: vec![HistoryPage::from_rows(&[sample_row()], false)],
                marks: vec![FrozenMark {
                    row: 0,
                    mark: sample_row().mark.unwrap().into(),
                }],
                ghostty_history: 1,
                clear_primary_history: true,
                collected: 2,
            }),
            sync: Some(SyncHold {
                rows: vec![sample_row()],
                since_ms: 300,
            }),
            replies: vec![
                HeldReply::Bytes(b"\x1b[0n".to_vec()),
                HeldReply::Colour(b"\x1b]11;?\x07".to_vec()),
            ],
            pending: b"\x1b[1".to_vec(),
            dcs: b"tmux;x".to_vec(),
            dcs_since_ms: Some(12),
            rename: Some(b"na".to_vec()),
            input_buffer_size: 1_048_576,
            scroll_on_clear: true,
            bells: 2,
            redraw: true,
        }
    }

    #[test]
    fn every_term_record_round_trips() {
        let t = sample_term();
        round_trip(&t);
        round_trip(&t.primary);
        round_trip(&t.primary.cursor);
        round_trip(t.primary.saved_cursor.as_ref().unwrap());
        round_trip(&charsets());
        round_trip(&t.modes[0]);
        round_trip(&t.scroll_region);
        round_trip(&t.palette[0]);
        round_trip(&t.cursor_style);
        round_trip(&t.mouse);
        round_trip(&t.history);
        round_trip(t.alt_entry.as_ref().unwrap());
        round_trip(&t.alt_entry.as_ref().unwrap().marks[0]);
        round_trip(t.sync.as_ref().unwrap());
        round_trip(&t.replies[0]);
        round_trip(&t.replies[1]);
        round_trip(&TermState::default());
    }

    #[test]
    fn a_default_record_writes_nothing() {
        let mut out = Vec::new();
        TermState::default().put(&mut out);
        assert_eq!(out, Vec::<u8>::new());
    }

    #[test]
    fn unknown_enum_number_is_an_error() {
        let mut out = Vec::new();
        crate::wire::put_varint(&mut out, 99);
        assert!(Charset::get(&out, &Ctx { version: 1 }).is_err());
    }
}
