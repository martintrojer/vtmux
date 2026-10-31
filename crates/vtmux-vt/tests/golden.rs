//! Byte-for-byte goldens recorded from the reference tmux 3.8.
//!
//! Each `tests/golden/*.case` stores its inputs, the command that produced
//! `expected`, and the expected bytes. Regenerate with
//! `python3 crates/vtmux-vt/tests/golden/regen.py` (needs the reference tmux,
//! see AGENTS.md). Never edit `expected=` by hand.

use std::fs;
use std::path::Path;

use vtmux_vt::{CaptureOpts, LineRef, PaneTerm, capture};

struct Case {
    kind: String,
    cols: u16,
    rows: u16,
    bytes: Vec<u8>,
    flags: String,
    expected: Vec<u8>,
    /// `seq=N`: the pane first prints `l0..l<N-1>`, one per line.
    seq: usize,
    /// `limit=N`: `history-limit`; tmux's default is 2000.
    limit: usize,
    /// `resize=COLSxROWS`: the pane is resized after its bytes are written.
    resize: Option<(u16, u16)>,
    /// `format=...`: the `display-message -p` format (format kind).
    format: String,
}

/// Inverse of the escaping in `regen.py`: `\n \r \t \\` and `\NNN` octal.
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
            d if d.is_ascii_digit() => {
                let oct = std::str::from_utf8(&b[i + 1..i + 4]).unwrap();
                (out.push(u8::from_str_radix(oct, 8).unwrap()), i += 4)
            }
            other => panic!("bad escape \\{}", other as char),
        };
    }
    out
}

fn load(name: &str) -> Case {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.case"));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let get = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(&format!("{key}=")))
            .map(str::to_owned)
    };
    assert!(get("command").is_some(), "{name}: no recorded command");
    Case {
        kind: get("kind").unwrap(),
        cols: get("cols").map_or(20, |v| v.parse().unwrap()),
        rows: get("rows").map_or(5, |v| v.parse().unwrap()),
        bytes: unescape(&get("bytes").unwrap()),
        flags: get("flags").unwrap_or_default(),
        expected: unescape(&get("expected").unwrap()),
        seq: get("seq").map_or(0, |v| v.parse().unwrap()),
        limit: get("limit").map_or(2000, |v| v.parse().unwrap()),
        resize: get("resize").map(|v| {
            let (w, h) = v.split_once('x').unwrap();
            (w.parse().unwrap(), h.parse().unwrap())
        }),
        format: get("format").unwrap_or_default(),
    }
}

/// The value after `-S` / `-E` in a capture-pane flag string, if any.
fn line_ref(flags: &str, flag: &str, default: LineRef, dash: LineRef) -> LineRef {
    let mut it = flags.split_whitespace();
    while let Some(w) = it.next() {
        if w == flag {
            let v = it.next().expect("flag value");
            return if v == "-" {
                dash
            } else {
                LineRef::Line(v.parse().unwrap())
            };
        }
    }
    default
}

fn run(name: &str) {
    let c = load(name);
    let mut term = PaneTerm::new(c.cols, c.rows, c.limit);
    // The reference pane sits behind a pty with ONLCR, so a program's `\n`
    // reaches the terminal as `\r\n`.
    let mut written = (0..c.seq)
        .flat_map(|i| format!("l{i}\n").into_bytes())
        .collect::<Vec<u8>>();
    written.extend_from_slice(&c.bytes);
    for &b in &written {
        if b == b'\n' {
            term.feed(b"\r");
        }
        term.feed(&[b]);
    }
    if let Some((w, h)) = c.resize {
        term.resize(w, h);
    }
    let got = if c.kind == "reply" {
        term.take_replies()
    } else if c.kind == "format" {
        let (x, y) = term.cursor();
        let v = match c.format.as_str() {
            "#{history_size}" => term.history_size(),
            "#{cursor_x}" => usize::from(x),
            "#{cursor_y}" => usize::from(y),
            f => panic!("{name}: unsupported format {f}"),
        };
        format!("{v}\n").into_bytes()
    } else {
        capture(
            &term,
            CaptureOpts {
                escapes: c
                    .flags
                    .split_whitespace()
                    .next()
                    .is_some_and(|f| f.contains('e')),
                join: c
                    .flags
                    .split_whitespace()
                    .next()
                    .is_some_and(|f| f.contains('J')),
                start: line_ref(&c.flags, "-S", LineRef::Visible, LineRef::Start),
                end: line_ref(&c.flags, "-E", LineRef::Visible, LineRef::End),
            },
        )
    };
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&c.expected),
        "{name}: got {got:?}, tmux 3.8 gave {:?}",
        c.expected
    );
}

macro_rules! golden {
    ($($(#[$m:meta])* $name:ident),* $(,)?) => {
        $( $(#[$m])* #[test] fn $name() { run(stringify!($name)); } )*
    };
}

golden! {
    cap_plain,
    cap_trailing_spaces,
    cap_bold,
    cap_fg_bg_256,
    cap_rgb,
    cap_wrap20,
    cap_crlf,
    cap_cjk,
    cap_join_trailing,
    cap_join_escapes,
    cap_erase_bg,
    cap_ansi16,
    cap_underline_curly,
    #[ignore = "D-pending: tmux 3.8 keeps a tab as a tab cell (GRID_FLAG_TAB) and captures `\\t`; Ghostty's grid stores spaces"]
    cap_tab,
    cap_trailing_default_after_colour,
    cap_sgr_run,
    cap_sgr_unwritten_gap,
    cap_sgr_style_runs,
    cap_scroll_history,
    cap_history_sawtooth_20x5,
    cap_history_sawtooth_range,
    fmt_history_size_sawtooth,
    fmt_history_size_sawtooth_80x24,
    cap_alt_history,
    fmt_alt_history_size,
    cap_cluster_truncated,
    #[ignore = "D-pending: tmux moves lines scrolled out of any scroll region into history (grid-view.c:107-117); Ghostty only for a region starting at row 0"]
    cap_region_scroll_history,
    #[ignore = "D-pending: tmux moves lines scrolled out of any scroll region into history (grid-view.c:107-117); Ghostty only for a region starting at row 0"]
    fmt_region_scroll_history_size,
    #[ignore = "D-pending: tmux reflow on narrowing pushes the top row into history; Ghostty keeps both rows on screen"]
    cap_reflow_narrow,
    #[ignore = "D-pending: tmux reflow on narrowing pushes the top row into history; Ghostty keeps both rows on screen"]
    fmt_reflow_narrow_history_size,
    #[ignore = "D-pending: tmux reflow on narrowing pushes the top row into history; Ghostty keeps both rows on screen"]
    fmt_reflow_narrow_cursor_y,
    #[ignore = "D-pending: tmux clusters ZWJ sequences (cursor_x 4 for x+family+y); Ghostty without mode 2027 does not, like xterm"]
    fmt_zwj_cursor_x,
    cap_ed3_alt,
    fmt_ed3_alt,
    cap_ed3_alt_exit,
    fmt_ed3_alt_exit,
    #[ignore = "D-pending: P6 tmux scroll-on-clear moves the screen into history on ED2 and ED0 at 0,0 (screen-write.c:1936-1942, 2094); Ghostty just clears"]
    cap_scroll_on_clear_ed2,
    #[ignore = "D-pending: P6 tmux scroll-on-clear moves the screen into history on ED2 and ED0 at 0,0 (screen-write.c:1936-1942, 2094); Ghostty just clears"]
    fmt_scroll_on_clear_ed2,
    #[ignore = "D-pending: P6 tmux scroll-on-clear moves the screen into history on ED2 and ED0 at 0,0 (screen-write.c:1936-1942, 2094); Ghostty just clears"]
    cap_scroll_on_clear_ctrl_l,
    #[ignore = "D-pending: P6 tmux scroll-on-clear moves the screen into history on ED2 and ED0 at 0,0 (screen-write.c:1936-1942, 2094); Ghostty just clears"]
    fmt_scroll_on_clear_ctrl_l,
    #[ignore = "D-pending: P7 tmux RIS keeps history (screen.c:109-130 screen_reinit); Ghostty full reset drops scrollback"]
    cap_ris,
    #[ignore = "D-pending: P7 tmux RIS keeps history (screen.c:109-130 screen_reinit); Ghostty full reset drops scrollback"]
    fmt_ris,
    #[ignore = "D-pending: P7 tmux RIS keeps history (screen.c:109-130 screen_reinit); Ghostty full reset drops scrollback"]
    cap_ris_alt,
    #[ignore = "D-pending: P7 tmux RIS keeps history (screen.c:109-130 screen_reinit); Ghostty full reset drops scrollback"]
    fmt_ris_alt,
    reply_da1,
    reply_da2,
    reply_cpr,
    reply_cpr_origin,
    reply_cpr_pending_wrap,
    reply_dsr5,
    reply_xtversion,
    reply_xtwinops_18,
    reply_decrqm_wrap,
    reply_decrqm_alt,
    reply_decrqm_irm,
    reply_decrqm_ansi_unknown,
}
