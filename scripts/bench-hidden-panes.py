#!/usr/bin/env python3
"""Hidden-pane memory and window-switch latency (docs/PERFORMANCE.md, *Hidden panes*).

    python3 scripts/bench-hidden-panes.py <binary> <-L label> [windows=100]

Starts a server on `-L label` (killed at the end), attaches a 100x51 client on
a PTY, opens `windows` more windows whose pane fills its 100x50 screen, then
prints the server's RSS before and after and the median time from `C-b n` to
the client's last output byte over 40 switches. Linux only (/proc).
"""
import os, pty, fcntl, termios, struct, subprocess, threading, time, sys, statistics
B = sys.argv[1]
L = sys.argv[2]
N = int(sys.argv[3]) if len(sys.argv) > 3 else 100
env = dict(os.environ); env.pop("TMUX", None)
def t(*a): return subprocess.run([B, "-L", L, *a], env=env, capture_output=True, text=True)
FILL = "sh -c 'i=0; while [ $i -lt 60 ]; do echo \"$i 0123456789abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789ab\"; i=$((i+1)); done; exec cat'"
try:
    t("-f", "/dev/null", "new", "-d", "-s", "a", "-x", "100", "-y", "51", FILL)
    t("set", "-g", "status-right", "R"); t("set", "-g", "status-interval", "0")
    pid = int(t("display", "-p", "#{pid}").stdout)
    m, s = pty.openpty()
    fcntl.ioctl(s, termios.TIOCSWINSZ, struct.pack("HHHH", 51, 100, 0, 0))
    p = subprocess.Popen([B, "-L", L, "attach", "-t", "a"], stdin=s, stdout=s, stderr=s, env=env, start_new_session=True)
    os.close(s)
    st = {"n": 0, "last": time.monotonic()}
    def drain():
        while True:
            try: b = os.read(m, 65536)
            except OSError: return
            if not b: return
            st["n"] += len(b); st["last"] = time.monotonic()
    threading.Thread(target=drain, daemon=True).start()
    def quiet(secs=0.3):
        while time.monotonic() - st["last"] < secs: time.sleep(0.02)
    quiet()
    def rss():
        for l in open(f"/proc/{pid}/status"):
            if l.startswith("VmRSS"): return int(l.split()[1])
    r0 = rss()
    for i in range(N):
        t("neww", "-t", "a", FILL)
        quiet(0.05)
    t("selectw", "-t", "a:0"); quiet(0.5)
    r1 = rss()
    lat = []
    for i in range(1, 41):
        quiet(0.1)
        t0 = time.monotonic()
        os.write(m, b"\x02n")
        time.sleep(0.15)
        quiet(0.1)
        lat.append((st["last"] - t0) * 1000)
    print(f"{os.path.basename(B)} panes={N+1} rss_before={r0} rss_after={r1} per_hidden_kb={(r1-r0)/N:.1f} switch_ms median={statistics.median(lat):.2f} min={min(lat):.2f} max={max(lat):.2f}")
finally:
    t("kill-server")
