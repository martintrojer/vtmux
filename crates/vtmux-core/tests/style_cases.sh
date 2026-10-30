#!/usr/bin/env bash
# Regenerates every `// BEGIN GENERATED <name>` ... `// END GENERATED <name>` region in
#   crates/vtmux-core/src/{colour,style,styled,width}.rs
# from the reference tmux 3.8. Usage (from anywhere; names pick parts, default all):
#   crates/vtmux-core/tests/style_cases.sh [colour] [style] [width] [cluster] [owner] [styled]
# Needs the reference tmux ($VTMUX_REFERENCE_TMUX, default
# ~/.cache/vtmux/tmux-3.8/bin/tmux; its source tree next to it as tmux-3.8-src, or
# $VTMUX_REFERENCE_SRC), python3, gcc, and the en_US.UTF-8 locale (utf8.c asks for it
# and glibc's wcwidth answers for it).
#
# Every expected value comes from the reference, isolated: env -u TMUX -u TMUX_PANE
# -u EDITOR -u VISUAL, SHELL=/bin/sh, explicit -L, -f /dev/null. What is asked:
#
#   colour_forms   set -g status-fg X ; show -gv status-fg      (colour_fromstring,
#                  colour_tostring; options.c:616 `bad colour: X`); every 7th also through
#                  set -g pane-colours[0] X
#   colour_rgb     set -gF @out '#{c:X}' ; show -gv @out        (colour_force_rgb, 256 palette,
#                  grey0..100)
#   x11            the colour.c table; each name is checked with #{c:NAME} first
#   style_cases    set -g status-style 'S'   -> exit code + stderr   (options.c:1202)
#                  and, for accepted S, a `-vv` server drawing '#[S]x' -> the
#                  `format_draw: style 'S' -> 'T'` log line (T is style_tostring)
#                  set -g display-panes-colour X -> `invalid colour: X` (options.c:1207)
#   width_*        (`#{w:X}` reads X as a variable name: texts go in @w options) a C program calling glibc wcwidth on every code point (the reference is not
#                  linked with utf8proc), the utf8.c / utf8-combined.c tables, and #{w:X}
#                  for the edge of every range (checked, not trusted)
#   format_width   set -g @w S ; set -gF @out '#{w:@w}' ; show -gv @out, S with #[...] and ##
#   cluster_cases  a reference pane printing TEXT: its #{cursor_x} and capture-pane row (what
#                  screen_write_combine makes of ZWJ, VS16, skin tones, flags, jamo, marks)
#   owner_styles   every #[...] of the owner's two configs ($VTMUX_OWNER_CONFS, colon-separated,
#                  to override): set -g status-style S must be accepted
#   styled_cases   a client attached under a PTY at COLS x 5 with status-format[0] = FMT: the
#                  bytes it writes (the Rust test replays them through vtmux_vt::PaneTerm),
#                  `display -p FMT` (what format_draw is given), and for range cases the
#                  `#{mouse_status_range}` (+ window/pane/session) under a click on every
#                  column of the status row
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export SRC="$HERE/../src"
REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
export REF
export TMUX_SRC="${VTMUX_REFERENCE_SRC:-$(cd "$(dirname "$REF")/../.." && pwd)/tmux-3.8-src}"
exec python3 - "$@" <<'PY'
import concurrent.futures as cf
import fcntl
import glob
import os
import pty
import re
import select
import struct
import subprocess
import sys
import tempfile
import termios
import time

REF = os.environ["REF"]
SRC = os.environ["SRC"]
TMUX_SRC = os.environ["TMUX_SRC"]
ENV = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "EDITOR", "VISUAL")}
ENV.update(SHELL="/bin/sh", LC_ALL="en_US.UTF-8", TERM="xterm-256color")
UNIQ = f"vtmux-style-{os.getpid()}"
_n = [0]


def sock():
    _n[0] += 1
    return f"{UNIQ}-{_n[0]}"


def tmux(sk, *args, extra=(), cwd=None):
    return subprocess.run([REF, "-u", *extra, "-L", sk, "-f", "/dev/null", *args], env=ENV,
                          capture_output=True, text=True, cwd=cwd)


class Server:
    def __init__(self, cols=80, rows=5, cwd=None, extra=()):
        self.sk = sock()
        self.cwd = cwd
        r = tmux(self.sk, "new-session", "-d", "-x", str(cols), "-y", str(rows), "cat",
                 extra=extra, cwd=cwd)
        assert r.returncode == 0, r.stderr

    def __call__(self, *args):
        return tmux(self.sk, *args, cwd=self.cwd)

    def out(self, *args):
        r = self(*args)
        assert r.returncode == 0, (args, r.stderr)
        return r.stdout

    def widths(self, texts):
        """`#{w:@vN}` for each text: #{w:X} looks X up as a variable, so the text goes in an
        option. Batched with `;` command separators."""
        out = []
        for i in range(0, len(texts), 100):
            batch = texts[i:i + 100]
            args = []
            for j, tx in enumerate(batch):
                if j:
                    args.append(";")
                args += ["set", "-g", "@w%d" % j, tx]
            self.out(*args)
            vals = self.expand("|".join("#{w:@w%d}" % j for j in range(len(batch)))).split("|")
            assert len(vals) == len(batch)
            out += [int(v) for v in vals]
        return out

    def expand(self, fmt):
        """The plain expansion path (format_expand), as format_cases.sh does."""
        self.out("set", "-gF", "@out", fmt)
        s = self.out("show", "-gv", "@out")
        return s[:-1] if s.endswith("\n") else s

    def close(self):
        self("kill-server")

    def __enter__(self):
        return self

    def __exit__(self, *a):
        self.close()


def inject(path, name, body):
    p = os.path.join(SRC, path)
    s = open(p).read()
    b, e = f"// BEGIN GENERATED {name}\n", f"// END GENERATED {name}\n"
    i = s.index(b)
    ind = s[s.rindex("\n", 0, i) + 1:i]  # keep the marker lines' indentation
    j = s.index(e, i)
    s = s[:i] + b + body + ind + s[j:]
    open(p, "w").write(s)


def hx(n):
    """A Rust hex literal with underscores where clippy wants them (groups of four)."""
    h = "%X" % n
    if len(h) <= 4:
        return "0x" + h
    h = h.zfill((len(h) + 3) // 4 * 4)
    return "0x" + "_".join(h[i:i + 4] for i in range(0, len(h), 4))


def rs(s):
    """A Rust string literal: printable ASCII as is, everything else \\u{..}."""
    out = []
    for ch in s:
        o = ord(ch)
        if ch == '"':
            out.append('\\"')
        elif ch == "\\":
            out.append("\\\\")
        elif 0x20 <= o < 0x7F:
            out.append(ch)
        else:
            out.append("\\u{%x}" % o)
    return '"' + "".join(out) + '"'


# ----------------------------------------------------------------------------- colours

COLOUR_INPUTS = [
    # names and case
    "default", "Default", "terminal", "TERMINAL", "black", "Red", "GREEN", "yellow", "Blue",
    "magenta", "cyan", "white",
    # bright forms and numbers
    "brightblack", "brightRED", "brightgreen", "brightyellow", "brightblue", "brightmagenta",
    "brightcyan", "brightwhite", "90", "91", "92", "93", "94", "95", "96", "97",
    "0", "1", "2", "3", "4", "5", "6", "7",
    # numbers that are NOT colours
    "8", "9", "10", "15", "16", "89", "98", "255", "256", "-1", "00", "01", "091", "0x1", " 1", "1 ",
    # colourN / colorN
    "colour0", "colour5", "COLOUR5", "Colour255", "color17", "COLOR5", "colour256", "colour", "color",
    "colour-0", "colour+5", "colour 5", "colour5x", "colour05", "colour1000",
    # rgb
    "#FFaa00", "#ffaa00", "#000000", "#fff", "#12345", "#1234567", "#ggg000", "#12345g", "# 12345",
    # grey
    "grey", "gray", "GREY", "grey0", "grey50", "gray100", "grey33", "grey 5", "grey-1", "grey101",
    "greyx", "grey1.5", "grey05",
    # themes
    "themeblack", "themewhite", "themelightgrey", "themedarkgrey", "themegreen", "themeyellow",
    "themered", "themeblue", "themecyan", "thememagenta", "THEMEGREEN", "theme", "themepink",
    # X11
    "alice blue", "AliceBlue", "aliceblue", "ALICEBLUE", "antique white", "DarkOliveGreen2",
    "dark olive green", "medium spring green", "YellowGreen", "light goldenrod yellow",
    "navy blue", "NavyBlue", "violet red", "WhiteSmoke", "white smoke", "Snow4", "snow 4",
    # junk
    "", " ", "red ", " red", "nope", "red,blue", "none", "-", "colour red",
]


def gen_colour():
    forms = []
    with Server() as t:
        for c in COLOUR_INPUTS:
            r = t("set", "-g", "status-fg", c)
            if r.returncode == 0:
                v = t.out("show", "-gv", "status-fg").rstrip("\n")
                forms.append((c, ("Ok", v)))
            else:
                forms.append((c, ("Err", r.stderr.rstrip("\n"))))
        # pane-colours[] goes through the same parser (options.c:616)
        for c, res in forms[::7]:
            r = t("set", "-g", "pane-colours[0]", c)
            if res[0] == "Ok":
                assert r.returncode == 0, (c, r.stderr)
                assert t.out("show", "-gv", "pane-colours[0]").rstrip("\n") == res[1], c
            else:
                assert r.returncode != 0 and r.stderr.rstrip("\n") == res[1], (c, r.stderr)
        # `#{c:X}`: colour_force_rgb
        rgb_inputs = [c for c, res in forms if res[0] == "Ok" and "#" not in c and "," not in c]
        rgb_inputs += ["#ff8000", "#010203", "nope", "none", "colour9", "colour196", "colour232"]
        got = t.expand("|".join("#{c:%s}" % c for c in rgb_inputs)).split("|")
        assert len(got) == len(rgb_inputs)
        rgb = list(zip(rgb_inputs, got))
        pal = t.expand("|".join("#{c:colour%d}" % n for n in range(256))).split("|")
        assert len(pal) == 256
        grey = t.expand("|".join("#{c:grey%d}" % n for n in range(101))).split("|")
        assert len(grey) == 101
        # the X11 table of colour.c, each name checked against the reference
        src = open(os.path.join(TMUX_SRC, "colour.c")).read()
        i = src.index("colour_byname(const char *name)")
        blk = src[i:src.index("};", i)]
        names = re.findall(r'\{ "([^"]+)", 0x([0-9a-fA-F]{6}) \}', blk)
        assert len(names) > 500
        # (colour_fromstring tries the ANSI names before the table, so the table's
        # `black red green yellow blue magenta cyan white` entries are never reached.)
        shadowed = {"black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"}
        got = t.expand("|".join("#{c:%s}" % n for n, _ in names)).split("|")
        for (n, v), g in zip(names, got):
            assert g == v or n.lower() in shadowed, (n, v, g)
    out = "".join(
        "    (%s, %s),\n" % (rs(c), ("Ok(%s)" if k == "Ok" else "Err(%s)") % rs(v))
        for c, (k, v) in forms)
    inject("colour.rs", "colour_forms",
           "    // set -g status-fg X; show -gv status-fg   (every 7th also set -g pane-colours[0] X)\n"
           "    const FORMS: &[(&str, Result<&str, &str>)] = &[\n" + out + "    ];\n")
    inject("colour.rs", "colour_rgb",
           "    // set -gF @out '#{c:X}'; show -gv @out   (colour_force_rgb: hex, or empty)\n"
           "    const RGB_ROWS: &[(&str, &str)] = &[\n"
           + "".join("    (%s, %s),\n" % (rs(c), rs(v)) for c, v in rgb) + "    ];\n"
           "    // #{c:colourN} for N in 0..=255\n    const PALETTE: &[&str] = &[\n"
           + "".join("    %s,\n" % rs(v) for v in pal) + "    ];\n"
           "    // #{c:greyN} for N in 0..=100\n    const GREYS: &[&str] = &[\n"
           + "".join("    %s,\n" % rs(v) for v in grey) + "    ];\n")
    inject("colour.rs", "x11", "".join(
        '    ("%s", (0x%s, 0x%s, 0x%s)),\n' % (n, v[0:2], v[2:4], v[4:6]) for n, v in names))


# ----------------------------------------------------------------------------- styles

# Each is fed to `set -g status-style`: every grammar item of style.c:96-283 and the
# places it can fail.
STYLE_INPUTS = [
    "", "default", "DEFAULT", "none", "NONE", " , ,\n", "fg=red", "FG=RED", "Bg=Blue", "bg=#102030",
    "fg=colour196,bg=colour17", "fg=default", "bg=default", "fg=terminal", "fg=themegreen",
    "fg=nope", "bg=", "fg=", "xg=red", "fg=#12345", "fg=red,bg=blue,bold", "fg=red bg=blue italics",
    "fg=red\nbg=blue", "fg=red,,bg=blue", "  fg=red  ", "us=red", "us=#00ff00", "us=default", "us=",
    "us=nope", "bold", "BOLD", "bright", "dim", "underscore", "blink", "reverse", "hidden", "italics",
    "strikethrough", "double-underscore", "curly-underscore", "dotted-underscore", "dashed-underscore",
    "overline", "acs", "bold,italics", "bold|italics", "bold italics reverse", "bold,",
    "underscore|reverse", "bold,nope", "nope", "noattr", "NOATTR", "nobold", "nobright", "nodim",
    "noreverse", "NOBOLD", "nodouble-underscore", "nonope", "no", "nonone", "nodefault", "bold,nobold",
    "nobold,bold", "nolink", "link=http://example.com", "link=", "link=a b", "NOLINK", "LINK=x",
    "link=" + "a" * 250, "link=" + "a" * 251,
    "list=on", "list=focus", "list=left-marker", "list=right-marker", "LIST=ON", "list=", "list=off",
    "list=nope", "nolist", "NOLIST",
    "align=left", "align=centre", "align=right", "align=absolute-centre", "ALIGN=CENTRE", "align=center",
    "align=", "align=nope", "noalign",
    "range=left", "range=right", "RANGE=LEFT", "range=left|1", "range=right|x",
    "range=pane|%3", "range=pane|%0", "range=pane|3", "range=pane|%", "range=pane|%x",
    "range=pane|%4294967295", "range=pane|%4294967296", "range=pane", "range=pane|",
    "range=window|3", "range=window|0", "range=window|x", "range=window|-1", "range=window|4294967295",
    "range=window|4294967296", "range=window",
    "range=session|$2", "range=session|2", "range=session|$", "range=session|$x", "range=session",
    "range=control|0", "range=control|9", "range=control|10", "range=control|x", "range=control",
    "range=user|abc", "range=user|a|b", "range=user|0123456789abcdef", "range=user|0123456789abcde",
    "range=user", "range=user|", "range=foo", "range=foo|1", "range=", "range=|", "norange", "NORANGE",
    "fill=red", "fill=#abcdef", "fill=default", "fill=nope", "fill=",
    "dim=0", "dim=50", "dim=50%", "dim=100", "dim=101", "dim=-1", "dim=x", "dim=", "dim=%", "DIM=10",
    "width=10", "width=0", "width=10%", "width=100%", "width=101%", "width=%", "width=x", "width=",
    "width=2147483647", "width=2147483648", "width=4294967295", "width=4294967296", "width=-1",
    "pad=2", "pad=0", "pad=x", "pad=", "pad=2147483648", "pad=-1",
    "push-default", "pop-default", "set-default", "PUSH-DEFAULT", "ignore", "noignore", "IGNORE",
    "fg=red,align=right,range=window|2,list=on,bold,width=5,pad=1,dim=20,us=blue,fill=black,link=u",
    "a" * 255, "a" * 256, "fg=" + "r" * 252, "fg=" + "r" * 253,
]
# style_tostring has no case for range=control (style.c:340-358) and prints the label
# left over from the previous branch; not copied, so that line is not compared.
NO_DISPLAY = {s for s in STYLE_INPUTS if s.startswith("range=control")}


def settle(fd, quiet=0.6, limit=4.0):
    """Read the client's output until it has been quiet for `quiet` seconds."""
    buf = b""
    end = time.time() + limit
    last = time.time()
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                d = os.read(fd, 65536)
            except OSError:
                break
            if not d:
                break
            buf += d
            last = time.time()
        elif time.time() - last > quiet:
            break
    return buf


def attach(sk, cols, rows, extra_args=()):
    pid, fd = pty.fork()
    if pid == 0:
        fcntl.ioctl(0, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.execvpe(REF, [REF, "-u", "-L", sk, "-f", "/dev/null", "attach", *extra_args], ENV)
    return pid, fd


def reap(pid, fd):
    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except OSError:
        pass


def logged_style(s):
    """What the reference's style_parse made of one style string: the
    `format_draw: style 'S' -> 'T'` line of a -vv server (T is style_tostring)."""
    with tempfile.TemporaryDirectory() as d:
        sk = sock()
        r = subprocess.run([REF, "-vv", "-L", sk, "-f", "/dev/null", "new-session", "-d", "-x", "40",
                            "-y", "5", "cat"], env=ENV, capture_output=True, text=True, cwd=d)
        assert r.returncode == 0, r.stderr
        try:
            tmux(sk, "set", "-g", "status-style", "default", cwd=d)
            # `%` goes through strftime in status-format; keep it literal
            tmux(sk, "set", "-g", "status-format[0]", ("#[" + s + "]x").replace("%", "%%"), cwd=d)
            pid, fd = attach(sk, 40, 5)
            settle(fd, quiet=0.5, limit=3.0)
            tmux(sk, "kill-server", cwd=d)
            reap(pid, fd)
        finally:
            tmux(sk, "kill-server", cwd=d)
        want = "format_draw: style '%s' -> '" % s
        found = set()
        for f in glob.glob(os.path.join(d, "tmux-server-*.log")):
            for line in open(f, errors="replace"):
                if want in line:
                    found.add(line.rstrip("\n").split(want, 1)[1][:-1])
        # `acs` is also drawn elsewhere (borders), on top of a themed style
        found = {x for x in found if "theme" not in x} or found
        assert len(found) == 1, (s, found)
        return found.pop()


def gen_style():
    rows = []
    with Server() as t:
        for s in STYLE_INPUTS:
            r = t("set", "-g", "status-style", s)
            if r.returncode == 0:
                rows.append([s, True, None])
            else:
                assert r.stderr.rstrip("\n") == "invalid style: " + s, (s, r.stderr)
                rows.append([s, False, None])
        # display-panes-colour is a string option checked by style_parse_colour
        colours = []
        for c in COLOUR_INPUTS + ["#{x}", "red#{x}"]:
            r = t("set", "-g", "display-panes-colour", c)
            if r.returncode == 0:
                assert t.out("show", "-gv", "display-panes-colour").rstrip("\n") == c
                colours.append((c, None))
            else:
                colours.append((c, r.stderr.rstrip("\n")))
        # style options skip the check when the value holds `#{`
        for s in ["#{x}", "fg=nope#{x}", "fg=#{@c}"]:
            assert t("set", "-g", "status-style", s).returncode == 0
    todo = [r for r in rows if r[1] and r[0] not in NO_DISPLAY and "\n" not in r[0]
            and r[0].strip(" ,") != ""]
    with cf.ThreadPoolExecutor(max_workers=8) as ex:
        for r, disp in zip(todo, ex.map(lambda r: logged_style(r[0]), todo)):
            r[2] = disp
    body = ("    // set -g status-style 'S': accepted?, then for accepted S the style_tostring of what\n"
            "    // style_parse built, from a -vv server drawing '#[S]x' (None: not compared)\n"
            "    const STYLE_CASES: &[(&str, bool, Option<&str>)] = &[\n")
    for s, ok, d in rows:
        body += "        (%s, %s, %s),\n" % (rs(s), "true" if ok else "false",
                                              "Some(%s)" % rs(d) if d is not None else "None")
    body += "    ];\n"
    body += ("    // set -g display-panes-colour X: None is accepted, Some is the error text\n"
             "    const COLOUR_STRING_CASES: &[(&str, Option<&str>)] = &[\n")
    for c, e in colours:
        body += "        (%s, %s),\n" % (rs(c), "Some(%s)" % rs(e) if e is not None else "None")
    body += "    ];\n"
    inject("style.rs", "style_cases", body)


# ----------------------------------------------------------------------------- widths

WCWIDTH_C = r"""
#define _XOPEN_SOURCE 700
#include <locale.h>
#include <wchar.h>
#include <stdio.h>
/* tmux's utf8_width without its cache: glibc wcwidth; negative is 0 for the C1
 * controls and 1 otherwise (utf8.c:575-583). */
static int tw(unsigned c) { int w = wcwidth(c); if (w < 0) w = (c >= 0x80 && c <= 0x9f) ? 0 : 1; return w; }
int main(void) {
    if (!setlocale(LC_CTYPE, "en_US.UTF-8")) { fprintf(stderr, "no en_US.UTF-8\n"); return 1; }
    int prev = 1; unsigned start = 0x80;
    for (unsigned c = 0x80; c <= 0x110000; c++) {
        int w = 1;
        if (c <= 0x10FFFF) w = (c >= 0xD800 && c <= 0xDFFF) ? prev : tw(c);
        if (w != prev) { if (prev != 1) printf("%X %X %d\n", start, c - 1, prev); start = c; prev = w; }
    }
    return 0;
}
"""

# label, text
WIDTH_STRINGS = [
    ("empty", ""), ("ascii", "hello"), ("space", " "), ("tab", "a\tb"), ("c0 controls", "a\x01b\x1fc"),
    ("del", "a\x7fb"), ("c1 control", "a\u0085b"), ("c1 range", "\u0080\u0090\u009f"), ("nbsp", "\u00a0"),
    ("latin1", "caf\u00e9"), ("greek", "\u03b1\u03b2"), ("cjk", "\u4e16\u754c"), ("cjk+ascii", "a\u4e16b"),
    ("hiragana", "\u3042"), ("katakana", "\u30a2"), ("hangul syllable", "\ud55c"),
    ("hangul jamo cho", "\u1100"), ("hangul jamo jung", "\u1161"), ("hangul jamo jong", "\u11a8"),
    ("hangul filler", "\u3164"), ("fullwidth a", "\uff21"), ("halfwidth katakana", "\uff71"),
    ("combining acute", "e\u0301"), ("combining only", "\u0301"), ("combining x3", "a\u0301\u0302\u0303"),
    ("zero width space", "\u200b"), ("zwj", "\u200d"), ("zwnj", "\u200c"), ("bom", "\ufeff"),
    ("variation selector 16", "\ufe0f"), ("heart text", "\u2764"), ("heart emoji vs16", "\u2764\ufe0f"),
    ("grinning", "\U0001f600"), ("thumbs up", "\U0001f44d"),
    ("thumbs up + tone", "\U0001f44d\U0001f3fd"), ("tone alone", "\U0001f3fd"),
    ("ok hand + tone", "\U0001f44c\U0001f3fb"), ("raised hand", "\u270b"),
    ("index pointing up", "\u261d"), ("man zwj woman", "\U0001f468\u200d\U0001f469"),
    ("family", "\U0001f468\u200d\U0001f469\u200d\U0001f467"),
    ("rainbow flag", "\U0001f3f3\ufe0f\u200d\U0001f308"),
    ("flag JP", "\U0001f1ef\U0001f1f5"), ("regional A", "\U0001f1e6"), ("regional Z", "\U0001f1ff"),
    ("keycap", "1\ufe0f\u20e3"), ("mahjong", "\U0001f004"), ("square", "\u2b1b"), ("star", "\u2b50"),
    ("box drawing", "\u2500\u2502"), ("block", "\u2588"), ("powerline", "\ue0b0"), ("private use", "\ue000"),
    ("noncharacter", "\ufffe"), ("replacement", "\ufffd"), ("max plane", "\U0010fffd"),
    ("tag char", "\U000e0020"), ("arabic", "\u0645\u0631"), ("thai", "\u0e01\u0e34"),
    ("devanagari", "\u0915\u094d"), ("braille", "\u2800"), ("math", "\u2211\u221e"),
    ("mixed", "a\u4e16\U0001f600\u0301b"), ("long ascii", "x" * 100),
]

# strings for format_width: `#{w:S}` (S is format-expanded first, so `##` halves)
FORMAT_WIDTH_STRINGS = [
    "ab", "#[fg=red]ab", "a#[bold]b#[default]c", "#[fg=red]#[bg=blue]", "##", "###", "####", "#####",
    "a##b", "a###b", "##[x", "####[x", "####[fg=red]x", "#####[fg=red]x", "#[unterminated",
    "ab#[unterminated", "#", "a#", "#x", "##x", "#[a]#[b]#[c]", "\u4e16#[bold]\u754c", "a#[fg=#ff0000]b",
    "#[range=user|a#,b]x", "#[align=right]ab#[default]", "#[x#]y]z", "tab\there", "e\u0301#[bold]e\u0301",
    "#[a#{b}c]d", "#[a#}b]c", "#[a##]b",
]


def gen_width():
    with tempfile.TemporaryDirectory() as d:
        c = os.path.join(d, "w.c")
        open(c, "w").write(WCWIDTH_C)
        subprocess.run(["gcc", "-o", os.path.join(d, "w"), c], check=True)
        ranges = subprocess.run([os.path.join(d, "w")], check=True, capture_output=True,
                                text=True).stdout.split("\n")
    ranges = [l.split() for l in ranges if l]
    zero = [(int(a, 16), int(b, 16)) for a, b, w in ranges if w == "0"]
    wide = [(int(a, 16), int(b, 16)) for a, b, w in ranges if w == "2"]
    assert len(zero) + len(wide) == len(ranges)
    u = open(os.path.join(TMUX_SRC, "utf8.c")).read()
    overrides = [(int(a, 16), int(b)) for a, b in
                 re.findall(r"\{ \.wc = 0x([0-9A-F]+), \.width = (\d) \}", u[:u.index("struct utf8_item")])]
    assert overrides == sorted(overrides) and len(overrides) > 100
    uc = open(os.path.join(TMUX_SRC, "utf8-combined.c")).read()
    k = uc.index("/* Emoji skin tone modifiers. */")
    skin = [int(a, 16) for a in re.findall(r"case 0x([0-9A-F]+):", uc[k:uc.index("return (1);", k)])]
    assert skin == sorted(skin) and len(skin) > 50

    def fmt(r):
        return "".join("    (%s, %s),\n" % (hx(a), hx(b)) for a, b in r)

    inject("width.rs", "width_zero", fmt(zero))
    inject("width.rs", "width_wide", fmt(wide))
    inject("width.rs", "width_overrides", "".join("    (%s, %d),\n" % (hx(a), b) for a, b in overrides))
    inject("width.rs", "width_skin", "".join("    %s,\n" % hx(c) for c in skin))

    def scalar(c):
        return 0 <= c <= 0x10FFFF and not 0xD800 <= c <= 0xDFFF

    def table_width(c):
        for a, b in wide:
            if a <= c <= b:
                return 2
        for a, b in zero:
            if a <= c <= b:
                return 0
        return 1

    probes = {0x20, 0x41, 0x7E, 0xA0, 0xA1, 0xFF, 0x100}
    for a, b in zero + wide:
        for c in (a - 1, a, b, b + 1):
            if scalar(c) and c >= 0x80:
                probes.add(c)
    probes |= {cp for cp, _ in overrides}
    probes |= {0x80, 0x9F, 0xA0, 0x10FFFF, 0x10FFFD, 0x1F600, 0x4E16, 0xFE0F, 0x200D, 0x3164}
    probes = sorted(p for p in probes if scalar(p))
    override = dict(overrides)
    with Server() as t:
        got = dict(zip(probes, t.widths([chr(p) for p in probes])))
        for p in probes:
            want = override.get(p, table_width(p) if p >= 0x80 else 1)
            assert got[p] == want, ("U+%04X" % p, got[p], want)
        strings = [(lb, tx, w) for (lb, tx), w in
                   zip(WIDTH_STRINGS, t.widths([tx for _, tx in WIDTH_STRINGS]))]
        # format_width sees the variable's raw text (`##`, `#[..]` untouched)
        fw = list(zip(FORMAT_WIDTH_STRINGS, t.widths(FORMAT_WIDTH_STRINGS)))
    sample = sorted(set(probes[::6] + [p for p in probes if p in override][::3] + [0x20, 0x7E, 0x10FFFD]))
    inject("width.rs", "width_strings",
           "    // set -g @v TEXT; set -gF @out '#{w:@v}'; show -gv @out\n"
           "    const STRINGS: &[(&str, usize)] = &[\n"
           + "".join("        (%s, %d), // %s\n" % (rs(tx), w, lb) for lb, tx, w in strings) + "    ];\n")
    inject("width.rs", "width_probes",
           "    // set -gF @out '#{w:X}'; show -gv @out, at the edges of every range (the generator\n"
           "    // checks all of them; a sample is kept here)\n"
           "    const PROBES: &[(u32, usize)] = &[\n"
           + "".join("        (%s, %d),\n" % (hx(p), got[p]) for p in sample) + "    ];\n")
    inject("styled.rs", "format_width",
           "    // set -g @w S; set -gF @out '#{w:@w}'; show -gv @out  (format_width of the raw text)\n"
           "    const FORMAT_WIDTHS: &[(&str, usize)] = &[\n"
           + "".join("        (%s, %d),\n" % (rs(s), w) for s, w in fw) + "    ];\n")


# ----------------------------------------------------------------------------- clusters

# What `screen_write_combine` does with a run of characters, seen in a reference pane: the
# text a pane prints, then its cursor column and what capture-pane shows.
CLUSTER_STRINGS = [
    "ab", "a\u4e16b", "e\u0301x", "\u0301", "a\u0301\u0302", "\u200d", "a\u200db", "\ufe0f", "a\ufe0fb",
    "\u2764\ufe0f", "a\u2764\ufe0fb", "\u2764\ufe0f\u2764\ufe0f", "\u263a\ufe0f", "1\ufe0f\u20e3",
    "\U0001f44d", "\U0001f44d\U0001f3fd", "a\U0001f44d\U0001f3fdb", "\U0001f3fd", "\U0001f600\U0001f3fd",
    "\U0001f44d\U0001f3fd\U0001f3fd", "\U0001f468\u200d\U0001f469", "\U0001f468\u200d\U0001f469\u200d\U0001f467",
    "\U0001f468\u200d\U0001f469\u200d\U0001f467\u200d\U0001f466\u200d\U0001f466\u200d\U0001f466",
    "\U0001f3f3\ufe0f\u200d\U0001f308", "a\U0001f468\u200d\U0001f469b", "\U0001f1ef\U0001f1f5",
    "\U0001f1ef\U0001f1f5\U0001f1fa", "\U0001f1ef\U0001f1f5\U0001f1fa\U0001f1f8",
    "\U0001f1ef", "\U0001f1ef\U0001f600", "\u1100\u1161", "\u1100\u1161\u11a8", "\u1161", "\u11a8",
    "a\u1161", "\u1100\u1100", "\u3164", "a\u3164b", "\u4e16\u0301", "\u4e16\u200d\u4e16",
    "\u00e9\u0301", "\u200d\u200d", "a\u200d\u200d\u200d", "\U0001f44d\u200d", "\U0001f44d\u200d\U0001f3fd",
    "\u2b50\ufe0f", "x" * 5 + "\u4e16" * 3,
]


def gen_cluster():
    rows = []
    for text in CLUSTER_STRINGS:
        octal = "".join("\\%03o" % b for b in text.encode())
        with Server(cols=40, rows=5) as t:
            t.out("respawn-pane", "-k", "sh -c 'printf \"%b\" \"" + octal + "\"; sleep 30'")
            for _ in range(40):
                time.sleep(0.05)
                x = t.out("display-message", "-p", "#{cursor_x}").strip()
                if x != "0":
                    break
            time.sleep(0.15)
            x = int(t.out("display-message", "-p", "#{cursor_x}").strip())
            cap = t.out("capture-pane", "-p").split("\n")[0].rstrip(" ")
            rows.append((text, cap, x))
    inject("styled.rs", "cluster_cases",
           "    // A reference pane (40x5) prints TEXT with printf; its #{cursor_x} and the first row of\n"
           "    // capture-pane -p (trailing blanks trimmed). Pane output goes through the same\n"
           "    // screen_write_cell / screen_write_combine as status lines.\n"
           "    const CLUSTERS: &[(&str, &str, usize)] = &[\n"
           + "".join("        (%s, %s, %d),\n" % (rs(t), rs(c), x) for t, c, x in rows) + "    ];\n")


# ----------------------------------------------------------------------------- owner configs

OWNER_CONFS = os.environ.get("VTMUX_OWNER_CONFS", "").split(":") if os.environ.get("VTMUX_OWNER_CONFS") else [
    os.path.expanduser("~/dotfiles/tmux/.tmux.conf"),
    os.path.expanduser("~/.local/share/mu-crew-dotfiles/tmux/mu-crew.conf"),
]


def gen_owner():
    """Every `#[...]` in the owner's configs, `#{@name}` replaced by the hex the config
    gives @name (else a fixed hex), each checked with `set -g status-style`."""
    texts = [open(p).read() for p in OWNER_CONFS]
    values = {}
    for text in texts:
        for m in re.finditer(r"""^set -g (@\w+) ['"](#[0-9a-fA-F]{6})['"]""", text, re.M):
            values[m.group(1)] = m.group(2)
    styles = []
    for text in texts:
        # drop comment lines (`# ...`), but not format continuation lines (`#{?...`)
        text = "\n".join(l for l in text.split("\n") if not re.match(r"\s*#(?![{\[])", l))
        pos = 0
        while True:
            i = text.find("#[", pos)
            if i < 0:
                break
            j = text.find("]", i)
            body = text[i + 2:j]
            pos = j + 1
            body = re.sub(r"#\{(@\w+)\}", lambda m: values.get(m.group(1), "#a6adc8"), body)
            assert "#{" not in body, body
            if body not in styles:
                styles.append(body)
    assert len(styles) >= 15, styles
    with Server() as t:
        for s in styles:
            r = t("set", "-g", "status-style", s)
            assert r.returncode == 0, (s, r.stderr)
    inject("styled.rs", "owner_styles",
           "    /// The distinct `#[...]` bodies of ~/dotfiles/tmux/.tmux.conf and mu-crew.conf; each was\n"
           "    /// accepted by `set -g status-style` on the reference (`style_cases.sh`, `gen_owner`).\n"
           "    const OWNER_STYLES: &[&str] = &[\n"
           + "".join("        %s,\n" % rs(s) for s in styles) + "    ];\n")


# ----------------------------------------------------------------------------- styled rows

# (name, format, columns, status-style, ranges?). No `%` (status lines go through strftime).
STYLED = [
    ("plain", "hello world", 20, "default", False),
    ("empty", "", 10, "default", False),
    ("colours and attrs", "#[fg=red,bg=#102030,bold]hi#[default] there", 24, "default", False),
    ("nested styles", "#[fg=red]a#[bg=blue]b#[nobold,italics]c#[bold]d#[none]e#[default]f", 20, "default", False),
    ("every attribute", "#[bold]a#[nobold,dim]b#[nodim,underscore]c#[nounderscore,blink]d#[noblink,reverse]e"
     "#[noreverse,hidden]f#[nohidden,italics]g#[noitalics,strikethrough]h#[nostrikethrough,overline]i"
     "#[none]j", 20, "default", False),
    ("underline kinds", "#[double-underscore]a#[nodouble-underscore,curly-underscore]b"
     "#[nocurly-underscore,dotted-underscore]c#[nodotted-underscore,dashed-underscore]d"
     "#[nodashed-underscore,us=#ff8800,underscore]e#[us=red,italics]f#[us=default,noitalics]g"
     "#[us=colour9,bold]h", 20, "default", False),
    ("colour forms", "#[fg=colour196]a#[fg=brightred]b#[fg=91]c#[fg=#00ff00]d#[fg=terminal]e#[fg=default]f"
     "#[bg=alice blue]g#[bg=colour17]h#[fg=3,bg=7]i#[bg=terminal]j", 24, "default", False),
    ("base style and default", "x#[fg=red]a#[default]b#[fg=default]c#[bg=default]d", 12,
     "fg=yellow,bg=blue", False),
    ("base attrs and default", "x#[nobold]a#[default]b#[none]c", 12, "bold,fg=green,underscore", False),
    ("push default", "#[bg=blue]a#[push-default]b#[fg=red]c#[default]d#[pop-default]e#[default]f", 16,
     "default", False),
    ("pop default", "#[fg=red,push-default]a#[bg=blue]b#[default]c#[pop-default]d#[default]e", 16,
     "default", False),
    ("set default", "#[fg=red,set-default]a#[bg=blue]b#[default]c#[pop-default]d#[default]e", 16,
     "fg=yellow", False),
    ("push default under base", "#[bg=red]a#[push-default]b#[default]c#[pop-default]d#[default]e", 16,
     "fg=yellow,bg=blue", False),
    ("status-left shape", "#[align=left range=left default]#[push-default]L #[pop-default]#[norange default]"
     "#[nolist align=right range=right default]#[push-default]R#[pop-default]#[norange default]", 20,
     "bg=blue,fg=white", True),
    ("align left centre right", "L#[align=centre]C#[align=right]R", 20, "default", False),
    ("align all four", "#[align=left]LL#[align=centre]CC#[align=right]RR#[align=absolute-centre]AA", 21,
     "default", False),
    ("align overflow", "#[align=left]LLLL#[align=centre]CCCC#[align=right]RRRR", 8, "default", False),
    ("align overflow hard", "#[align=left]LLLLLL#[align=centre]CCCC#[align=right]RRRRRR", 8, "default", False),
    ("align overflow left only", "#[align=left]LLLLLLLLLLLL#[align=right]RR", 8, "default", False),
    ("align odd widths", "#[align=left]L#[align=centre]CCC#[align=right]R", 12, "default", False),
    ("absolute centre", "#[align=absolute-centre]MID", 11, "default", False),
    ("absolute centre with sides", "left#[align=absolute-centre]MID#[align=right]right", 21, "default", False),
    ("absolute centre overflow", "#[align=absolute-centre]MIDDLEPART", 6, "default", False),
    ("styled aligns", "#[fg=red]L#[align=centre,fg=green]C#[align=right,fg=blue,bold]R", 15, "default", False),
    ("noalign", "#[align=right]R#[noalign]L", 10, "default", False),
    ("fill", "#[fill=blue]x", 10, "default", False),
    ("fill and align", "#[fill=colour4]a#[align=right,bg=red]b", 10, "default", False),
    ("fill over base", "#[fill=red]x", 10, "bg=blue", False),
    ("hash escapes", "a##b#[fg=red]c", 12, "default", False),
    ("hash runs before style", "###[fg=red]x####[fg=red]y", 24, "default", False),
    ("hash at end", "abc#", 8, "default", False),
    ("ignore", "#[ignore]#[fg=red]a#[noignore]#[fg=green]b", 12, "default", False),
    ("ignore hashes", "#[ignore]##[x #[noignore]y", 16, "default", False),
    ("invalid style skipped", "a#[bogus]b#[fg=nope]c#[fg=red]d", 12, "default", False),
    ("invalid style rolls back", "#[fg=red,bogus]a#[fg=blue]b", 12, "default", False),
    ("unterminated style", "abc#[fg=red", 12, "default", False),
    ("unterminated after align", "#[align=right]ab#[fg=red", 12, "default", False),
    ("style with escapes", "#[fg=red]a#[align=#]]b", 12, "default", False),
    ("layout-only styles", "#[width=5,pad=2]ab#[width=7]cd#[dim=50]ef", 12, "default", False),
    ("link style", "#[link=http://x]ab#[nolink]cd", 12, "default", False),
    ("dim style", "#[dim=50,fg=red]ab#[dim=0]cd", 12, "default", False),
    ("wide characters", "a\u4e16\u754cb", 12, "default", False),
    ("wide cut at the edge", "ab\u4e16\u754c", 5, "default", False),
    ("wide cut in centre", "#[align=centre]\u4e16\u754c\u4e16\u754c", 7, "default", False),
    ("wide right trim", "#[align=right]\u4e16\u754c\u4e16", 5, "default", False),
    ("combining mark", "e\u0301x", 8, "default", False),
    ("emoji", "a\U0001f600b", 8, "default", False),
    ("emoji tone", "a\U0001f44d\U0001f3fdb", 8, "default", False),
    ("zwj sequence", "a\U0001f468\u200d\U0001f469b", 8, "default", False),
    ("flag", "a\U0001f1ef\U0001f1f5b", 8, "default", False),
    ("control chars dropped", "a\x01b\x7fc\td", 8, "default", False),
    ("wide with style", "#[fg=red,bg=blue]\u4e16#[default]\u754c", 8, "default", False),
    # lists
    ("list fits", "#[align=left]A#[list=on]1 2 3#[nolist]Z", 20, "default", False),
    ("list focus fits", "#[align=left]A#[list=on]1 #[list=focus]2#[list=on] 3#[nolist]Z", 20, "default", False),
    ("list markers fit", "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "abc #[list=focus]def#[list=on] ghi#[nolist]Z", 30, "default", False),
    ("list overflow focus middle", "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>"
     "#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd eeee#[nolist]Z", 20, "default", False),
    ("list overflow focus start", "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>"
     "#[list=on]#[list=focus]aaaa#[list=on] bbbb cccc dddd eeee#[nolist]Z", 20, "default", False),
    ("list overflow focus end", "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>"
     "#[list=on]aaaa bbbb cccc dddd #[list=focus]eeee#[nolist]Z", 20, "default", False),
    ("list overflow no focus", "#[align=left]A#[list=on]#[list=left-marker]<#[list=right-marker]>"
     "#[list=on]aaaa bbbb cccc dddd eeee#[nolist]Z", 20, "default", False),
    ("list overflow no markers", "#[align=left]A#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd eeee"
     "#[nolist]Z", 14, "default", False),
    ("list centre", "L#[align=centre]C#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z#[align=right]R", 30, "default", False),
    ("list centre fits", "L#[align=centre]C#[list=on]abc#[list=focus]def#[nolist]Z#[align=right]R", 30,
     "default", False),
    ("list right", "L#[align=right]R#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z", 24, "default", False),
    ("list right fits", "L#[align=right]R#[list=on]abc#[list=focus]def#[nolist]Z", 24, "default", False),
    ("list absolute centre", "L#[align=absolute-centre]X#[list=on]#[list=left-marker]<#[list=right-marker]>"
     "#[list=on]aaaa bbbb #[list=focus]cccc#[list=on] dddd#[nolist]Z#[align=right]R", 30, "default", False),
    ("list absolute centre fits", "L#[align=absolute-centre]X#[list=on]ab#[list=focus]cd#[nolist]Z", 30,
     "default", False),
    ("list squeezed to nothing", "#[align=left]LLLLLLLL#[list=on]abc#[nolist]Z", 8, "default", False),
    ("list with window-status shape", "#[align=left range=left default]#[push-default]0 #[pop-default]"
     "#[norange default]#[list=on align=left]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "#[range=window|0 list=focus underscore]#[push-default]0:w*#[pop-default]#[norange list=on default] "
     "#[range=window|1 default]#[push-default]1:second #[pop-default]#[norange default]"
     "#[nolist align=right range=right default]#[push-default]R#[pop-default]#[norange default]", 30,
     "bg=blue,fg=white", True),
    ("list with window-status overflow", "#[align=left range=left default]#[push-default]LEFT #[pop-default]"
     "#[norange default]#[list=on align=left]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "#[range=window|0 default]#[push-default]0:one #[pop-default]#[norange list=on default] "
     "#[range=window|1 list=focus default]#[push-default]1:two* #[pop-default]#[norange list=on default] "
     "#[range=window|2 default]#[push-default]2:three #[pop-default]#[norange list=on default] "
     "#[range=window|3 default]#[push-default]3:four #[pop-default]#[norange default]"
     "#[nolist align=right range=right default]#[push-default]RIGHT#[pop-default]#[norange default]", 40,
     "bg=blue,fg=white", True),
    # ranges (pane ranges must name a pane of the current window, %0; session $1 is `other`)
    ("ranges", "#[range=user|ab]hello#[norange] #[range=window|2]win#[norange]#[range=pane|%0]P#[norange]"
     "#[range=session|$1]S#[norange]#[align=right]#[range=left]R#[norange]", 30, "default", True),
    ("range runs into next", "#[range=window|1]ab#[range=window|2]cd#[norange]ef", 12, "default", True),
    ("range same type continues", "#[range=left]ab#[fg=red]cd#[range=left]ef#[norange]gh", 12, "default", True),
    ("range left and right", "#[range=left]L#[norange]#[align=right]#[range=right]R#[norange]", 12, "default", True),
    ("range open at end dropped", "ab#[range=left]cd", 12, "default", True),
    ("range empty dropped", "ab#[range=left]#[norange]cd", 12, "default", True),
    ("range in centre", "a#[align=centre]#[range=user|mid]mid#[norange]#[align=right]z", 20, "default", True),
    ("range cut by width", "#[range=user|a]abcdefgh#[norange]", 5, "default", True),
    ("range in right cut", "#[align=left]LLL#[align=right]#[range=user|r]rrrrrr#[norange]", 8, "default", True),
    ("range in wide text", "#[range=user|w]\u4e16\u754c#[norange]x", 8, "default", True),
    ("range user label", "#[range=user|0123456789abcdef]ab#[norange]", 8, "default", True),
    ("range control", "#[range=control|3]ab#[norange]", 8, "default", True),
    ("range in list", "#[align=left]#[list=on]#[range=user|a]aa#[norange]#[range=user|b]bb#[norange]#[nolist]", 12,
     "default", True),
    ("range in list overflow", "#[align=left]#[list=on]#[list=left-marker]<#[list=right-marker]>#[list=on]"
     "#[range=user|a]aaaa#[norange]#[range=user|b list=focus]bbbb#[norange list=on]#[range=user|c]cccc#[norange]"
     "#[nolist]", 10, "default", True),
    ("range in list markers", "#[align=left]#[list=on]#[list=left-marker]#[range=user|lm]<#[norange]"
     "#[list=right-marker]#[range=user|rm]>#[norange]#[list=on]aaaa bbbb cccc#[nolist]", 8, "default", True),
    ("range after the list", "#[align=left]#[list=on]aa#[nolist]#[range=user|z]zz#[norange]", 12, "default", True),
]

# Where a click lands: status (window/pane/session/user ranges), StatusLeft/StatusRight
# (range=left/right), StatusDefault (no range), Control0-9 (range=control|N).
# Clicks within 300 ms of each other are a second and third click (server-client.c:925-950),
# so every kind is bound and the clicks are spaced further apart than that.
def random_cases(n, seed=20261029):
    """Seeded random status lines (kept stable so the goldens are reproducible): the point
    is to reach layouts nobody wrote by hand. No `%` (strftime) and no single quote."""
    import random
    rnd = random.Random(seed)
    atoms = ["a", "bc", "def", "ghij", "\u4e16", "\u4e16\u754c", "e\u0301", "##", "#", " ", "x y",
             "#[fg=red]", "#[bg=blue]", "#[bold]", "#[default]", "#[push-default]", "#[pop-default]",
             "#[nobold,italics]", "#[align=left]", "#[align=centre]", "#[align=right]",
             "#[align=absolute-centre]", "#[list=on]", "#[list=focus]", "#[nolist]",
             "#[list=left-marker]", "#[list=right-marker]", "#[fill=green]", "#[range=user|a]",
             "#[range=user|b]", "#[range=left]", "#[range=right]", "#[norange]", "#[noalign]",
             "#[fg=#445566,us=red,underscore]", "#[bogus]", "#[ignore]", "#[noignore]",
             "\U0001f44d", "\U0001f44d\U0001f3fd", "<", ">"]
    cases = []
    for i in range(n):
        fmt = "".join(rnd.choice(atoms) for _ in range(rnd.randint(3, 16)))
        cols = rnd.randint(4, 40)
        base = rnd.choice(["default", "fg=yellow,bg=blue", "bg=red"])
        cases.append(("random %02d" % i, fmt, cols, base, "range=" in fmt))
    return cases


STYLED += random_cases(120)

MOUSE_KEYS = [kind + "1" + k for kind in ("MouseDown", "SecondClick", "TripleClick")
              for k in ["Status", "StatusLeft", "StatusRight", "StatusDefault"]
              + ["Control%d" % n for n in range(10)]]


def run_styled(case):
    name, fmt, cols, base, want_ranges = case
    rows = 5
    with Server(cols=cols, rows=rows) as t:
        for _ in range(3):
            t.out("new-window", "-d", "cat")
        t.out("new-session", "-d", "-s", "other", "cat")
        t.out("select-window", "-t", "0")
        t.out("set", "-s", "terminal-features", "xterm-256color:RGB:usstyle:overline")
        t.out("set", "-g", "status-style", base)
        t.out("set", "-g", "status-format[0]", fmt)
        expanded = t.out("display-message", "-p", fmt)
        expanded = expanded[:-1] if expanded.endswith("\n") else expanded
        out = tempfile.mktemp(prefix="vtmux-mouse-")
        if want_ranges:
            for k in MOUSE_KEYS:
                t.out("bind", "-n", k, "run-shell", "-t", "=",
                      "echo '#{mouse_x}|#{mouse_status_range}|#{window_index}|#{pane_id}|#{session_id}' >> "
                      + out)
        pid, fd = attach(t.sk, cols, rows, ("-t", "0"))
        data = settle(fd)
        ranges = None
        if want_ranges:
            for x in range(cols):
                os.write(fd, b"\x1b[<0;%d;%dM" % (x + 1, rows))
                settle(fd, quiet=0.04, limit=0.2)
                os.write(fd, b"\x1b[<0;%d;%dm" % (x + 1, rows))
                settle(fd, quiet=0.36, limit=0.5)
            time.sleep(0.6)
            seen = {}
            if os.path.exists(out):
                for line in open(out):
                    x, kind, win, pane, sess = line.rstrip("\n").split("|")
                    seen[int(x)] = (kind, win, pane, sess)
                os.remove(out)
            ranges = []
            for x in range(cols):
                assert x in seen, (name, "no mouse event at column", x)
                kind, win, pane, sess = seen[x]
                if kind == "window":
                    ranges.append("window:" + win)
                elif kind == "pane":
                    ranges.append("pane:" + pane)
                elif kind == "session":
                    ranges.append("session:" + sess)
                elif kind in ("", "left", "right", "control"):
                    ranges.append(kind)  # "" is a column with no range
                else:
                    ranges.append("user:" + kind)
        t.close()
        reap(pid, fd)
    return case, expanded, data, ranges


def byte_lit(b):
    out = []
    for c in b:
        if c == 0x22:
            out.append('\\"')
        elif c == 0x5C:
            out.append("\\\\")
        elif c == 0x0A:
            out.append("\\n")
        elif c == 0x0D:
            out.append("\\r")
        elif 0x20 <= c < 0x7F:
            out.append(chr(c))
        else:
            out.append("\\x%02x" % c)
    return 'b"' + "".join(out) + '"'


def gen_styled():
    with cf.ThreadPoolExecutor(max_workers=6) as ex:
        results = list(ex.map(run_styled, STYLED))
    body = ("    // One case per status line. `bytes` is what a reference client attached under a PTY wrote\n"
            "    // (set -g status-style BASE; set -g status-format[0] FORMAT, at COLS x 5), replayed through\n"
            "    // PaneTerm by the test. `expanded` is `display-message -p FORMAT`, what format_draw is\n"
            "    // given. `ranges` is #{mouse_status_range} (with window_index, pane_id and session_id for\n"
            "    // those kinds) under a click on every column of the status row.\n"
            "    #[expect(clippy::too_many_lines, reason = \"generated data\")]\n"
            "    fn golden_cases() -> Vec<Golden> {\n        vec![\n")
    for (name, fmt, cols, base, want), expanded, data, ranges in results:
        body += "            Golden {\n"
        body += "                name: %s,\n" % rs(name)
        body += "                expanded: %s,\n" % rs(expanded)
        body += "                cols: %d,\n" % cols
        body += "                base: %s,\n" % rs(base)
        body += "                bytes: %s,\n" % byte_lit(data)
        if ranges is None:
            body += "                ranges: None,\n"
        else:
            body += "                ranges: Some(&[%s]),\n" % ", ".join(rs(r) for r in ranges)
        body += "            },\n"
    body += "        ]\n    }\n"
    inject("styled.rs", "styled_cases", body)


if __name__ == "__main__":
    what = sys.argv[1:] or ["colour", "style", "width", "cluster", "owner", "styled"]
    for w in what:
        t0 = time.time()
        {"colour": gen_colour, "style": gen_style, "width": gen_width, "cluster": gen_cluster, "owner": gen_owner,
         "styled": gen_styled}[w]()
        print(f"generated {w} in {time.time() - t0:.1f}s", file=sys.stderr)
PY
