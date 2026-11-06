#!/usr/bin/env python3
"""Record what the oracle scenarios' pane programs write to their terminal.

For every pane a scenario starts (`new`, `neww`, `splitw`, `respawn-pane`,
`new-pane`, `display-popup`, ...), run its program under a pty of the
pane's size (`-x`/`-y`, else 80x24), with a stub `tmux` first on PATH (so
`tmux wait-for` returns at once), type the scenario's `feed` text into it,
then end its input, and keep what it wrote (at most 256 KiB, 3 s). The
result, `oracle_programs.txt`, is the corpus `state_roundtrip.rs` imports
and exports; the test reads only that file, so it does not depend on the
host. Run from anywhere: `python3 crates/vtmux-vt/tests/state/regen_corpus.py`.
"""

import fcntl
import glob
import os
import pty
import select
import signal
import struct
import tempfile
import termios
import time
import tomllib

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "../../../.."))
SPAWN = {
    "new", "new-session", "neww", "new-window", "splitw", "split-window",
    "respawn-pane", "respawnp", "respawn-window", "respawnw", "new-pane",
    "display-popup", "popup",
}
# Options that take a value, per command family.
VALUE_FLAGS = set("cefFnstxyTXYlpwhLSRB")
POPUP_VALUE_FLAGS = VALUE_FLAGS | {"d"}


def program(argv):
    """(cols, rows, command argv) of a spawning command, or None."""
    name, args = argv[0], argv[1:]
    values = POPUP_VALUE_FLAGS if name in ("display-popup", "popup") else VALUE_FLAGS
    opts = {}
    i = 0
    while i < len(args) and args[i].startswith("-") and args[i] != "-" and len(args[i]) > 1:
        if args[i] == "--":
            i += 1
            break
        flags = args[i][1:]
        for j, f in enumerate(flags):
            if f in values:
                rest = flags[j + 1:]
                if rest:
                    opts[f] = rest
                else:
                    i += 1
                    opts[f] = args[i] if i < len(args) else ""
                break
            opts[f] = True
        i += 1
    cmd = args[i:]
    if not cmd:
        return None

    def size(key, default):
        v = opts.get(key)
        try:
            n = int(v)
            return n if 1 <= n <= 500 else default
        except (TypeError, ValueError):
            return default

    if len(cmd) == 1:
        cmd = ["sh", "-c", cmd[0]]
    return size("x", 80), size("y", 24), cmd


def run(cols, rows, cmd, feeds, stub):
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["PATH"] = stub + ":" + os.environ.get("PATH", "/usr/bin:/bin")
        os.environ["TERM"] = "tmux-256color"
        os.environ["SHELL"] = "/bin/sh"
        os.environ["PS1"] = "$ "
        os.environ.pop("TMUX", None)
        os.chdir(stub)
        try:
            os.execvp(cmd[0], cmd)
        finally:
            os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = bytearray()
    deadline = time.monotonic() + 3.0
    pending = [f.encode() for f in feeds] + [b"\x04", b"\x04"]
    next_write = time.monotonic() + 0.2
    while time.monotonic() < deadline and len(out) < 256 * 1024:
        if pending and time.monotonic() >= next_write:
            try:
                os.write(fd, pending.pop(0))
            except OSError:
                pending = []
            next_write = time.monotonic() + 0.05
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            out += chunk
        elif not pending:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                pid = 0
                # Drain what is left.
                while select.select([fd], [], [], 0.05)[0]:
                    try:
                        chunk = os.read(fd, 65536)
                    except OSError:
                        break
                    if not chunk:
                        break
                    out += chunk
                break
    if pid:
        try:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        except OSError:
            pass
    os.close(fd)
    return bytes(out[: 256 * 1024])


def escape(b):
    out = []
    for c in b:
        if c == 0x0A:
            out.append("\\n")
        elif c == 0x0D:
            out.append("\\r")
        elif c == 0x09:
            out.append("\\t")
        elif c == 0x5C:
            out.append("\\\\")
        elif 0x20 <= c < 0x7F:
            out.append(chr(c))
        else:
            out.append("\\%03o" % c)
    return "".join(out)


def main():
    seen = set()
    jobs = []
    for path in sorted(glob.glob(os.path.join(ROOT, "oracle/scenarios/*.toml"))):
        with open(path, "rb") as f:
            scenario = tomllib.load(f)
        steps = scenario.get("steps", [])
        feeds = [s["feed"]["bytes"] for s in steps if "feed" in s and "bytes" in s["feed"]]
        for step in steps:
            argv = step.get("run")
            if not argv or argv[0] not in SPAWN:
                continue
            p = program(argv)
            if p is None:
                continue
            key = (p[0], p[1], tuple(p[2]), tuple(feeds))
            if key in seen:
                continue
            seen.add(key)
            jobs.append((scenario["name"], p, feeds))
    with tempfile.TemporaryDirectory() as stub:
        tmux = os.path.join(stub, "tmux")
        with open(tmux, "w") as f:
            f.write("#!/bin/sh\nexit 0\n")
        os.chmod(tmux, 0o755)
        lines = [
            "# Generated by regen_corpus.py from oracle/scenarios: what each",
            "# scenario's pane programs wrote to their pty. Do not edit.",
        ]
        for name, (cols, rows, cmd), feeds in jobs:
            data = run(cols, rows, cmd, feeds, stub)
            if not data:
                continue
            lines.append(f"name={name} {' '.join(cmd)[:60]!r}")
            lines.append(f"size={cols}x{rows}")
            lines.append(f"bytes={escape(data)}")
    with open(os.path.join(HERE, "oracle_programs.txt"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"{(len(lines) - 2) // 3} programs")


if __name__ == "__main__":
    main()
