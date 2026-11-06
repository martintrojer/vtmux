//! `PaneTerm::export_state` / `import_state` round trips (live upgrade
//! spec, decisions 5 and 24): a pane imported from an export is the same
//! terminal. Every capture (`capture-pane -e -S -` and the other flags),
//! the cursor, the modes, every row's flags and the whole re-exported
//! state are equal, and feeding both the same input afterwards keeps them
//! equal and gives the same replies, which proves the state no capture
//! shows (saved cursor, charsets, tab stops, scroll region, modes,
//! parser state) came across too.

use vtmux_vt::{
    CaptureOpts, CompressedPages, Compression, LineRef, PageState, PaneTerm, StateError, TermState,
    capture, capture_alternate,
};

/// Everything a script or the compositor can read from a pane.
/// `times`: with each history row's time, which differs between two panes
/// fed the same input in different seconds.
#[expect(clippy::too_many_lines, reason = "one line per thing observed")]
fn observe(t: &PaneTerm, times: bool) -> String {
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
        "marks {:?} osc133 {:?} pending {:?} sync {}",
        t.line_marks(),
        t.osc133_lines(),
        t.pending_input(),
        t.sync_held().is_some(),
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
fn exported(t: &PaneTerm) -> TermState {
    let mut s = t.export_state();
    if let Some((_, left)) = s.pane.sync.as_mut() {
        *left = std::time::Duration::ZERO;
    }
    s
}

/// A screen's rows, each with its flags and cells, styles and links
/// resolved: the same for any split of the rows into pages and any page
/// capacity.
fn flat_rows(screen: &vtmux_vt::ScreenState) -> Vec<String> {
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
fn exported_flat(t: &PaneTerm) -> String {
    let mut s = exported(t);
    s.pane.times.clear();
    let primary = flat_rows(&s.primary);
    let alternate = s.alternate.as_ref().map(flat_rows);
    s.primary.pages.clear();
    if let Some(alt) = s.alternate.as_mut() {
        alt.pages.clear();
    }
    format!("{s:#?}\n{primary:#?}\n{alternate:#?}")
}

fn assert_same(a: &PaneTerm, b: &PaneTerm, ctx: &str) {
    assert_alike(a, b, ctx, true);
}

/// `exact`: the re-exported states are equal, pages and capacities too;
/// else only their rows are.
fn assert_alike(a: &PaneTerm, b: &PaneTerm, ctx: &str, exact: bool) {
    let (oa, ob) = (observe(a, exact), observe(b, exact));
    if oa != ob {
        let line = oa
            .lines()
            .zip(ob.lines())
            .position(|(x, y)| x != y)
            .unwrap_or(0);
        panic!(
            "{ctx}: observations differ at line {line}\n--- source\n{}\n--- import\n{}",
            oa.lines().nth(line).unwrap_or(""),
            ob.lines().nth(line).unwrap_or("")
        );
    }
    let (da, db) = if exact {
        let (sa, sb) = (exported(a), exported(b));
        if sa == sb {
            return;
        }
        (format!("{sa:#?}"), format!("{sb:#?}"))
    } else {
        (exported_flat(a), exported_flat(b))
    };
    if da != db {
        let line = da
            .lines()
            .zip(db.lines())
            .position(|(x, y)| x != y)
            .unwrap_or(0);
        let context = |d: &str| {
            d.lines()
                .skip(line.saturating_sub(12))
                .take(16)
                .collect::<Vec<_>>()
                .join("\n")
        };
        panic!(
            "{ctx}: re-exported states differ at line {line}\n--- source\n{}\n--- import\n{}",
            context(&da),
            context(&db)
        );
    }
}

/// Export `a`, import it, check both alike, then feed `rest` to both and
/// check again, replies included.
fn round_trip(a: &mut PaneTerm, compressed: CompressedPages, rest: &[u8], ctx: &str) -> PaneTerm {
    let _ = a.take_answers();
    let state = a.export_state_with(compressed);
    let mut b = PaneTerm::import_state(&state).unwrap_or_else(|e| panic!("{ctx}: import: {e}"));
    assert_same(a, &b, ctx);
    a.feed(rest);
    b.feed(rest);
    assert_eq!(
        format!("{:?}", a.take_answers()),
        format!("{:?}", b.take_answers()),
        "{ctx}: replies after import"
    );
    assert_alike(a, &b, &format!("{ctx} (after more input)"), false);
    b
}

fn pane(cols: u16, rows: u16, limit: usize, bytes: &[u8]) -> PaneTerm {
    let mut t = PaneTerm::new(cols, rows, limit);
    t.feed(bytes);
    t
}

/// Bytes that exercise state no capture shows: each moves, prints or
/// reports through it.
const PROBE: &[u8] = b"\x1b8X\x1b[6n\tT\x1b[?6n\x1b(0q\x0eq\x0f\x1b[2;3r\x1b[H\nL\x1b[99B\nB\x1b[0c\x1b[?2004$p\x1b[?1049$p\x1b]4;1;?\x07\x1b]10;?\x07\x1b[23t\x1b[b";

#[test]
fn plain_text_and_history() {
    let mut text = Vec::new();
    for i in 0..300 {
        text.extend_from_slice(format!("line {i} {}\r\n", "x".repeat(i % 50)).as_bytes());
    }
    let mut a = pane(40, 10, 100, &text);
    round_trip(&mut a, CompressedPages::Rows, PROBE, "plain");
}

#[test]
fn styles_wide_graphemes_links_and_flags() {
    let bytes = "a\x1b[1;3;4:3;9;53;38;5;3;48;2;1;2;3;58;5;200mb\x1b[0m\u{4e16}\u{754c}e\u{301}\
        \u{1f468}\u{200d}\u{1f469}\x1b]8;id=k;http://x\x1b\\L\x1b]8;;\x1b\\\x1b]8;;http://y\x07M\x1b]8;;\x07\
        \x1b[38;5;9mP\x1b[48;5;12m\x1b[K\r\n0123456789012345678901234567\u{4e16}tail\r\n\tA\tB\x1b[2;5H\x1b[X\x1b[3@\x1b[2P\r\n\
        \x1b]133;A\x07$ \x1b]133;B\x07cmd\x1b]133;C\x07\r\nout\r\n\x1b]133;D;2\x07\x1b(0lqk\x1b(B";
    let mut a = pane(30, 6, 50, bytes.as_bytes());
    round_trip(&mut a, CompressedPages::Rows, PROBE, "styles");
}

#[test]
fn cursor_modes_charsets_tabs_and_region() {
    let bytes = b"\x1b[?1h\x1b=\x1b[?1000h\x1b[?1006h\x1b[?2004h\x1b[?1004h\x1b[4h\x1b[20h\x1b[?7l\
        \x1b[?7h\x1b[?25l\x1b[3 q\x1b[?12h\x1b[3g\x1b[1;5H\x1bH\x1b[1;13H\x1bH\x1b[3;7r\x1b[?6h\
        \x1b)0\x0e\x1b[2;4Hab\x1b7\x1b[5;20Hzz\x1b[?1s\x1b[?1l\x1b[22t\x1b]2;one\x07\x1b[22t\
        \x1b]2;two\x07\x1b]4;1;rgb:10/20/30\x07\x1b]10;rgb:ff/00/00\x07\x1b[>4;1m\x1b[?2026h\
        0123456789012345678901234567890123456789";
    let mut a = pane(40, 8, 20, bytes);
    round_trip(&mut a, CompressedPages::Rows, PROBE, "modes");
}

#[test]
fn pending_wrap_and_saved_cursor() {
    let mut a = pane(10, 3, 10, b"abcdefghij\x1b7\r\nxyz\x1b[1;10Hq");
    round_trip(&mut a, CompressedPages::Rows, b"!\x1b8!", "pending wrap");
}

#[test]
fn alternate_screen_keeps_primary_history() {
    let mut bytes = Vec::new();
    for i in 0..60 {
        bytes.extend_from_slice(format!("p{i}\r\n").as_bytes());
    }
    bytes.extend_from_slice(b"\x1b]133;A\x07$ \x1b[?1049h\x1b[2J\x1b[Halt screen\x1b[5;5Hthere");
    let mut a = pane(20, 6, 30, &bytes);
    assert!(a.alternate_on());
    round_trip(
        &mut a,
        CompressedPages::Rows,
        b"more\x1b[?1049lback\r\n",
        "alternate",
    );
}

#[test]
fn half_parsed_sequences_carry_on() {
    for tail in [
        &b"\x1b["[..],
        b"\x1b[38;5",
        b"\x1b]2;half title",
        b"\x1bP1$q",
        b"\x1bPtmux;\x1b\x1b]2;x",
        b"\x1bkname so far",
        b"\xe4\xb8",
        b"\x1b]8;;http://half",
        b"\x1b]133;D;",
    ] {
        let mut a = pane(20, 4, 10, b"start ");
        a.feed(tail);
        round_trip(
            &mut a,
            CompressedPages::Rows,
            b"m\x07\x1b\\ end",
            &format!("tail {tail:?}"),
        );
    }
}

#[test]
fn reflow_resize_and_limits() {
    let mut text = Vec::new();
    for i in 0..500 {
        text.extend_from_slice(format!("{i:04} {}\r\n", "wrapme ".repeat(i % 7)).as_bytes());
    }
    let mut a = pane(50, 10, 200, &text);
    a.resize(23, 7);
    a.set_history_limit(150);
    a.set_history_budget(1 << 20);
    round_trip(&mut a, CompressedPages::Rows, b"after resize\r\n", "resize");
}

/// Compressed history: as rows (any build) and as blobs (decision 23,
/// same build), the import is the same terminal and its pages stay
/// compressed.
#[test]
fn compressed_history_as_rows_and_blobs() {
    let mut text = Vec::new();
    for i in 0..5000 {
        text.extend_from_slice(
            format!(
                "\x1b[3{}m{i:05}\x1b[m history {}\r\n",
                i % 8,
                "z".repeat(i % 60)
            )
            .as_bytes(),
        );
    }
    let mut a = pane(80, 24, 4000, &text);
    while a.compress_step() == Compression::Pending {}
    // Ghostty reclaims compressed pages' memory on Linux and macOS.
    let reclaims = cfg!(any(target_os = "linux", target_os = "macos"));
    assert!(a.compressed_pages() > 0 || !reclaims, "nothing compressed");
    if a.compressed_pages() == 0 {
        return;
    }
    let state = a.export_state_with(CompressedPages::Blob);
    let blobs = state
        .primary
        .pages
        .iter()
        .filter(|p| matches!(p, PageState::Blob(_)))
        .count();
    assert_eq!(u64::try_from(blobs).unwrap(), a.compressed_pages());
    let b = PaneTerm::import_state(&state).expect("import blobs");
    assert_eq!(
        b.compressed_pages(),
        a.compressed_pages(),
        "blobs stay compressed"
    );
    assert_same(&a, &b, "blobs");
    let mut a2 = pane(80, 24, 4000, &text);
    while a2.compress_step() == Compression::Pending {}
    round_trip(&mut a2, CompressedPages::Blob, PROBE, "blobs, more input");
    round_trip(&mut a2, CompressedPages::Rows, PROBE, "compressed as rows");
}

#[test]
fn foreign_blobs_are_refused() {
    let mut text = Vec::new();
    for i in 0..3000 {
        text.extend_from_slice(format!("{i} {}\r\n", "q".repeat(70)).as_bytes());
    }
    let mut a = pane(80, 24, 3000, &text);
    while a.compress_step() == Compression::Pending {}
    let mut state = a.export_state_with(CompressedPages::Blob);
    let mut changed = false;
    for p in &mut state.primary.pages {
        if let PageState::Blob(b) = p {
            b.stack = "another build".into();
            changed = true;
        }
    }
    if changed {
        assert_eq!(
            PaneTerm::import_state(&state).err(),
            Some(StateError::ForeignBlob)
        );
    }
}

/// A seeded generator of terminal input: text, controls and the escape
/// sequences tmux and Ghostty handle, cut anywhere.
struct Fuzz(u64);

impl Fuzz {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, n: usize) -> usize {
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

    fn input(&mut self, pieces: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..pieces {
            self.piece(&mut out);
        }
        out
    }
}

/// Seeds for the escape-sequence fuzzer; each run is deterministic.
fn fuzz_seeds() -> std::ops::Range<u64> {
    let var = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u64>().ok());
    if let Some(only) = var("VTMUX_STATE_FUZZ_ONLY") {
        return only..only + 1;
    }
    1..var("VTMUX_STATE_FUZZ_SEEDS").unwrap_or(60) + 1
}

#[test]
fn fuzzed_escape_sequences_round_trip() {
    for seed in fuzz_seeds() {
        let mut f = Fuzz(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let cols = 8 + u16::try_from(f.below(60)).unwrap();
        let rows = 2 + u16::try_from(f.below(20)).unwrap();
        let limit = [0, 5, 50, 300][f.below(4)];
        let n = 30 + f.below(300);
        let head = f.input(n);
        let n = 20 + f.below(100);
        let mut tail = f.input(n);
        tail.extend_from_slice(PROBE);
        // Cut the head anywhere, mid-sequence too.
        let cut = f.below(head.len() + 1);
        let mut a = PaneTerm::new(cols, rows, limit);
        a.feed(&head[..cut]);
        // A resize writes its own sequences to Ghostty, which would break
        // a half-parsed one (a resize mid-sequence desyncs Ghostty's parser
        // from tmux's in `PaneTerm` itself, import or not).
        if f.below(4) == 0 && a.pending_input().is_empty() {
            a.resize(
                8 + u16::try_from(f.below(60)).unwrap(),
                2 + u16::try_from(f.below(20)).unwrap(),
            );
        }
        let mut rest = head[cut..].to_vec();
        rest.extend_from_slice(&tail);
        round_trip(
            &mut a,
            CompressedPages::Rows,
            &rest,
            &format!("seed {seed} {cols}x{rows} limit {limit} cut {cut}"),
        );
    }
}

/// Inverse of `regen_corpus.py`'s escaping: `\n \r \t \\` and `\NNN`.
fn unescape(s: &str) -> Vec<u8> {
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

/// What the oracle scenarios' pane programs wrote to their terminals
/// (`tests/state/oracle_programs.txt`, recorded by `regen_corpus.py`),
/// each cut at several points: the import at each cut is the same
/// terminal and stays so through the rest of the program's output. At
/// the end, the pane's history compressed, as blobs too.
#[test]
fn oracle_pane_programs_round_trip() {
    let text = include_str!("state/oracle_programs.txt");
    let mut lines = text.lines().filter(|l| !l.starts_with('#'));
    let mut programs = 0;
    while let (Some(name), Some(size), Some(bytes)) = (lines.next(), lines.next(), lines.next()) {
        let name = name.strip_prefix("name=").expect("name line");
        let (cols, rows) = size
            .strip_prefix("size=")
            .and_then(|s| s.split_once('x'))
            .expect("size line");
        let (cols, rows): (u16, u16) = (cols.parse().unwrap(), rows.parse().unwrap());
        let bytes = unescape(bytes.strip_prefix("bytes=").expect("bytes line"));
        programs += 1;
        for k in 1..=4 {
            let cut = bytes.len() * k / 4;
            let mut a = PaneTerm::new(cols, rows, 2000);
            a.feed(&bytes[..cut]);
            let mut rest = bytes[cut..].to_vec();
            rest.extend_from_slice(PROBE);
            round_trip(
                &mut a,
                CompressedPages::Rows,
                &rest,
                &format!("{name} cut {cut}/{}", bytes.len()),
            );
        }
        let mut a = PaneTerm::new(cols, rows, 2000);
        a.feed(&bytes);
        while a.compress_step() == Compression::Pending {}
        round_trip(
            &mut a,
            CompressedPages::Blob,
            PROBE,
            &format!("{name} compressed"),
        );
    }
    assert!(programs > 200, "corpus has {programs} programs");
}
