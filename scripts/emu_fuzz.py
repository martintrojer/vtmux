#!/usr/bin/env python3
"""Differential emulator fuzz: vtmux against tmux 3.8, pane grid only.

Usage: python3 scripts/emu_fuzz.py [--cases N] [--seed S] [--jobs J] [--minimise] [--out DIR]
       python3 scripts/emu_fuzz.py --replay FILE.json [--minimise]

Each case is a random but plausible stream of terminal output (C0, ESC, CSI, OSC, DCS;
SGR with colon forms, scroll regions, DECLRMM margins, tabs, wide and combining text,
origin, insert and wrap modes, charsets, REP, DECALN, resets) cut into chunks, with a
window resize between some chunks. A pane program (`sh -c`, tty in raw mode with no
output processing) cats one chunk, signals `wait-for`, and waits for the driver to let
it go on. After each chunk, and after each resize, the driver records
`capture-pane -p -e -S -` (and `-a -q`) plus the pane's cursor and mode formats, on
both servers. Any difference is a finding; `--minimise` cuts the stream (ddmin over
tokens, then over bytes) to the smallest one that still differs in the same place.

Servers run on private sockets under a fresh /tmp dir (`-S`, `-f /dev/null`, env
cleared) and are killed on exit. Env: EMU_FUZZ_IGNORE (formats, `capture` or `alt`: not
compared; `tail`: the SGR, SO and SI codes ending a captured line are dropped, as they
follow tmux's `cellsize`, which vtmux models (compat/DIVERGENCES.md D-027);
comma-separated), VTMUX (target/debug/vtmux), VTMUX_REFERENCE_TMUX
(~/.cache/vtmux/tmux-3.8/bin/tmux).

`--gen round2` (round 2) picks one of the focused generators per seed (seed modulo
their count; `--gen <name>` runs one): `charsets` (G0-G3 designations, SI/SO, SS2/SS3,
LS2/LS3), `decsc` (DECSC/DECRC and SCP/RCP with attributes, charsets, origin mode, a
pending wrap and links), `longline` (lines many times the width, wide and plain, with
reflowing resizes), `margins` (DECLRMM and DECSLRM, which tmux does not have, with
ICH/DCH/ECH/IL/DL, DECIC/DECDC, insert mode), `origin` (DECOM with scroll regions,
cursor moves and resizes), `osc8` (links across wraps, BEL and ST), `tabs` (HTS, TBC,
CHT, CBT, DECST8C across resizes), `widemargin` (wide characters at the right edge and
around ICH/DCH, autowrap and insert modes), `zwj` (ZWJ sequences, VS15/VS16, flags and
lone joiners at line ends).
"""
import argparse
import concurrent.futures
import json
import os
import random
import re
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VTMUX = os.environ.get("VTMUX", os.path.join(ROOT, "target/debug/vtmux"))
TMUX = os.environ.get(
    "VTMUX_REFERENCE_TMUX",
    os.path.join(os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")),
                 "vtmux/tmux-3.8/bin/tmux"),
)

FORMATS = [
    "cursor_x", "cursor_y", "cursor_flag", "insert_flag", "keypad_cursor_flag",
    "keypad_flag", "wrap_flag", "origin_flag", "scroll_region_upper",
    "scroll_region_lower", "alternate_on", "alternate_saved_x", "alternate_saved_y",
    "history_size", "bracket_paste_flag", "mouse_any_flag", "mouse_button_flag",
    "mouse_standard_flag", "mouse_all_flag", "mouse_sgr_flag", "cursor_shape",
    "cursor_blinking", "cursor_character", "pane_width", "pane_height", "pane_title",
]
FMT = "\t".join("#{%s}" % f for f in FORMATS)
IGNORE = set(filter(None, os.environ.get("EMU_FUZZ_IGNORE", "").split(",")))

SIZES = [(80, 24), (20, 6), (40, 10), (11, 4), (132, 8), (7, 3), (30, 30)]

# --- generator -------------------------------------------------------------

WIDE = [chr(c) for c in (0x4E00, 0x4E8C, 0x3042, 0xAC00, 0xFF21, 0x6F22)]
COMBINING = ["\u0301", "\u0308", "\u0323", "\u20dd"]
LATIN = ["é", "ß", "ø", "Ω", "ж", "→", "│", "─"]


def num(r, w, h):
    c = r.random()
    if c < 0.15:
        return ""
    if c < 0.25:
        return "0"
    if c < 0.55:
        return str(r.randint(1, 4))
    if c < 0.9:
        return str(r.randint(1, max(w, h) + 2))
    return str(r.randint(1, 300))


def sgr(r):
    opts = [
        "0", "1", "2", "3", "4", "5", "7", "8", "9", "21", "22", "23", "24", "25", "27",
        "28", "29", "53", "55", "39", "49", "59",
        lambda: str(r.choice([30, 31, 32, 37, 40, 44, 47, 90, 97, 100, 107])),
        lambda: "38;5;%d" % r.randint(0, 255), lambda: "48;5;%d" % r.randint(0, 255),
        lambda: "38;2;%d;%d;%d" % (r.randint(0, 255), r.randint(0, 255), r.randint(0, 255)),
        lambda: "48:2::%d:%d:%d" % (r.randint(0, 255), r.randint(0, 255), r.randint(0, 255)),
        lambda: "38:2:%d:%d:%d" % (r.randint(0, 255), r.randint(0, 255), r.randint(0, 255)),
        lambda: "38:5:%d" % r.randint(0, 255), lambda: "58:5:%d" % r.randint(0, 255),
        lambda: "58:2::%d:%d:%d" % (r.randint(0, 255), r.randint(0, 255), r.randint(0, 255)),
        lambda: "4:%d" % r.randint(0, 5),
    ]
    parts = []
    for _ in range(r.randint(1, 3)):
        o = r.choice(opts)
        parts.append(o() if callable(o) else o)
    return "\x1b[" + ";".join(parts) + "m"


def text(r):
    c = r.random()
    if c < 0.5:
        return "".join(r.choice("abcdefghijklmnopqrstuvwxyz0123456789 .-_") for _ in range(r.randint(1, 30)))
    if c < 0.65:
        return "".join(r.choice(WIDE) for _ in range(r.randint(1, 6)))
    if c < 0.75:
        return r.choice("aeoxAZ") + r.choice(COMBINING)
    if c < 0.85:
        return "".join(r.choice(LATIN) for _ in range(r.randint(1, 5)))
    if c < 0.9:
        return r.choice(WIDE) + r.choice(COMBINING)
    return "lqkxjmtuvwn" if r.random() < 0.5 else "abc"


def token(r, w, h):
    n = lambda: num(r, w, h)  # noqa: E731
    k = r.random()
    if k < 0.30:
        return text(r)
    if k < 0.40:
        return r.choice(["\r\n", "\n", "\r", "\b", "\t", "\x0b", "\x0c", "\x07", "\x0e", "\x0f", "\r\n\r\n"])
    if k < 0.50:
        return sgr(r)
    if k < 0.58:
        return r.choice([
            "\x1b7", "\x1b8", "\x1bD", "\x1bE", "\x1bM", "\x1bH", "\x1b=", "\x1b>",
            "\x1b#8", "\x1b(0", "\x1b(B", "\x1b)0", "\x1b)B", "\x1bc", "\x1b[!p",
            "\x1b[s", "\x1b[u", "\x1bN", "\x1bO", "\x1b(A", "\x1b*0", "\x1bn",
        ])
    if k < 0.80:
        f = r.choice([
            "A", "B", "C", "D", "E", "F", "G", "H", "f", "d", "e", "a", "`",
            "J", "K", "L", "M", "P", "@", "X", "S", "T", "b", "g", "I", "Z",
        ])
        if f in "Hf":
            return "\x1b[%s;%s%s" % (n(), n(), f) if r.random() < 0.7 else "\x1b[%s%s" % (n(), f)
        if f in "JK":
            return "\x1b[%s%s" % (r.choice(["", "0", "1", "2", "3"]), f)
        if f == "g":
            return "\x1b[%sg" % r.choice(["", "0", "3"])
        return "\x1b[%s%s" % (n(), f)
    if k < 0.87:
        c = r.random()
        if c < 0.5:
            top, bot = sorted((r.randint(1, h + 1), r.randint(1, h + 1)))
            return "\x1b[%d;%dr" % (top, bot) if r.random() < 0.8 else "\x1b[r"
        lft, rgt = sorted((r.randint(1, w + 1), r.randint(1, w + 1)))
        return "\x1b[%d;%ds" % (lft, rgt) if r.random() < 0.8 else "\x1b[s"
    if k < 0.95:
        m = r.choice(["?1", "?5", "?6", "?7", "?25", "?45", "?69", "?1049", "?47", "?1047",
                      "?1048", "?2004", "?1000", "?1002", "?1003", "?1006", "?12", "4", "20",
                      "?3", "?66", "?2026"])
        return "\x1b[%s%s" % (m, r.choice("hl"))
    c = r.random()
    if c < 0.3:
        return "\x1b]%d;t%d\x07" % (r.choice([0, 2]), r.randint(0, 99))
    if c < 0.55:
        return "\x1b]8;;http://x/%d\x1b\\" % r.randint(0, 9) if r.random() < 0.6 else "\x1b]8;;\x1b\\"
    if c < 0.7:
        return "\x1b[%d q" % r.randint(0, 6)
    if c < 0.8:
        return "\x1bP+q544e\x1b\\"
    if c < 0.9:
        return "\x1b[%s;%s%s" % (n(), n(), r.choice("Hr"))
    return r.choice(["\x1b[?1;2c", "\x1b[>c", "\x1b]4;1;rgb:ff/00/00\x07", "\x1b[1;5;7;9m"])


# --- round 2: focused generators -------------------------------------------
#
# Each draws from a narrow family of sequences that the mixed generator above
# reaches too rarely to exercise (a margin needs ?69h before CSI s, a tab stop
# needs HTS, a G2 set needs SS2 after its designation, ...), with filler text
# and a resize between most chunks.

ZWJ_SEQS = [
    "\U0001F468\u200d\U0001F469\u200d\U0001F467", "\u2764\ufe0f", "\U0001F44D\U0001F3FD",
    "\U0001F1EF\U0001F1F5", "x\u200d\U0001F600", "\U0001F3F3\ufe0f\u200d\U0001F308",
    "a\u200d", "\u200d", "\U0001F600\u200d", "\u2601\ufe0f", "\U0001F1EF", "e\u0301\u200d",
    "\u00e9\ufe0e", "\u0915\u094d\u0937",
]


def plain(r, lo=1, hi=12):
    return "".join(r.choice("abcdefghijklmnopqrstuvwxyz0123456789") for _ in range(r.randint(lo, hi)))


def cup(r, w, h):
    return "\x1b[%d;%dH" % (r.randint(1, h + 1), r.randint(1, w + 1))


def to_col(r, w, near_end=True):
    """CHA to a column, mostly at or near the right edge."""
    c = w - r.randint(0, 2) if near_end and r.random() < 0.7 else r.randint(1, w + 1)
    return "\x1b[%dG" % max(1, c)


def common(r, w, h):
    k = r.random()
    if k < 0.35:
        return plain(r)
    if k < 0.45:
        return r.choice(["\r\n", "\n", "\r", "\b", "\t"])
    if k < 0.6:
        return sgr(r)
    if k < 0.75:
        return cup(r, w, h)
    if k < 0.85:
        return r.choice(WIDE) * r.randint(1, 3)
    return text(r)


def g_margins(r, w, h):
    k = r.random()
    if k < 0.1:
        return "\x1b[?69%s" % ("h" if r.random() < 0.85 else "l")
    if k < 0.22:
        lft, rgt = sorted((r.randint(1, w), r.randint(1, w)))
        return "\x1b[%d;%ds" % (lft, rgt) if r.random() < 0.85 else "\x1b[s"
    if k < 0.3:
        top, bot = sorted((r.randint(1, h), r.randint(1, h)))
        return "\x1b[%d;%dr" % (top, bot)
    if k < 0.55:
        return "\x1b[%s%s" % (r.choice(["", "1", "2", "3", str(r.randint(1, w + 2))]),
                              r.choice("@PXLMST"))
    if k < 0.6:
        return "\x1b[%d '%s" % (r.randint(0, 4), r.choice("}~"))  # DECIC / DECDC
    if k < 0.65:
        return r.choice(["\x1b[4h", "\x1b[4l", "\x1b[?6h", "\x1b[?6l", "\x1b[?7l", "\x1b[?7h"])
    if k < 0.72:
        return r.choice(["\x1bD", "\x1bM", "\x1bE", "\x1b6", "\x1b9"])
    if k < 0.8:
        return to_col(r, w)
    return common(r, w, h)


def g_origin(r, w, h):
    k = r.random()
    if k < 0.12:
        return "\x1b[?6%s" % ("h" if r.random() < 0.7 else "l")
    if k < 0.27:
        top, bot = sorted((r.randint(1, h + 1), r.randint(1, h + 1)))
        return "\x1b[%d;%dr" % (top, bot) if r.random() < 0.85 else "\x1b[r"
    if k < 0.4:
        return cup(r, w, h) if r.random() < 0.7 else "\x1b[%dd" % r.randint(1, h + 2)
    if k < 0.55:
        return r.choice(["\n", "\x1bD", "\x1bM", "\x1bE", "\r\n", "\x1b[%dA" % r.randint(1, h),
                         "\x1b[%dB" % r.randint(1, h), "\x1b[%dS" % r.randint(1, 3),
                         "\x1b[%dT" % r.randint(1, 3), "\x1b[%dL" % r.randint(1, 3),
                         "\x1b[%dM" % r.randint(1, 3)])
    if k < 0.6:
        return r.choice(["\x1b7", "\x1b8", "\x1b[?1049h", "\x1b[?1049l", "\x1b[?47h", "\x1b[?47l"])
    if k < 0.65:
        return "\x1b[%dJ" % r.randint(0, 3)
    return common(r, w, h)


def g_tabs(r, w, h):
    k = r.random()
    if k < 0.2:
        return to_col(r, w, near_end=False) + "\x1bH"
    if k < 0.28:
        return "\x1b[%sg" % r.choice(["", "0", "3", "3", "2", "5"])
    if k < 0.5:
        return r.choice(["\t", "\t\t", "\x1b[%sI" % r.choice(["", "2", str(r.randint(1, 30))]),
                         "\x1b[%sZ" % r.choice(["", "2", str(r.randint(1, 30))])])
    if k < 0.58:
        return to_col(r, w)
    if k < 0.63:
        return r.choice(["\x1b[?69h", "\x1b[%d;%ds" % tuple(sorted((r.randint(1, w), r.randint(1, w)))),
                         "\x1b[?69l", "\x1b[?5W", "\x1b[W", "\x1b[2W", "\x1b[5W"])
    if k < 0.66:
        return r.choice(["\x1bc", "\x1b[!p"])
    return common(r, w, h)


def g_charsets(r, w, h):
    k = r.random()
    if k < 0.25:
        return "\x1b%s%s" % (r.choice("()*+"), r.choice("0BA0B"))
    if k < 0.45:
        return r.choice(["\x0e", "\x0f", "\x1bN", "\x1bO", "\x1bn", "\x1bo", "\x1b|", "\x1b}", "\x1b~"])
    if k < 0.7:
        return "".join(r.choice("lqkxjmtuvwn`afg~_{}|abc") for _ in range(r.randint(1, 8)))
    if k < 0.75:
        return r.choice(["\x1b7", "\x1b8", "\x1b[s", "\x1b[u", "\x1bc", "\x1b[!p"])
    if k < 0.8:
        return r.choice(LATIN) + r.choice(WIDE)
    return common(r, w, h)


def g_decsc(r, w, h):
    k = r.random()
    if k < 0.2:
        return r.choice(["\x1b7", "\x1b8", "\x1b[s", "\x1b[u", "\x1b8", "\x1b[?1048h", "\x1b[?1048l",
                         "\x1b[?1049h", "\x1b[?1049l"])
    if k < 0.35:
        return sgr(r)
    if k < 0.45:
        return "\x1b%s%s" % (r.choice("()"), r.choice("0B")) if r.random() < 0.7 else r.choice("\x0e\x0f")
    if k < 0.52:
        return r.choice(["\x1b[?6h", "\x1b[?6l", "\x1b[?7l", "\x1b[?7h",
                         "\x1b[%d;%dr" % tuple(sorted((r.randint(1, h), r.randint(1, h))))])
    if k < 0.6:
        return to_col(r, w) + plain(r, 1, 3)  # leave a wrap pending
    if k < 0.65:
        return r.choice(["\x1b]8;;http://s/%d\x1b\\" % r.randint(0, 9), "\x1b]8;;\x1b\\"])
    if k < 0.7:
        return "lqkx"
    return common(r, w, h)


def g_widemargin(r, w, h):
    k = r.random()
    if k < 0.25:
        return to_col(r, w) + r.choice(WIDE) * r.randint(1, 2)
    if k < 0.35:
        return r.choice(WIDE) + r.choice(COMBINING + ["\u200d", "\ufe0f"])
    if k < 0.45:
        return r.choice(["\x1b[?69h", "\x1b[%d;%ds" % tuple(sorted((r.randint(1, w), r.randint(1, w)))),
                         "\x1b[?69l", "\x1b[?7l", "\x1b[?7h", "\x1b[4h", "\x1b[4l"])
    if k < 0.6:
        return "\x1b[%s%s" % (r.choice(["", "1", "2", "3"]), r.choice("@PXbDC"))
    if k < 0.7:
        return "".join(r.choice(WIDE + ["a", "b"]) for _ in range(r.randint(w // 2, w + 2)))
    return common(r, w, h)


def g_zwj(r, w, h):
    k = r.random()
    if k < 0.35:
        return to_col(r, w) + r.choice(ZWJ_SEQS)
    if k < 0.55:
        return r.choice(ZWJ_SEQS)
    if k < 0.65:
        return plain(r, 1, 3) + r.choice(["\u200d", "\u0301", "\ufe0f"])
    if k < 0.7:
        return r.choice(["\x1b[?7l", "\x1b[?7h", "\x1b[4h", "\x1b[4l"])
    return common(r, w, h)


def g_osc8(r, w, h):
    k = r.random()
    if k < 0.25:
        uri = "http://h/%d" % r.randint(0, 9)
        params = r.choice(["", "id=%d" % r.randint(0, 3), "id=a:x=y"])
        st = r.choice(["\x1b\\", "\x07"])
        return "\x1b]8;%s;%s%s" % (params, uri, st)
    if k < 0.35:
        return "\x1b]8;;%s" % r.choice(["\x1b\\", "\x07"])
    if k < 0.55:
        return to_col(r, w) + plain(r, 3, 2 * w)
    if k < 0.65:
        return "".join(r.choice(WIDE) for _ in range(r.randint(1, w)))
    return common(r, w, h)


def g_longline(r, w, h):
    k = r.random()
    if k < 0.3:
        n = r.randint(w, 12 * w)
        return "".join(r.choice("abcdefghijklmnopqrstuvwxyz0123456789 ") for _ in range(n))
    if k < 0.4:
        return "".join(r.choice(WIDE + ["x"]) for _ in range(r.randint(w, 6 * w)))
    if k < 0.5:
        return sgr(r)
    if k < 0.55:
        return "\t" * r.randint(1, 4)
    if k < 0.62:
        return r.choice(["\x1b]8;;http://l/%d\x1b\\" % r.randint(0, 9), "\x1b]8;;\x1b\\"])
    if k < 0.7:
        return r.choice(["\r\n", "\x1b[K", "\x1b[1K", "\x1b[%dX" % r.randint(1, w)])
    return common(r, w, h)


GENERATORS = {
    "margins": g_margins, "origin": g_origin, "tabs": g_tabs, "charsets": g_charsets,
    "decsc": g_decsc, "widemargin": g_widemargin, "zwj": g_zwj, "osc8": g_osc8,
    "longline": g_longline,
}
# Odd and small widths put margins, tab stops and wide characters at the edge.
SIZES2 = [(80, 24), (20, 6), (21, 5), (9, 4), (40, 10), (13, 7), (132, 8), (33, 12), (5, 3)]


def gen_case(seed, gen="mixed"):
    r = random.Random(seed)
    if gen == "mixed":
        size = r.choice(SIZES)
        w, h = size
        chunks = []
        for _ in range(r.randint(1, 4)):
            toks = [token(r, w, h) for _ in range(r.randint(5, 80))]
            resize = r.choice(SIZES) if r.random() < 0.6 else None
            chunks.append({"tokens": toks, "resize": resize})
            if resize:
                w, h = resize
        return {"seed": seed, "size": list(size), "chunks": chunks}
    if gen == "round2":
        names = sorted(GENERATORS)
        gen = names[seed % len(names)]
    g = GENERATORS[gen]
    size = r.choice(SIZES2)
    w, h = size
    chunks = []
    for _ in range(r.randint(2, 5)):
        toks = [g(r, w, h) for _ in range(r.randint(5, 60))]
        resize = r.choice(SIZES2) if r.random() < 0.75 else None
        chunks.append({"tokens": toks, "resize": resize})
        if resize:
            w, h = resize
    return {"seed": seed, "gen": gen, "size": list(size), "chunks": chunks}


# --- running ---------------------------------------------------------------


def env_for(tmp):
    return {
        "HOME": tmp, "TMUX_TMPDIR": tmp, "SHELL": "/bin/sh", "TERM": "xterm-256color",
        "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
    }


class Server:
    def __init__(self, binary, tmp, name):
        self.binary = binary
        self.sock = os.path.join(tmp, name)
        self.env = env_for(tmp)

    def run(self, *args, timeout=20):
        p = subprocess.run([self.binary, "-S", self.sock, "-f", "/dev/null", *args],
                           env=self.env, capture_output=True, timeout=timeout)
        return p.returncode, p.stdout.decode("utf-8", "replace"), p.stderr.decode("utf-8", "replace")

    def kill(self):
        try:
            self.run("kill-server", timeout=5)
        except Exception:
            pass


def run_case(server, case, tmp, tag):
    """Feed the case to a fresh session; return the list of observations."""
    d = os.path.join(tmp, "d-" + tag)
    os.makedirs(d, exist_ok=True)
    for i, ch in enumerate(case["chunks"]):
        with open(os.path.join(d, "c%d" % i), "wb") as f:
            f.write("".join(ch["tokens"]).encode("utf-8"))
    for i in range(len(case["chunks"]), len(case["chunks"]) + 8):
        p = os.path.join(d, "c%d" % i)
        if os.path.exists(p):
            os.unlink(p)
    b = server.binary
    script = (
        "stty raw -echo -opost; i=0; while [ -f %s/c$i ]; do cat %s/c$i; "
        "%s wait-for -S %s-s$i; %s wait-for %s-g$i; i=$((i+1)); done; exec sleep 3600"
    ) % (d, d, b, tag, b, tag)
    w, h = case["size"]
    rc, _, err = server.run("new-session", "-d", "-s", tag, "-x", str(w), "-y", str(h),
                            "sh", "-c", script)
    if rc != 0:
        raise RuntimeError("new-session failed: " + err)
    obs = []
    try:
        for i, ch in enumerate(case["chunks"]):
            server.run("wait-for", "%s-s%d" % (tag, i), timeout=30)
            obs.append(("chunk%d" % i, observe(server, tag)))
            if ch["resize"]:
                rw, rh = ch["resize"]
                server.run("resize-window", "-t", tag, "-x", str(rw), "-y", str(rh))
                obs.append(("resize%d" % i, observe(server, tag)))
            server.run("wait-for", "-S", "%s-g%d" % (tag, i))
    finally:
        server.run("kill-session", "-t", tag)
    return obs


TAIL = re.compile(r"(?:\x1b\[[0-9;:]*m|[\x0e\x0f])+$", re.M)


def observe(server, tag):
    t = tag + ":0.0"
    _, cap, _ = server.run("capture-pane", "-p", "-e", "-S", "-", "-t", t)
    _, alt, _ = server.run("capture-pane", "-p", "-e", "-a", "-q", "-t", t)
    if "tail" in IGNORE:
        cap, alt = TAIL.sub("", cap), TAIL.sub("", alt)
    _, fmt, _ = server.run("display", "-p", "-t", t, FMT)
    fmts = dict(zip(FORMATS, fmt.rstrip("\n").split("\t")))
    return {"capture": cap, "alt": alt, "formats": fmts}


def diff_obs(a, b):
    """First difference between two observation lists: (step, field) or None."""
    for (sa, oa), (sb, ob) in zip(a, b):
        for k in ("capture", "alt"):
            if k not in IGNORE and oa[k] != ob[k]:
                return (sa, k)
        for f in FORMATS:
            if f not in IGNORE and oa["formats"].get(f) != ob["formats"].get(f):
                return (sa, f)
    if len(a) != len(b):
        return ("len", "len")
    return None


class Pair:
    def __init__(self):
        self.tmp = tempfile.mkdtemp(prefix="rvfz-", dir="/tmp")
        self.ref = Server(TMUX, self.tmp, "t")
        self.vt = Server(VTMUX, self.tmp, "v")
        self.n = 0
        for s in (self.ref, self.vt):
            s.run("start-server", ";", "set", "-g", "exit-empty", "off")
            s.run("set", "-g", "remain-on-exit", "off")

    def compare(self, case):
        self.n += 1
        tag = "c%d" % self.n
        with concurrent.futures.ThreadPoolExecutor(2) as ex:
            fa = ex.submit(run_case, self.ref, case, self.tmp, "t" + tag)
            fb = ex.submit(run_case, self.vt, case, self.tmp, "v" + tag)
            a, b = fa.result(), fb.result()
        return diff_obs(a, b), a, b

    def close(self):
        self.ref.kill()
        self.vt.kill()
        shutil.rmtree(self.tmp, ignore_errors=True)


# --- minimising ------------------------------------------------------------


def flatten(case):
    return [(ci, t) for ci, ch in enumerate(case["chunks"]) for t in ch["tokens"]]


def rebuild(case, items):
    chunks = [{"tokens": [], "resize": ch["resize"]} for ch in case["chunks"]]
    for ci, t in items:
        chunks[ci]["tokens"].append(t)
    return {"seed": case.get("seed"), "size": case["size"], "chunks": chunks}


def minimise(pair, case, want):
    def fails(c):
        d, _, _ = pair.compare(c)
        return d is not None and d[1] == want[1]

    # drop trailing chunks and resizes first
    while len(case["chunks"]) > 1:
        c = dict(case, chunks=case["chunks"][:-1])
        if not fails(c):
            break
        case = c
    for i, ch in enumerate(case["chunks"]):
        if ch["resize"]:
            c = json.loads(json.dumps(case))
            c["chunks"][i]["resize"] = None
            if fails(c):
                case = c
    items = flatten(case)
    n = 2
    while len(items) >= 2:
        size = max(1, len(items) // n)
        reduced = False
        for start in range(0, len(items), size):
            cand = items[:start] + items[start + size:]
            if cand and fails(rebuild(case, cand)):
                items, n, reduced = cand, max(n - 1, 2), True
                break
        if not reduced:
            if size == 1:
                break
            n = min(len(items), n * 2)
    # shrink each token by characters
    for idx in range(len(items)):
        ci, t = items[idx]
        changed = True
        while changed and len(t) > 1:
            changed = False
            for j in range(len(t)):
                cand_t = t[:j] + t[j + 1:]
                cand = items[:idx] + [(ci, cand_t)] + items[idx + 1:]
                if fails(rebuild(case, cand)):
                    t, changed = cand_t, True
                    items = cand
                    break
    return rebuild(case, items)


def show(case):
    out = []
    for i, ch in enumerate(case["chunks"]):
        out.append("chunk%d: %r resize=%s" % (i, "".join(ch["tokens"]), ch["resize"]))
    return "size=%s\n" % (case["size"],) + "\n".join(out)


def worker(args):
    seeds, do_min, outdir, gen = args
    pair = Pair()
    found = []
    try:
        for seed in seeds:
            case = gen_case(seed, gen)
            try:
                d, a, b = pair.compare(case)
            except Exception as e:  # noqa: BLE001
                found.append({"seed": seed, "error": repr(e)})
                continue
            if d is None:
                continue
            rec = {"seed": seed, "where": d, "case": case}
            if do_min:
                m = minimise(pair, case, d)
                d2, a2, b2 = pair.compare(m)
                rec.update(min=m, min_where=d2, ref=a2, vt=b2)
            with open(os.path.join(outdir, "seed-%d.json" % seed), "w") as f:
                json.dump(rec, f, indent=1)
            found.append(rec)
    finally:
        pair.close()
    return len(seeds), found


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cases", type=int, default=50)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--minimise", action="store_true")
    ap.add_argument("--out", default="/tmp/emu-fuzz")
    ap.add_argument("--replay")
    ap.add_argument("--gen", default="mixed",
                    choices=["mixed", "round2"] + sorted(GENERATORS),
                    help="mixed: round 1's generator; round2: the focused generators by seed")
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    if a.replay:
        rec = json.load(open(a.replay))
        case = rec.get("min") or rec["case"]
        if "chunks" not in case:
            case = rec["case"]
        pair = Pair()
        try:
            d, ra, rb = pair.compare(case)
            print(show(case))
            print("diff:", d)
            if d:
                for (s, oa), (_, ob) in zip(ra, rb):
                    if oa != ob:
                        print("--", s)
                        for k in ("capture", "alt"):
                            if oa[k] != ob[k]:
                                print("tmux  %s:\n%s" % (k, oa[k]))
                                print("vtmux %s:\n%s" % (k, ob[k]))
                        for f in FORMATS:
                            if oa["formats"].get(f) != ob["formats"].get(f):
                                print("  %s: tmux=%r vtmux=%r" % (f, oa["formats"].get(f), ob["formats"].get(f)))
                        break
            if d and a.minimise:
                m = minimise(pair, case, d)
                print("minimised:\n" + show(m))
                rec["min"] = m
                json.dump(rec, open(a.replay, "w"), indent=1)
        finally:
            pair.close()
        return
    seeds = list(range(a.seed, a.seed + a.cases))
    groups = [seeds[i::a.jobs] for i in range(a.jobs)]
    total, found = 0, []
    t0 = time.time()
    with concurrent.futures.ProcessPoolExecutor(a.jobs) as ex:
        for n, f in ex.map(worker, [(g, a.minimise, a.out, a.gen) for g in groups]):
            total += n
            found += f
    print("cases %d, differing %d, %.0fs" % (total, len(found), time.time() - t0))
    for rec in sorted(found, key=lambda r: r["seed"]):
        print(rec["seed"], rec.get("case", {}).get("gen", ""), rec.get("where"), rec.get("min_where"),
              rec.get("error", ""))


if __name__ == "__main__":
    sys.exit(main())
