//! What the terminal-state round trips share: the observations two
//! panes are compared by, the escape-sequence fuzzer, and the oracle
//! pane-program corpus. Included by `state_roundtrip.rs` here and by
//! `vtmux-server`'s `snapshot_term.rs` test (`#[path]`).

#![allow(dead_code, reason = "each test binary uses part of it")]

use vtmux_vt::{CaptureOpts, LineRef, PageState, PaneTerm, TermState, capture, capture_alternate};
/// Everything a script or the compositor can read from a pane.
/// `times`: with each history row's time, which differs between two panes
/// fed the same input in different seconds.
#[expect(clippy::too_many_lines, reason = "one line per thing observed")]
pub fn observe(t: &PaneTerm, times: bool) -> String {
    use std::fmt::Write as _;
    let all = CaptureOpts {
        start: LineRef::Start,
        end: LineRef::End,
        ..CaptureOpts::default()
    };
    let mut out = String::new();
    for (name, opts) in [
        (
            "e",
            CaptureOpts {
                escapes: true,
                ..all
            },
        ),
        (
            "JN",
            CaptureOpts {
                join: true,
                keep_spaces: true,
                ..all
            },
        ),
        (
            "FT",
            CaptureOpts {
                flags: true,
                used_only: true,
                ..all
            },
        ),
        (
            "H",
            CaptureOpts {
                hyperlinks: true,
                ..all
            },
        ),
        (
            "CeI",
            CaptureOpts {
                octal: true,
                escapes: true,
                times,
                ..all
            },
        ),
    ] {
        let _ = writeln!(
            out,
            "-{name}:\n{}",
            String::from_utf8_lossy(&capture(t, opts))
        );
    }
    if let Some(saved) = capture_alternate(
        t,
        CaptureOpts {
            escapes: true,
            ..all
        },
    ) {
        let _ = writeln!(out, "-a:\n{}", String::from_utf8_lossy(&saved));
    }
    let _ = writeln!(
        out,
        "cursor {:?} alt {} hsize {} retained {} limit {} gen {} title {:?} pwd {:?}",
        t.cursor(),
        t.alternate_on(),
        t.history_size(),
        t.retained_rows(),
        t.history_limit(),
        t.history_generation(),
        t.title(),
        t.pwd(),
    );
    let _ = writeln!(
        out,
        "modes ckm {} kpam {} focus {} paste {} mouse {:?} keys {:?} theme {} colours {:?}",
        t.app_cursor(),
        t.app_keypad(),
        t.focus_events(),
        t.bracketed_paste(),
        t.mouse_modes(),
        t.key_mode(),
        t.theme_updates(),
        t.default_colours(),
    );
    let _ = writeln!(
        out,
        "marks {:?} osc133 {:?} pending {:?} sync {} progress {:?}",
        t.line_marks(),
        t.osc133_lines(),
        t.pending_input(),
        t.sync_held().is_some(),
        t.progress_bar(),
    );
    for y in 0..t.history_size().min(if times { 3 } else { 0 }) {
        let id = t.history_id(y);
        let _ = writeln!(out, "history {y} id {id} time {}", t.history_time(id));
    }
    let view = t.screen();
    let _ = writeln!(out, "view {:?}", view.snapshot());
    out
}

/// The export, with what differs between two exports of one terminal
/// (the time left on a sync hold) zeroed.
pub fn exported(t: &PaneTerm) -> TermState {
    let mut s = t.export_state();
    if let Some((_, left)) = s.pane.sync.as_mut() {
        *left = std::time::Duration::ZERO;
    }
    s
}

/// A screen's rows, each with its flags and cells, styles and links
/// resolved: the same for any split of the rows into pages and any page
/// capacity.
pub fn flat_rows(screen: &vtmux_vt::ScreenState) -> Vec<String> {
    use std::fmt::Write as _;
    let mut out = Vec::new();
    for page in &screen.pages {
        let PageState::Rows(p) = page else {
            out.push("blob".to_owned());
            continue;
        };
        for r in &p.rows {
            let mut line = format!("{} {} {}|", p.cols, r.flags, r.used);
            let start = usize::try_from(r.cell_start).unwrap();
            for (i, c) in p.cells[start..start + usize::from(r.cell_count)]
                .iter()
                .enumerate()
            {
                let index = u32::try_from(start + i).unwrap();
                let mut extra = Vec::new();
                let mut g = 0;
                while g + 1 < p.graphemes.len() {
                    let n = usize::try_from(p.graphemes[g + 1]).unwrap();
                    if p.graphemes[g] == index {
                        extra.extend_from_slice(&p.graphemes[g + 2..g + 2 + n]);
                    }
                    g += 2 + n;
                }
                let _ = write!(
                    line,
                    "{:x}/{}/{}/{:?}/{:?}/{:?};",
                    c.content,
                    c.flags,
                    c.tab,
                    p.styles[usize::from(c.style)],
                    p.links[usize::from(c.link)],
                    extra
                );
            }
            out.push(line);
        }
    }
    out
}

/// [`exported`] without the page structure: what two terminals that took
/// the same input since an import share, whatever their allocators did.
pub type Flat = (TermState, Vec<String>, Option<Vec<String>>);

/// [`Flat`] of `t`. Compare these, and format them ([`flat_text`]) only to
/// show a difference: the pretty `Debug` of a long history is most of a
/// round trip's time in a debug build.
pub fn exported_flat(t: &PaneTerm) -> Flat {
    let mut s = exported(t);
    s.pane.times.clear();
    let primary = flat_rows(&s.primary);
    let alternate = s.alternate.as_ref().map(flat_rows);
    s.primary.pages.clear();
    if let Some(alt) = s.alternate.as_mut() {
        alt.pages.clear();
    }
    (s, primary, alternate)
}

/// A [`Flat`] as text, one field a line, to find where two differ.
pub fn flat_text((s, primary, alternate): &Flat) -> String {
    format!("{s:#?}\n{primary:#?}\n{alternate:#?}")
}

/// Bytes that exercise state no capture shows: each moves, prints or
/// reports through it.
pub const PROBE: &[u8] = b"\x1b8X\x1b[6n\tT\x1b[?6n\x1b(0q\x0eq\x0f\x1b[2;3r\x1b[H\nL\x1b[99B\nB\x1b[0c\x1b[?2004$p\x1b[?1049$p\x1b]4;1;?\x07\x1b]10;?\x07\x1b[23t\x1b[b";

/// A seeded generator of terminal input: text, controls and the escape
/// sequences tmux and Ghostty handle, cut anywhere.
pub struct Fuzz(pub u64);

impl Fuzz {
    pub fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn below(&mut self, n: usize) -> usize {
        let n = u64::try_from(n).expect("small");
        usize::try_from(self.next() % n).expect("below n")
    }

    fn pick<T: Copy>(&mut self, items: &[T]) -> T {
        items[self.below(items.len())]
    }

    fn num(&mut self) -> String {
        match self.below(4) {
            0 => String::new(),
            1 => self.below(4).to_string(),
            2 => self.below(30).to_string(),
            _ => self.below(300).to_string(),
        }
    }

    #[expect(clippy::too_many_lines, reason = "one arm per kind of input")]
    fn piece(&mut self, out: &mut Vec<u8>) {
        const TEXT: &[&str] = &[
            "hello",
            "x",
            "  ",
            "\u{4e16}\u{754c}",
            "e\u{301}",
            "\u{1f600}",
            "\u{1f1ef}\u{1f1f5}",
            "a\u{200b}b",
            "\u{1f468}\u{200d}\u{1f467}",
            "\u{fe0f}",
            "\u{0915}\u{094d}\u{0937}",
            "\u{a0}\u{e9}",
            "abcdefghijklmnopqrstuvwxyz0123456789",
        ];
        const SGR: &[&str] = &[
            "0",
            "1",
            "2",
            "3",
            "4",
            "4:3",
            "5",
            "7",
            "8",
            "9",
            "53",
            "21",
            "22",
            "24",
            "27",
            "31",
            "42",
            "95",
            "107",
            "38;5;9",
            "48;5;200",
            "38;2;1;2;3",
            "48;2;9;8;7",
            "58;5;4",
            "58:2::1:2:3",
            "39",
            "49",
            "59",
        ];
        const MODES: &[&str] = &[
            "?1", "?5", "?6", "?7", "?12", "?25", "?47", "?69", "?1000", "?1002", "?1003", "?1004",
            "?1005", "?1006", "?1047", "?1048", "?1049", "?2004", "?2026", "?2027", "?2031", "4",
            "20", "?45", "?1007",
        ];
        match self.below(40) {
            0..=9 => out.extend_from_slice(self.pick(TEXT).as_bytes()),
            10 => out.extend_from_slice(b"\r\n"),
            11 => out.push([b'\r', b'\n', 8, 9, 7, 0x0e, 0x0f, 0x0b, 0x0c][self.below(9)]),
            12 => {
                let f = b"ABCDEFGHIJKLMPSTXZ@`abdeghr"[self.below(27)];
                out.extend_from_slice(format!("\x1b[{}{}", self.num(), f as char).as_bytes());
            }
            13 => out.extend_from_slice(format!("\x1b[{};{}H", self.num(), self.num()).as_bytes()),
            14 | 15 => {
                let n = 1 + self.below(4);
                let attrs: Vec<&str> = (0..n).map(|_| self.pick(SGR)).collect();
                out.extend_from_slice(format!("\x1b[{}m", attrs.join(";")).as_bytes());
            }
            16 | 17 => {
                let m = self.pick(MODES);
                let hl = if self.below(2) == 0 { 'h' } else { 'l' };
                out.extend_from_slice(format!("\x1b[{m}{hl}").as_bytes());
            }
            18 => out.extend_from_slice(format!("\x1b[{}J", self.below(4)).as_bytes()),
            19 => out.extend_from_slice(format!("\x1b[{}K", self.below(3)).as_bytes()),
            20 => out.extend_from_slice(format!("\x1b[{};{}r", self.num(), self.num()).as_bytes()),
            21 => out.extend_from_slice(format!("\x1b[{};{}s", self.num(), self.num()).as_bytes()),
            22 => out.extend_from_slice(
                [
                    &b"\x1b7"[..],
                    b"\x1b8",
                    b"\x1b[s",
                    b"\x1b[u",
                    b"\x1bD",
                    b"\x1bE",
                    b"\x1bM",
                    b"\x1bH",
                    b"\x1b[g",
                    b"\x1b[3g",
                    b"\x1b#8",
                    b"\x1bc",
                    b"\x1b[!p",
                ][self.below(13)],
            ),
            23 => out.extend_from_slice(
                [
                    &b"\x1b(0"[..],
                    b"\x1b(B",
                    b"\x1b)0",
                    b"\x1b)B",
                    b"\x1b*0",
                    b"\x1bn",
                    b"\x1bN",
                ][self.below(7)],
            ),
            24 => out.extend_from_slice(
                format!("\x1b]{};t{}\x07", [0, 2][self.below(2)], self.below(9)).as_bytes(),
            ),
            25 => {
                if self.below(2) == 0 {
                    out.extend_from_slice(
                        format!(
                            "\x1b]8;id={};http://h/{}\x1b\\",
                            self.below(3),
                            self.below(5)
                        )
                        .as_bytes(),
                    );
                } else {
                    out.extend_from_slice(b"\x1b]8;;\x1b\\");
                }
            }
            26 => out.extend_from_slice(
                format!(
                    "\x1b]133;{}\x07",
                    ["A", "B", "C", "D;1", "P;k=s", "N"][self.below(6)]
                )
                .as_bytes(),
            ),
            27 => out.extend_from_slice(
                format!(
                    "\x1b]4;{};rgb:{:02x}/{:02x}/{:02x}\x07",
                    self.below(256),
                    self.below(256),
                    self.below(256),
                    self.below(256)
                )
                .as_bytes(),
            ),
            28 => out
                .extend_from_slice(format!("\x1b]{};rgb:1/2/3\x07", 10 + self.below(3)).as_bytes()),
            29 => out.extend_from_slice(format!("\x1b[{};0t", 22 + self.below(2)).as_bytes()),
            30 => out.extend_from_slice(format!("\x1b[{} q", self.below(7)).as_bytes()),
            31 => out.extend_from_slice(format!("\x1b[>4;{}m", self.below(3)).as_bytes()),
            32 => out.extend_from_slice(b"\x1bP1$qm\x1b\\"),
            33 => out.extend_from_slice(b"\x1bkwname\x1b\\"),
            34 => out.extend_from_slice(format!("\x1b[{}\"q", self.below(3)).as_bytes()),
            35 => out.extend_from_slice(format!("\x1b[{}X", self.num()).as_bytes()),
            36 => out.extend_from_slice(b"\x1b[3J"),
            37 => out.extend_from_slice(format!("\x1b[{}b", self.num()).as_bytes()),
            38 => out.extend_from_slice(b"\x1b]7;file://host/tmp/x\x07"),
            _ => {
                out.extend_from_slice(format!("{}\r\n", "fill ".repeat(self.below(30))).as_bytes());
            }
        }
    }

    pub fn input(&mut self, pieces: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..pieces {
            self.piece(&mut out);
        }
        out
    }
}

/// Seeds for the escape-sequence fuzzer; each run is deterministic.
pub fn fuzz_seeds() -> std::ops::Range<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    if let Some(only) = var("VTMUX_STATE_FUZZ_ONLY") {
        return only..only + 1;
    }
    1..var("VTMUX_STATE_FUZZ_SEEDS").unwrap_or(60) + 1
}

/// Inverse of `regen_corpus.py`'s escaping: `\n \r \t \\` and `\NNN`.
pub fn unescape(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        match b[i + 1] {
            b'n' => (out.push(b'\n'), i += 2),
            b'r' => (out.push(b'\r'), i += 2),
            b't' => (out.push(b'\t'), i += 2),
            b'\\' => (out.push(b'\\'), i += 2),
            _ => {
                let oct = std::str::from_utf8(&b[i + 1..i + 4]).unwrap();
                (out.push(u8::from_str_radix(oct, 8).unwrap()), i += 4)
            }
        };
    }
    out
}

/// The oracle scenarios' pane programs (`oracle_programs.txt`, recorded
/// by `regen_corpus.py`): name, pane size and the bytes written.
pub fn oracle_programs() -> Vec<(String, u16, u16, Vec<u8>)> {
    let text = include_str!("oracle_programs.txt");
    let mut lines = text.lines().filter(|l| !l.starts_with('#'));
    let mut out = Vec::new();
    while let (Some(name), Some(size), Some(bytes)) = (lines.next(), lines.next(), lines.next()) {
        let name = name.strip_prefix("name=").expect("name line");
        let (cols, rows) = size
            .strip_prefix("size=")
            .and_then(|s| s.split_once('x'))
            .expect("size line");
        out.push((
            name.to_owned(),
            cols.parse().unwrap(),
            rows.parse().unwrap(),
            unescape(bytes.strip_prefix("bytes=").expect("bytes line")),
        ));
    }
    out
}
