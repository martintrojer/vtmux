#!/usr/bin/env python3
"""Regenerate the expected output of every golden case from the reference tmux 3.8.

Usage: regen.py [--dry] [CASE.case ...]   (default: every *.case next to this script)

A case file is `key=value` lines. Inputs (hand-written):
  kind     capture | reply
  cols,rows  pane size (default 20x5)
  bytes    what the pane program writes, as a printf format (octal \\NNN escapes only)
  flags    capture-pane flags, e.g. -pJe (capture kind)
  terminator  last byte of the reply to wait for (reply kind)
Outputs (written by this script):
  command  the exact recorded command line
  expected the reference output, escaped the same way as `bytes`

Run it with the reference tmux: $VTMUX_REFERENCE_TMUX or
~/.cache/vtmux/tmux-3.8/bin/tmux. Needs python3. No sleeps: the pane signals
`wait-for ready` once its bytes are written.
"""
import os, subprocess, sys, tempfile, shlex, glob

HERE = os.path.dirname(os.path.abspath(__file__))
REF = os.environ.get("VTMUX_REFERENCE_TMUX") or os.path.join(
    os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"),
    "vtmux/tmux-3.8/bin/tmux")

# Raw-mode reply reader: writes the query bytes, reads the reply up to the
# terminator byte, prints it hex-encoded, then signals wait-for.
READER = r'''
import os, sys, tty, select
tty.setraw(0)
os.write(1, bytes.fromhex(sys.argv[1]))
term = sys.argv[2].encode()
buf = b""
while not buf.endswith(term):
    if not select.select([0], [], [], 5.0)[0]:
        break
    buf += os.read(0, 256)
os.write(1, ("REPLY:" + buf.hex()).encode())
os.system(sys.argv[3] + " wait-for -S ready")
'''


def unescape(s):
    out = bytearray()
    b = s.encode()
    i = 0
    while i < len(b):
        c = b[i]
        if c != 0x5C:
            out.append(c); i += 1; continue
        n = b[i + 1]
        if n == ord("n"): out.append(10); i += 2
        elif n == ord("r"): out.append(13); i += 2
        elif n == ord("t"): out.append(9); i += 2
        elif n == 0x5C: out.append(0x5C); i += 2
        elif 0x30 <= n <= 0x37:
            out.append(int(b[i + 1:i + 4], 8)); i += 4
        else:
            raise ValueError("bad escape in %r" % s)
    return bytes(out)


def escape(b):
    out = []
    for c in b:
        if c == 0x5C: out.append("\\\\")
        elif c == 10: out.append("\\n")
        elif c == 13: out.append("\\r")
        elif c == 9: out.append("\\t")
        elif c < 32 or c == 127: out.append("\\%03o" % c)
        else: out.append(chr(c) if c < 128 else None)
    # keep UTF-8 sequences readable: re-decode runs of high bytes
    res, run = [], bytearray()
    for c, o in zip(b, out):
        if o is None:
            run.append(c)
        else:
            if run:
                res.append(run.decode("utf-8")); run = bytearray()
            res.append(o)
    if run:
        res.append(run.decode("utf-8"))
    return "".join(res)


def read_case(path):
    d = {}
    with open(path, encoding="utf-8") as f:
        for line in f:
            line = line.rstrip("\n")
            if not line or line.startswith("#"):
                continue
            k, v = line.split("=", 1)
            d[k] = v
    return d


def tmux(sock, *args, **kw):
    env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE")}
    return subprocess.run([REF, "-S", sock, "-f", "/dev/null", *args],
                          env=env, check=True, capture_output=True, **kw)


def run_case(path):
    c = read_case(path)
    cols, rows = c.get("cols", "20"), c.get("rows", "5")
    raw = unescape(c["bytes"])
    with tempfile.TemporaryDirectory() as td:
        sock = os.path.join(td, "s")
        if c["kind"] == "capture":
            shell = "printf %s; $REF wait-for -S ready; exec cat" % shlex.quote(c["bytes"])
            real = shell.replace("$REF", shlex.quote(REF))
            pane_cmd = "sh -c " + shlex.quote(real)
            recorded = "sh -c " + shlex.quote(shell)
            cap = ["capturep", *c["flags"].split()]
        else:
            rd = os.path.join(td, "reader.py")
            open(rd, "w").write(READER)
            real = "python3 %s %s %s %s; exec cat" % (
                rd, raw.hex(), shlex.quote(unescape(c["terminator"]).decode()), shlex.quote(REF))
            pane_cmd = real
            recorded = "python3 <raw-mode reader: write hex %s, read to %s, print REPLY:<hex>, tmux wait-for -S ready>; exec cat" % (raw.hex(), c["terminator"])
            cap = ["capturep", "-pJ"]  # -J: the hex line wraps at the pane width
        tmux(sock, "new-session", "-d", "-x", cols, "-y", rows, pane_cmd)
        try:
            tmux(sock, "wait-for", "ready")
            out = tmux(sock, *cap).stdout
        finally:
            subprocess.run([REF, "-S", sock, "kill-server"], capture_output=True)
    if c["kind"] == "reply":
        text = out.decode()
        assert "REPLY:" in text, text
        hexs = text.split("REPLY:", 1)[1].split("\n")[0].strip()
        out = bytes.fromhex(hexs)
        full = "new-session -d -x %s -y %s '%s' ; wait-for ready ; capturep -pJ" % (cols, rows, recorded)
    else:
        full = "new-session -d -x %s -y %s '%s' ; wait-for ready ; %s" % (
            cols, rows, recorded, " ".join(cap))
    return full, escape(out)


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    dry = "--dry" in sys.argv
    paths = args or sorted(glob.glob(os.path.join(HERE, "*.case")))
    for p in paths:
        full, exp = run_case(p)
        if dry:
            print("%s\n  %s\n  expected=%s" % (os.path.basename(p), full, exp))
            continue
        lines = [l for l in open(p, encoding="utf-8").read().split("\n")
                 if not l.startswith(("command=", "expected="))]
        while lines and lines[-1] == "":
            lines.pop()
        lines.append("command=" + full)
        lines.append("expected=" + exp)
        open(p, "w", encoding="utf-8").write("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
