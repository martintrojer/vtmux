#!/usr/bin/env python3
"""Reading all of a long history: copy-mode search, capture-pane, jumps.

usage: bench-history-read.py <binary> <label> <lines> [<lines> ...]

For each line count: one server (private -S socket, -f /dev/null), one 120x40
pane with history-limit lines+1000 that cats <lines> log lines of about 100
characters (a file cached under $TMPDIR), one attached 120x40 client in a pty
(output drained), then each command's wall time from the client, in ms:

  capture_S-       capture-pane -p -S -          (live pane, uncompressed)
  capture_S-_e     capture-pane -p -e -S -
  copy-mode        copy-mode
  search-backward  send -X search-backward <absent>   (scans every line)
  search-forward   send -X search-forward <absent>
  history-top, goto-line-mid, history-bottom   each followed by display -p, so
                   the client's redraw of the jump is in the time
  capture_M        capture-pane -p -M -S -       (copy mode's copy)
  alt-on           ?1049h written by the pane until #{alternate_on} is 1

One line per line count on stdout. Works for tmux and vtmux alike. Results:
docs/PERFORMANCE.md, *Copy mode and search*.
"""

import fcntl
import os
import pty
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

ABSENT = "zzznotfound"


def lines_file(n):
    path = os.path.join(tempfile.gettempdir(), f"vtmux-bench-lines-{n}.txt")
    if not os.path.exists(path):
        with open(path + ".tmp", "w") as f:
            for i in range(n):
                f.write(
                    "2026-11-01T12:00:00Z INFO worker[%d] handled request id=%08x "
                    "path=/api/v1/items/%d status=200 bytes=4096 xx\n"
                    % (i % 64, i * 2654435761 % (1 << 32), i)
                )
        os.rename(path + ".tmp", path)
    return path


def run(binary, label, n):
    d = tempfile.mkdtemp(prefix="vbhr-", dir="/tmp")
    sock = os.path.join(d, "s")
    env = {
        "PATH": os.environ["PATH"],
        "HOME": d,
        "TERM": "xterm-256color",
        "SHELL": "/bin/sh",
        "TMUX_TMPDIR": d,
    }

    def t(*args, out=subprocess.PIPE):
        return subprocess.run(
            [binary, "-S", sock, "-f", "/dev/null", *args],
            env=env,
            stdout=out,
            stderr=subprocess.PIPE,
            timeout=120,
        )

    def ms(*args):
        t0 = time.perf_counter()
        r = t(*args, out=subprocess.DEVNULL)
        dt = (time.perf_counter() - t0) * 1000
        assert r.returncode == 0, (args, r.stderr)
        return dt

    client = None
    try:
        f = lines_file(n)
        r = t("set", "-g", "history-limit", str(n + 1000), ";",
              "set", "-g", "status-right", "x", ";",
              "new", "-d", "-s", "a", "-x", "120", "-y", "40",
              "sh", "-c", f"cat {f}; echo DONE; exec cat -")
        assert r.returncode == 0, r.stderr
        deadline = time.time() + 300
        while "DONE" not in t("capture-pane", "-p", "-t", "a").stdout.decode():
            if time.time() > deadline:
                raise SystemExit("timeout waiting for the output")
            time.sleep(0.2)
        client, fd = pty.fork()
        if client == 0:
            os.execve(binary, [binary, "-S", sock, "-f", "/dev/null", "attach", "-t", "a"], env)
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 41, 120, 0, 0))

        def drain():
            try:
                while os.read(fd, 65536):
                    pass
            except OSError:
                pass

        threading.Thread(target=drain, daemon=True).start()
        while b"1" not in t("display", "-p", "#{session_attached}").stdout:
            time.sleep(0.1)
        res = {}
        res["capture_S-"] = ms("capture-pane", "-p", "-S", "-", "-t", "a")
        res["capture_S-_e"] = ms("capture-pane", "-p", "-e", "-S", "-", "-t", "a")
        res["copy-mode"] = ms("copy-mode", "-t", "a")
        res["search-backward"] = ms("send", "-t", "a", "-X", "search-backward", ABSENT)
        res["search-forward"] = ms("send", "-t", "a", "-X", "search-forward", ABSENT)
        for name, cmd in (
            ("history-top", ["history-top"]),
            ("goto-line-mid", ["goto-line", str(n // 2)]),
            ("history-bottom", ["history-bottom"]),
        ):
            res[name] = ms("send", "-t", "a", "-X", *cmd, ";", "display", "-p", "x")
        res["capture_M"] = ms("capture-pane", "-p", "-M", "-S", "-", "-t", "a")
        t("send", "-t", "a", "-X", "cancel")
        t0 = time.perf_counter()
        t("send", "-t", "a", "-l", "\x1b[?1049h", ";", "send", "-t", "a", "Enter")
        while t("display", "-p", "-t", "a", "#{alternate_on}").stdout.strip() != b"1":
            if time.perf_counter() - t0 > 120:
                raise SystemExit("timeout waiting for the alternate screen")
        res["alt-on"] = (time.perf_counter() - t0) * 1000
        hsize = int(t("display", "-p", "-t", "a", "#{history_size}").stdout)
        print(label, n, f"hsize={hsize}", " ".join(f"{k}={v:.0f}" for k, v in res.items()), flush=True)
    finally:
        t("kill-server")
        if client:
            for _ in range(50):
                if os.waitpid(client, os.WNOHANG)[0]:
                    break
                time.sleep(0.1)
            else:
                os.kill(client, 9)
                os.waitpid(client, 0)
        shutil.rmtree(d, ignore_errors=True)


def main():
    if len(sys.argv) < 4:
        raise SystemExit(__doc__)
    binary, label = sys.argv[1], sys.argv[2]
    for n in map(int, sys.argv[3:]):
        run(binary, label, n)


main()
