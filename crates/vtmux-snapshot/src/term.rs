//! A pane's terminal state ([`TermState`]): both screens with their
//! history, cell for cell in the vtmux row format ([`crate::row`]), and the
//! state Ghostty and `vtmux-vt`'s `PaneTerm` keep beside the grid.
//!
//! The record is typed but opaque to everything except the pane-state
//! conversion (`vtmux_server::snapshot_term`): the server only moves it
//! between a pane and the snapshot. Deadlines are carried as the
//! milliseconds left, past instants as the milliseconds since; both count
//! from the moment the snapshot was taken.
//!
//! Rows of a screen are numbered as the emulator numbers them: history
//! first, oldest row 0, then the visible rows. A screen's history pages
//! and visible rows are cut where the writer liked; [`Screen::pages`]
//! records how the emulator split them into its own pages, a hint the
//! reader follows when it adds up.

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
        /// No 94-character set: UTF-8 as received (Ghostty's default).
        5 => Utf8,
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
        /// The emulator's cursor shape (Ghostty's `CursorStyle` number).
        8 => one visual: u32,
        /// The pen's content kind ends at the end of the line (OSC 133).
        9 => one semantic_clear_eol: bool,
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
    /// What `?1049h` saved for `?1049l` (tmux's `saved_cx`, `saved_cy` and
    /// `saved_cell`), kept after the alternate screen ends: `x` is the
    /// column count while a wrap was pending.
    pub struct AltSaved {
        1 => one x: u32,
        2 => one y: u32,
        3 => one style: Style,
    }
}

record! {
    /// A link the pane's cells were drawn with: the emulator's URI and id
    /// for it, and the number of the external id the outer terminal was
    /// given (`tmuxN`, tmux's `hyperlinks_put`). A reader without it gives
    /// the pane's links new numbers as it draws them.
    pub struct DrawnLink {
        1 => one uri: Vec<u8>,
        /// The `id=` parameter; without one, the emulator's own id.
        2 => opt id: Option<Vec<u8>>,
        3 => one implicit: u32,
        /// An emulator id of the alternate screen's (each screen counts
        /// its own).
        4 => one alternate: bool,
        5 => one number: u64,
    }
}

record! {
    /// tmux's pen hyperlink and the copies of the pen it keeps, each as the
    /// OSC 8 payload (`params;uri`) that set it, and the link the
    /// emulator's cursor has ([`TermState::pen_links`]). A reader without
    /// it keeps the emulator's link from [`Cursor`] and restores no link on
    /// DECRC or `?1049l`, as builds before it did.
    pub struct PenLinks {
        /// `ictx->cell.cell.link`.
        1 => opt pen: Option<Vec<u8>>,
        /// DECSC's copy (`ictx->old_cell`).
        2 => opt saved: Option<Vec<u8>>,
        /// The last alternate screen entry's copy (`s->saved_cell`).
        3 => opt alternate: Option<Vec<u8>>,
        4 => opt emulator: Option<Vec<u8>>,
    }
}

record! {
    /// The primary screen while the alternate screen leaves it at the size
    /// it had (tmux resizes it when it comes back), when that is not the
    /// terminal's ([`TermState::sized_primary`]).
    pub struct SizedPrimary {
        1 => one cols: u32,
        2 => one rows: u32,
        /// The screen at that size: its visible rows are that many. Absent
        /// when [`TermState::primary`] is this screen, which only a reader
        /// with this field takes: an upgrade onto a binary of the writer's
        /// Ghostty patch stack (live upgrade spec, decision 23), which has
        /// it. Otherwise [`TermState::primary`] is the screen resized to the
        /// terminal's size and reflowed, as a reader from before this field
        /// keeps it (its emulator resized it at every resize), and
        /// [`TermState::marks`] and [`AltEntry::entry_row`] count that
        /// screen's rows.
        3 => opt screen: Option<Screen>,
        /// [`TermState::marks`] and [`AltEntry::entry_row`] in the rows of
        /// [`SizedPrimary::screen`], when it is there.
        4 => many marks: Vec<FrozenMark>,
        5 => opt entry_row: Option<u64>,
    }
}

record! {
    /// Where `?1049h`'s save is for a reader from before
    /// [`TermState::alternate_saved`], which restores `?1049l` from the
    /// primary screen's DECSC slot: every writer with that field writes
    /// this record ([`TermState::alternate_saved_layout`]). Without it (an
    /// older writer), the primary screen's DECSC slot held `?1049h`'s save
    /// while the alternate screen was up with mode 1049.
    pub struct AltSaveLayout {
        /// [`TermState::primary`]'s [`Screen::saved_cursor`] is `?1049h`'s
        /// save, put there for an older reader; the slot's own value is
        /// [`AltSaveLayout::primary_saved`].
        1 => one moved: bool,
        2 => opt primary_saved: Option<SavedCursor>,
    }
}

record! {
    /// One screen: its visible rows, its history and its cursor state.
    /// The alternate screen has no history.
    pub struct Screen {
        /// The visible rows, top first; tags 13 and 14 their line flags
        /// ([`Row::kept`], [`Row::lacks`]), 15 the cells they keep past
        /// the width ([`Row::past`]).
        1 / 13 / 14 / 15 => many rows: Vec<Row>,
        /// History, oldest page first; each page's rows oldest first.
        2 => many history: Vec<HistoryPage>,
        3 => one cursor: Cursor,
        4 => opt saved_cursor: Option<SavedCursor>,
        5 => one charsets: Charsets,
        /// The kitty keyboard flags stack as the emulator keeps it: a ring
        /// of eight entries, [`Screen::kitty_index`] the current one.
        6 => many kitty_keyboard: Vec<u32>,
        7 => one kitty_index: u32,
        /// DECSCA / SPA protection in force: 0 off, 1 ISO, 2 DEC.
        8 => one protected_mode: u32,
        /// OSC 133 click handling: kind and value (Ghostty's numbers).
        9 => one semantic_click_kind: u32,
        10 => one semantic_click_value: u32,
        /// The next implicit OSC 8 hyperlink id.
        11 => one hyperlink_counter: u32,
        /// The emulator's pages, oldest first: how its rows (history, then
        /// the visible ones) were split. Pages in the emulator's own form
        /// ([`HistoryPage::ghostty`]) are not listed: they are pages of
        /// their own.
        12 => many pages: Vec<PageLayout>,
    }
}

record! {
    /// One of the emulator's pages of rows: how many, how wide, and the
    /// allocation it had (hints; zero when unknown).
    pub struct PageLayout {
        1 => one rows: u32,
        2 => one cols: u32,
        3 => one capacity_rows: u32,
        4 => one capacity_styles: u32,
        5 => one capacity_hyperlink_bytes: u32,
        6 => one capacity_grapheme_bytes: u32,
        7 => one capacity_string_bytes: u32,
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
    /// One DEC private (`?n`) or ANSI mode.
    pub struct Mode {
        1 => one number: u32,
        2 => one ansi: bool,
        3 => one on: bool,
        /// Its value saved by XTSAVE (`CSI ? n s`).
        4 => one saved: bool,
        /// Its value at reset, when the writer knew it.
        5 => opt default: Option<bool>,
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
        /// `MODE_CURSOR_VERY_VISIBLE` (`CSI 34 l`). Written under
        /// [`crate::CURSOR_VERY_VISIBLE_FEATURE`] when set.
        5 => one very_visible: bool,
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
    /// A row OSC 133 marked: its number in the primary screen
    /// ([`TermState::marks`]), or its index in [`AltEntry::history`].
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
        1 / 11 => many saved: Vec<Row>,
        /// tmux's primary history, oldest first, as a reader from before
        /// Ghostty patch 0038 shows it under the alternate screen: the
        /// newest `hsize` rows of `primary`'s history, written with rows
        /// only (a reader of the same patch stack reads them in place).
        /// Not read since that patch: `primary` has them.
        2 => many history: Vec<HistoryPage>,
        3 => many marks: Vec<FrozenMark>,
        /// History rows Ghostty kept when the alternate screen came up.
        4 => one ghostty_history: u64,
        /// ED3 under the alternate screen, applied on the way back.
        5 => one clear_primary_history: bool,
        /// Rows `set_history_limit` collected meanwhile.
        6 => one collected: u64,
        /// The primary row tmux's history started at when the alternate
        /// screen came up, while the emulator still has it.
        7 => opt entry_row: Option<u64>,
        /// Whether [`AltEntry::saved`] and [`AltEntry::history`] were
        /// taken (the alternate screen came up since the last return to
        /// the primary one). `has_history` is written only with
        /// [`AltEntry::history`].
        8 => one has_saved: bool,
        9 => one has_history: bool,
        // 10: the cursor's row in `saved` that the primary screen was
        // resized with when the alternate one ended, while the emulator
        // resized the primary under it. Since the emulator leaves it at
        // its size (`TermState::sized_primary`) it is not written or read.
    }
}

record! {
    /// tmux's `MODE_SYNC`: the screen a pane in synchronized output keeps
    /// showing.
    pub struct SyncHold {
        1 / 6 => many rows: Vec<Row>,
        /// Since the last `?2026h` (the 1 s timer re-arms on each).
        2 => one since_ms: u64,
        /// The held screen's width, cursor and whether it was shown.
        3 => one cols: u32,
        4 => opt cursor: Option<(u32, u32)>,
        5 => one cursor_visible: bool,
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
        /// Rows scrolled into history, and those a resize moved there less
        /// those it took back: the newest row's id plus one.
        4 => one scrolled: u64,
        /// tmux's `scroll_generation`.
        5 => one generation: u64,
        /// The line limit last given to Ghostty.
        6 => opt line_limit: Option<u64>,
        /// History rows read since the compressor last looked.
        7 => one reads: u64,
        /// The history id [`HistoryState::times`] starts at.
        8 => one times_base: u64,
        /// When each history row from `times_base` on scrolled in
        /// (`grid_line_time`), as runs.
        9 => many times: Vec<TimeRun>,
        /// The emulator's history byte budget.
        10 => opt max_bytes: Option<u64>,
        /// tmux's `hscrolled`: the newest history rows a taller screen
        /// pulls back. A reader takes all of `hsize` without it, and
        /// starts the times no later than where they end (a writer without
        /// it could leave `times_base` past the newest row).
        11 => opt hscrolled: Option<u64>,
    }
}

record! {
    /// `count` history rows that scrolled in at `time`, epoch seconds.
    pub struct TimeRun {
        1 => one time: i64,
        2 => one count: u64,
    }
}

record! {
    /// tmux's own charset state (`s->mode`'s ACS bits, input.c): G0 and G1
    /// line drawing, SO in force, and what DECSC saved of them.
    pub struct TmuxCharset {
        1 => one g0_acs: bool,
        2 => one g1_acs: bool,
        3 => one shifted: bool,
        4 => one saved_g0_acs: bool,
        5 => one saved_g1_acs: bool,
        6 => one saved_shifted: bool,
    }
}

record! {
    /// Emulator state tmux has no counterpart for, in Ghostty's numbers at
    /// the writer's pin. A reader of another pin takes what it knows.
    pub struct Emulator {
        1 => one status_display: u32,
        /// The last printed character, for REP.
        2 => opt previous_char: Option<u32>,
        3 => one cursor_is_default: bool,
        4 => one cursor_default_style: u32,
        5 => opt cursor_default_blink: Option<bool>,
        6 => one shell_redraw: u32,
        7 => one modify_other_keys_2: bool,
        8 => one mouse_event: u32,
        9 => one mouse_format: u32,
        10 => opt mouse_shift_capture: Option<bool>,
        11 => one mouse_shape: u32,
        12 => one password_input: bool,
        13 => one xt_checksum: u32,
        /// The emulator's own palette overrides and OSC 10/11/12 colours
        /// (tmux's are [`TermState::palette`] and the rest).
        14 => many palette: Vec<PaletteEntry>,
        15 => opt fg: Option<u32>,
        16 => opt bg: Option<u32>,
        17 => opt cursor_colour: Option<u32>,
    }
}

record! {
    /// tmux's OSC 9;4 progress bar (`struct progress_bar`).
    pub struct ProgressBar {
        /// `enum progress_bar_state`: 0 hidden, 1 normal, 2 error,
        /// 3 indeterminate, 4 paused.
        1 => one state: u32,
        /// 0 to 100.
        2 => one progress: u32,
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
        /// Every mode the writer's emulator knows; a reader keeps its own
        /// value for one not listed.
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
        /// OSC 133 marks on the primary screen, in the order they were
        /// made (rows carry no mark of their own here).
        32 => many marks: Vec<FrozenMark>,
        33 => one tmux_charset: TmuxCharset,
        /// The `extended-keys` option the pane last had: 0 off, 1 on,
        /// 2 always.
        34 => one extended_keys: u32,
        35 => one emulator: Emulator,
        /// tmux's `INPUT_LAST`: the last thing parsed was a printed
        /// character, which a REP repeats (input.c `INPUT_CSI_REP`).
        36 => one input_last: bool,
        /// The primary screen while the alternate screen leaves it at the
        /// size it had, when that is not the terminal's. A reader without
        /// it takes [`TermState::primary`], at the terminal's size.
        37 => opt sized_primary: Option<SizedPrimary>,
        /// What `?1049h` saved, if anything.
        38 => opt alternate_saved: Option<AltSaved>,
        /// Always written beside [`TermState::alternate_saved`].
        39 => opt alternate_saved_layout: Option<AltSaveLayout>,
        /// The progress bar. Not required: a program sets one in an
        /// ordinary session, and a reader that skips it shows it hidden at
        /// 0 until the program sets it again.
        40 => one progress_bar: ProgressBar,
        /// Written when any of them has a link.
        41 => opt pen_links: Option<PenLinks>,
        /// The program holds mode 2048, in-band resize (D31). Written
        /// under [`crate::INBAND_RESIZE_FEATURE`] when set: a reader that
        /// skipped it would end the grant and send no reports, and the
        /// program, which stopped handling `SIGWINCH` once granted, would
        /// draw at the old size.
        42 => one inband_resize: bool,
        /// The links the pane's cells were drawn with.
        43 => many drawn_links: Vec<DrawnLink>,
        /// Each screen is granted kitty graphics (D33), primary then
        /// alternate. Written under [`crate::GRAPHICS_GRANT_FEATURE`] when
        /// set: a reader that skipped them would take a granted program's
        /// next image as a title. The images are never written: a resumed
        /// screen has none until its program sends them again.
        44 => one graphics_primary: bool,
        45 => one graphics_alternate: bool,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::row::tests::sample_row;
    use crate::row::{Color, HistoryPage, Row};
    use crate::wire::{Ctx, Value};

    fn round_trip<T: Value + PartialEq + std::fmt::Debug>(v: &T) {
        let mut out = Vec::new();
        v.put(&mut out);
        assert_eq!(&T::get(&out, &Ctx::new(1)).unwrap(), v);
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
                    implicit: 0,
                }),
                semantic: 1,
                ..Cursor::default()
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
            ..Screen::default()
        }
    }

    /// A terminal state with every field set.
    #[allow(clippy::too_many_lines)]
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
                    ..Mode::default()
                },
                Mode {
                    number: 4,
                    ansi: true,
                    on: true,
                    ..Mode::default()
                },
                Mode {
                    number: 7,
                    ansi: false,
                    on: false,
                    ..Mode::default()
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
                very_visible: false,
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
                ..HistoryState::default()
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
                ..AltEntry::default()
            }),
            sync: Some(SyncHold {
                rows: vec![sample_row()],
                since_ms: 300,
                ..SyncHold::default()
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
            // Fields added after the golden file stay out of it (lib.rs,
            // *Changing the format*): `later_fields` covers them.
            ..TermState::default()
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

    /// The primary screen's size under the alternate one and `?1049h`'s
    /// saved cursor, added after the golden file was written.
    #[test]
    fn alternate_screen_fields() {
        let mut t = sample_term();
        t.sized_primary = Some(SizedPrimary {
            cols: 30,
            rows: 4,
            screen: Some(t.primary.clone()),
            marks: t.marks.clone(),
            entry_row: Some(2),
        });
        t.alternate_saved = Some(AltSaved {
            x: 20,
            y: 3,
            style: t.primary.cursor.style,
        });
        t.alternate_saved_layout = Some(AltSaveLayout {
            moved: true,
            primary_saved: t.primary.saved_cursor.clone(),
        });
        round_trip(&t);
        round_trip(t.alternate_saved.as_ref().unwrap());
        round_trip(t.sized_primary.as_ref().unwrap());
        round_trip(t.alternate_saved_layout.as_ref().unwrap());
        // A writer always lists the layout, even with nothing moved.
        t.alternate_saved_layout = Some(AltSaveLayout::default());
        round_trip(&t);
    }

    /// tmux's pen link and its copies, added after the golden file was
    /// written.
    #[test]
    fn pen_links_field() {
        let mut t = sample_term();
        t.pen_links = Some(PenLinks {
            pen: Some(b"id=a;http://x/".to_vec()),
            saved: None,
            alternate: Some(b";http://y/".to_vec()),
            emulator: Some(b"id=a;http://x/".to_vec()),
        });
        round_trip(&t);
        round_trip(t.pen_links.as_ref().unwrap());
    }

    /// The links a pane's cells were drawn with, added after the golden
    /// file was written; a stream without them reads as none.
    #[test]
    fn drawn_links_field() {
        let mut t = sample_term();
        assert_eq!(t.drawn_links, []);
        t.drawn_links = vec![
            DrawnLink {
                uri: b"http://x/".to_vec(),
                id: Some(b"a".to_vec()),
                number: 7,
                ..DrawnLink::default()
            },
            DrawnLink {
                uri: b"http://y/".to_vec(),
                implicit: 3,
                alternate: true,
                number: 0x1_0000_0000,
                ..DrawnLink::default()
            },
        ];
        round_trip(&t);
        round_trip(&t.drawn_links[1]);
    }

    /// Every field added after the golden file was written, set.
    #[test]
    fn later_fields() {
        let mut t = sample_term();
        t.primary.cursor.visual = 2;
        t.primary.cursor.semantic_clear_eol = true;
        t.primary.charsets.g1 = Charset::Utf8;
        t.primary.kitty_keyboard = vec![1, 3, 0, 0, 0, 0, 0, 9];
        t.primary.kitty_index = 7;
        t.primary.protected_mode = 2;
        t.primary.semantic_click_kind = 1;
        t.primary.semantic_click_value = 3;
        t.primary.hyperlink_counter = 12;
        t.primary.pages = vec![PageLayout {
            rows: 3,
            cols: 20,
            capacity_rows: 200,
            capacity_styles: 16,
            capacity_hyperlink_bytes: 64,
            capacity_grapheme_bytes: 128,
            capacity_string_bytes: 256,
        }];
        t.modes[0].saved = true;
        t.modes[0].default = Some(false);
        t.modes[1].default = Some(true);
        t.history.times_base = 3;
        t.history.times = vec![TimeRun {
            time: 1_790_000_000,
            count: 7,
        }];
        t.history.max_bytes = Some(1 << 20);
        t.history.hscrolled = Some(2);
        t.cursor_style.very_visible = true;
        t.progress_bar = ProgressBar {
            state: 3,
            progress: 42,
        };
        let alt = t.alt_entry.as_mut().unwrap();
        alt.entry_row = Some(4);
        alt.has_saved = true;
        alt.has_history = true;
        let sync = t.sync.as_mut().unwrap();
        sync.cols = 20;
        sync.cursor = Some((1, 2));
        sync.cursor_visible = true;
        t.marks = vec![FrozenMark {
            row: 9,
            mark: sample_row().mark.unwrap().into(),
        }];
        t.tmux_charset = TmuxCharset {
            g0_acs: true,
            g1_acs: true,
            shifted: true,
            saved_g0_acs: true,
            saved_g1_acs: true,
            saved_shifted: true,
        };
        t.extended_keys = 2;
        t.input_last = true;
        t.emulator = Emulator {
            status_display: 1,
            previous_char: Some('x' as u32),
            cursor_is_default: true,
            cursor_default_style: 2,
            cursor_default_blink: Some(true),
            shell_redraw: 1,
            modify_other_keys_2: true,
            mouse_event: 3,
            mouse_format: 2,
            mouse_shift_capture: Some(false),
            mouse_shape: 4,
            password_input: true,
            xt_checksum: 1,
            palette: vec![PaletteEntry {
                index: 200,
                rgb: 0x0001_0203,
            }],
            fg: Some(1),
            bg: Some(2),
            cursor_colour: Some(3),
        };
        let link = Hyperlink {
            id: Some(vec![0xfe, b'k']),
            uri: b"http://\xff".to_vec(),
            implicit: 0,
        };
        let mut row = sample_row();
        row.links = vec![
            link.clone(),
            Hyperlink {
                id: None,
                uri: vec![0xc3],
                implicit: 41,
            },
        ];
        row.cells[0].link = 2;
        t.primary.rows.push(row.clone());
        t.primary.cursor.link = Some(link);
        round_trip(&t);
        round_trip(&t.primary.pages[0]);
        round_trip(&t.emulator);
        round_trip(&t.tmux_charset);
        assert_eq!(Row::decode(&row.encode()).unwrap(), row);
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
        assert!(Charset::get(&out, &Ctx::new(1)).is_err());
    }
}
