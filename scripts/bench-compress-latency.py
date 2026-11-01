#!/usr/bin/env python3
"""Busy-pane output latency while other panes compress their history.

The threading gate of docs/specs/2026-10-31-history-memory.md (task
hist_compress): a client is attached over a pty to window 0, whose pane
runs `sleep`. A marker (one character at the top left, a different one each time) is
written straight to that pane's PTY slave (`#{pane_tty}`), and the time
until it shows up in the client's output is the latency. Windows 1..N each fill a pane with SCENARIO's history and
go idle, so their compression starts IDLE (5 s) after their output stops.
Server RSS is polled meanwhile: the window where it falls is when
compression ran. Run once per binary (with and without compression) and
compare the p95 of the samples inside that window.

Usage: [GAP_MS=2] bench-compress-latency.py BINARY SCENARIO PANES [SECONDS] > run.json
       bench-compress-latency.py compare WITH.json... -- WITHOUT.json...
  SCENARIO: spec (120x40, limit 100000) or narrow (80x24, limit 10000,
  truecolour cells), the generators of scripts/bench-history-memory.sh.
  GAP_MS: the pause between one marker showing and the next written
  (2: a pane busy without pause; 20: typing pace, with quiet gaps).
Prints one JSON line: samples, RSS timeline and the latencies. `compare`
takes runs of the binary that compresses and of one that doesn't: the
window is where each WITH run's RSS fell by more than 1 MB from its
level at the fill (10 ms polls) to where it stopped falling. Samples
started inside it are compared with WITHOUT samples started in the same
seconds after the fill, and with WITH samples from before it.
"""

import json
import os
import pty
import select
import subprocess
import sys
import tempfile
import threading
import time

SCENARIOS = {
    "spec": (120, 40, 100000, r'''BEGIN { for (i = 0; i < 150000; i++)
    printf "2026-10-31T12:%02d:%02d.%03dZ INFO  worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=%d\n",
      (i / 60000) % 60, (i / 1000) % 60, i % 1000, i % 16, i * 2654435761 % 4294967296, i, (i * 37) % 100000 }'''),
    "narrow": (80, 24, 10000, r'''BEGIN { for (i = 0; i < 12000; i++) { s = sprintf("%05d", i)
    for (j = 0; j < 70; j++) s = s sprintf("\033[38;2;%d;%d;%dm%c", i % 256, j, (i * 7) % 256, 65 + j % 26)
    print s "\033[m" } }'''),
}


def p(xs, q):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(len(xs) * q))] * 1e3 if xs else float("nan")


def window(run):
    rss = run["rss"]
    top = rss[0][1]
    falling = [t for t, kb in rss if kb < top - 1024]
    if not falling:
        return None
    floor = min(kb for _, kb in rss)
    end = next(t for t, kb in rss if kb <= floor + 256)
    return falling[0], end


def compare(args):
    split = args.index("--")
    with_runs = [json.load(open(f)) for f in args[:split]]
    without = [json.load(open(f)) for f in args[split + 1:]]
    inside, before, base = [], [], []
    windows = []
    for run in with_runs:
        w = window(run)
        if w is None:
            continue
        windows.append(w)
        inside += [lat for t, lat in run["samples"] if w[0] <= t <= w[1]]
        before += [lat for t, lat in run["samples"] if 0.5 <= t < w[0] - 0.5]
    for w, run in zip(windows * len(without), without):
        base += [lat for t, lat in run["samples"] if w[0] <= t <= w[1]]
    width = sum(e - s for s, e in windows) / max(1, len(windows))
    print(f"windows {len(windows)} mean {width:.2f}s")
    for name, xs in (("compressing", inside), ("before", before), ("no compression", base)):
        print(f"{name:15} n={len(xs):5} p50={p(xs, .5):.3f} p95={p(xs, .95):.3f}"
              f" p99={p(xs, .99):.3f} max={p(xs, 1):.3f} ms")
    rise = p(inside, .95) / p(base, .95) - 1
    print(f"p95 rise against no compression: {rise * 100:+.1f}%")


def main():
    if sys.argv[1] == "compare":
        compare(sys.argv[2:])
        return
    binary, scenario, panes = sys.argv[1], sys.argv[2], int(sys.argv[3])
    seconds = float(sys.argv[4]) if len(sys.argv) > 4 else 12.0
    gap = float(os.environ.get("GAP_MS", "2")) / 1e3
    cols, rows, limit, prog = SCENARIOS[scenario]
    work = tempfile.mkdtemp(prefix="vtmux-lat-")
    data = os.path.join(work, "h.txt")
    with open(data, "w") as f:
        subprocess.run(["awk", prog], stdout=f, check=True)
    env = {k: v for k, v in os.environ.items()
           if k not in ("TMUX", "TMUX_PANE", "EDITOR", "VISUAL")}
    env.update(SHELL="/bin/sh", TMUX_TMPDIR=work, TERM="xterm-256color")
    label = f"lat{os.getpid()}"
    base = [binary, "-L", label, "-f", "/dev/null"]

    def t(*args):
        return subprocess.run(base + list(args), env=env, check=True,
                              capture_output=True, text=True).stdout

    t("new", "-d", "-s", "s", "-x", str(cols), "-y", str(rows), "exec sleep 999999",
      ";", "set", "-g", "history-limit", str(limit), ";", "set", "-g", "status", "off")
    # The client attaches before the history panes exist: its frames are
    # window 0's.
    pid, master = pty.fork()
    if pid == 0:
        os.execvpe(binary, base + ["attach", "-t", "s:0"], env)
    import fcntl
    import struct
    import termios
    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    out = bytearray()

    def pump(timeout):
        r, _, _ = select.select([master], [], [], timeout)
        if r:
            try:
                out.extend(os.read(master, 65536))
            except OSError:
                pass

    deadline = time.monotonic() + 5
    while t("list-clients").strip() == "" and time.monotonic() < deadline:
        pump(0.05)
    tty = t("display", "-p", "-t", "s:0", "#{pane_tty}").strip()
    server = int(t("display", "-p", "#{pid}").strip())
    for _ in range(panes):
        t("new-window", "-d", "-t", "s:", f"cat '{data}'; echo DONE; exec sleep 999999")
    # Fill: wait for every history pane to finish (visible rows only, so
    # no history is read).
    windows = [w for w in t("list-windows", "-t", "s", "-F", "#{window_index}").split() if w != "0"]
    while True:
        pump(0.05)
        if all("DONE" in t("capture-pane", "-p", "-t", f"s:{w}") for w in windows):
            break
    filled = time.monotonic()

    rss = []
    stop = threading.Event()

    def poll_rss():
        while not stop.is_set():
            with open(f"/proc/{server}/status") as f:
                kb = next(int(l.split()[1]) for l in f if l.startswith("VmRSS:"))
            rss.append((time.monotonic() - filled, kb))
            time.sleep(0.01)

    th = threading.Thread(target=poll_rss)
    th.start()
    samples = []
    fd = os.open(tty, os.O_WRONLY | os.O_NOCTTY)
    seq = 0
    while time.monotonic() - filled < seconds:
        seq += 1
        # The client is sent cells one at a time, so a marker is one
        # character, put where the last one was: the frame holds it.
        marker = bytes([ord("A") + seq % 26])
        out.clear()
        start = time.monotonic()
        os.write(fd, b"\x1b[H" + marker)
        while marker not in out:
            pump(1.0)
            if time.monotonic() - start > 5:
                break
        samples.append((start - filled, time.monotonic() - start))
        time.sleep(gap)
    stop.set()
    th.join()
    os.close(fd)
    t("kill-server")
    os.waitpid(pid, 0)
    subprocess.run(["rm", "-rf", work])
    print(json.dumps({"binary": binary, "scenario": scenario, "panes": panes,
                      "filled": filled, "rss": rss, "samples": samples}))


if __name__ == "__main__":
    main()
