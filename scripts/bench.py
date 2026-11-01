#!/usr/bin/env python3
"""Memory and CPU of the vtmux server against tmux 3.8 (docs/PERFORMANCE.md).

Usage: mise exec -- python3 scripts/bench.py [scenario...]
Scenarios: idle scale history startup throughput (default: all).

Every server runs on a private socket (-L plus TMUX_TMPDIR=<tempdir>, -f /dev/null);
its pid comes from `display -p '#{pid}'` (never pgrep) and is checked gone after
kill-server. Only the server process is measured, never its pane children:
  Linux  CPU     /proc/<pid>/stat utime+stime (fields 14+15) deltas per phase; idle uses
                 the nanosecond /proc/<pid>/task/*/schedstat sum when present
         RSS     settled VmRSS; per-phase peak VmHWM after `echo 5 > clear_refs`
         wakeups voluntary_ctxt_switches deltas (all threads)
  macOS  CPU/RSS `ps -o rss=,time=` sampled every 100 ms (peak = max sample)
Medians over RUNS runs. A Markdown table goes to stdout, a JSON file to $OUT.json.

Env: RUNS (3), IDLE_SECS (60), VTMUX (target/release/vtmux), VTMUX_REFERENCE_TMUX (the
`cargo xtask ref-tmux` cache), BASE (another vtmux binary, shown as `base`), OUT
(default target/bench/bench-<utc>), NO_BUILD=1 (skip cargo build), STARTUP_N (100),
THROUGHPUT_MB (100), HISTORY_SCENARIOS ("spec narrow wide"). Run cargo through `mise exec --`.
"""
import json
import os
import platform
import random
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time
from datetime import datetime, timezone

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LINUX = sys.platform.startswith("linux")
RUNS = int(os.environ.get("RUNS", "3"))
IDLE_SECS = float(os.environ.get("IDLE_SECS", "60"))
STARTUP_N = int(os.environ.get("STARTUP_N", "100"))
THROUGHPUT_MB = int(os.environ.get("THROUGHPUT_MB", "100"))
CLK_TCK = os.sysconf("SC_CLK_TCK")
ALL = ["idle", "scale", "history", "startup", "throughput"]


class Skip(Exception):
    pass


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


# ---- process stats ---------------------------------------------------------

def _ps(pid):
    """(rss_kb, cpu_ms) from ps; the `time` column is the process's own CPU."""
    out = subprocess.run(["ps", "-o", "rss=,time=", "-p", str(pid)], capture_output=True, text=True).stdout.split()
    if len(out) != 2:
        return None
    t = out[1]
    secs = 0.0
    for part in t.replace("-", ":").split(":"):
        secs = secs * 60 + float(part)
    return int(out[0]), secs * 1000


def _proc_status(pid, key):
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            if line.startswith(key + ":"):
                return int(line.split()[1])
    return 0


def rss_kb(pid):
    return _proc_status(pid, "VmRSS") if LINUX else _ps(pid)[0]


def cpu_ms(pid, fine=False):
    if LINUX:
        if fine:
            try:
                ns = 0
                for tid in os.listdir(f"/proc/{pid}/task"):
                    with open(f"/proc/{pid}/task/{tid}/schedstat") as f:
                        ns += int(f.read().split()[0])
                return ns / 1e6
            except OSError:
                pass
        with open(f"/proc/{pid}/stat") as f:
            fields = f.read().rsplit(")", 1)[1].split()  # fields[0] is field 3 (state)
        return (int(fields[11]) + int(fields[12])) * 1000 / CLK_TCK  # fields 14 + 15
    return _ps(pid)[1]


def vol_switches(pid):
    if not LINUX:
        return None
    return sum(_proc_status(f"{pid}/task/{t}", "voluntary_ctxt_switches") for t in os.listdir(f"/proc/{pid}/task"))


class Phase:
    """CPU, wakeups and peak RSS of one server pid over a with-block."""

    def __init__(self, pid, fine=False):
        self.pid, self.fine = pid, fine

    def __enter__(self):
        self.peak = 0
        if LINUX:
            with open(f"/proc/{self.pid}/clear_refs", "w") as f:
                f.write("5")
        else:
            self._stop = threading.Event()

            def sample():
                while not self._stop.is_set():
                    s = _ps(self.pid)
                    if s:
                        self.peak = max(self.peak, s[0])
                    self._stop.wait(0.1)

            self._th = threading.Thread(target=sample)
            self._th.start()
        self.t0, self.c0, self.v0 = time.monotonic(), cpu_ms(self.pid, self.fine), vol_switches(self.pid)
        return self

    def __exit__(self, *exc):
        self.cpu = cpu_ms(self.pid, self.fine) - self.c0
        self.wall = time.monotonic() - self.t0
        v1 = vol_switches(self.pid)
        self.wakeups = None if v1 is None else v1 - self.v0
        if LINUX:
            self.peak = _proc_status(self.pid, "VmHWM")
        else:
            self._stop.set()
            self._th.join()
        return False


# ---- servers ----------------------------------------------------------------

class Server:
    def __init__(self, binary, work, label):
        self.bin, self.label = binary, label
        self.env = {k: v for k, v in os.environ.items() if k not in ("TMUX", "TMUX_PANE", "EDITOR", "VISUAL")}
        self.env.update(TMUX_TMPDIR=work, SHELL="/bin/sh", TERM="xterm-256color",
                        LANG="C.UTF-8" if LINUX else "en_US.UTF-8")
        self.pid = None

    def run(self, *args, check=True):
        r = subprocess.run([self.bin, "-L", self.label, "-f", "/dev/null", *args],
                           env=self.env, capture_output=True, text=True, timeout=120)
        if check and r.returncode:
            raise RuntimeError(f"{self.bin} {' '.join(args[:6])}: {r.stderr.strip()}")
        return r.stdout

    def find_pid(self):
        self.pid = int(self.run("display", "-p", "#{pid}"))
        return self.pid

    def kill(self):
        self.run("kill-server", check=False)
        deadline = time.monotonic() + 10
        while self.pid and time.monotonic() < deadline:
            try:
                os.kill(self.pid, 0)
            except ProcessLookupError:
                return
            time.sleep(0.02)
        if self.pid:
            raise RuntimeError(f"server pid {self.pid} still alive after kill-server ({self.bin})")


class Env:
    def __init__(self, bins):
        self.bins = bins
        self.work = tempfile.mkdtemp(prefix="vtmux-bench-")
        self.n = 0

    def server(self, which):
        self.n += 1
        return Server(self.bins[which], self.work, f"b{os.getpid()}x{self.n}")

    def per_binary(self, fn):
        """fn(which) -> metrics dict, RUNS times each; the median of every metric."""
        out = {}
        for which in self.bins:
            runs = [fn(which) for _ in range(RUNS)]
            out[which] = {k: med([r[k] for r in runs]) for k in runs[0]}
        return out


# ---- scenarios --------------------------------------------------------------

def scenario_idle(env):
    def one(which):
        s = env.server(which)
        s.run("new", "-d", "-s", "idle", "cat")
        s.find_pid()
        try:
            time.sleep(2)  # settle
            with Phase(s.pid, fine=True) as p:
                time.sleep(IDLE_SECS)
            return {"rss_mb": rss_kb(s.pid) / 1024, "cpu_ms": p.cpu, "wakeups_per_s": None if p.wakeups is None else p.wakeups / p.wall}
        finally:
            s.kill()
    return {"title": f"idle (1 session, {IDLE_SECS:g} s, -f /dev/null)", "results": env.per_binary(one)}


def scenario_scale(env):
    if not LINUX:
        try:
            soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
            resource.setrlimit(resource.RLIMIT_NOFILE, (min(4096, hard) if hard != resource.RLIM_INFINITY else 4096, hard))
        except (ValueError, OSError) as e:
            raise Skip(f"cannot raise ulimit -n to 4096 on macOS: {e}")
    sessions, windows, splits = 10, 10, 3

    def one(which):
        s = env.server(which)
        s.run("new", "-d", "-s", "s0", "-x", "200", "-y", "50", "cat")
        s.find_pid()
        try:
            time.sleep(1)
            base = rss_kb(s.pid)
            with Phase(s.pid) as p:
                for i in range(sessions):
                    cmd = []  # one invocation per session: tmux caps a command line's length
                    if i:
                        cmd += ["new", "-d", "-s", f"s{i}", "-x", "200", "-y", "50", "cat", ";"]
                    for _ in range(windows - 1):
                        cmd += ["neww", "-d", "-t", f"s{i}:", "cat", ";"]
                    for w in range(windows):
                        for _ in range(splits):
                            cmd += ["splitw", "-d", "-t", f"s{i}:{w}", "cat", ";"]
                    s.run(*cmd[:-1])
            panes = len(s.run("list-panes", "-a", "-F", "x").split())
            if panes != sessions * windows * (splits + 1):
                raise RuntimeError(f"scale: {panes} panes, expected {sessions * windows * (splits + 1)}")
            time.sleep(3)
            rss = rss_kb(s.pid)
            return {"panes": panes, "rss_mb": rss / 1024, "rss_kb_per_pane": (rss - base) / (panes - 1),
                    "create_cpu_ms": p.cpu, "create_wall_s": p.wall, "peak_rss_mb": p.peak / 1024}
        finally:
            s.kill()
    return {"title": "scale (10 sessions x 10 windows x 4 panes of idle cat)", "results": env.per_binary(one)}


def scenario_history(env):
    script = os.path.join(ROOT, "scripts", "bench-history-memory.sh")
    scen = os.environ.get("HISTORY_SCENARIOS", "spec narrow wide").split()
    e = dict(os.environ, IDLE=str(int(min(10, IDLE_SECS))), RUNS=str(RUNS))
    e["VTMUX"], e["VTMUX_REFERENCE_TMUX"] = env.bins["vtmux"], env.bins["tmux"]
    if "base" in env.bins:
        e["BASE"] = env.bins["base"]
    r = subprocess.run(["bash", script, *scen], env=e, capture_output=True, text=True, cwd=ROOT)
    if r.returncode:
        raise RuntimeError(f"bench-history-memory.sh: {r.stderr.strip()}")
    lines = r.stdout.strip().splitlines()
    results = {}
    for line in lines[1:]:
        f = line.split()
        results.setdefault(f[0], {})[f[1]] = {"size": f[2], "limit": int(f[3]), "rss_mb": float(f[4]),
                                              "hist_size": int(f[5]), "retained": f[6], "output_s": float(f[7])}
    return {"title": "history (bench-history-memory.sh, fastest of RUNS)", "raw": r.stdout, "history": results}


def scenario_startup(env):
    def one(which):
        new, kill, rss = [], [], []
        for _ in range(STARTUP_N):
            s = env.server(which)
            t0 = time.monotonic()
            s.run("new", "-d", "-s", "x")
            t1 = time.monotonic()
            s.find_pid()
            rss.append(rss_kb(s.pid))
            t2 = time.monotonic()
            s.kill()
            t3 = time.monotonic()
            new.append((t1 - t0) * 1000)
            kill.append((t3 - t2) * 1000)
        return {"new_ms": med(new), "kill_ms": med(kill), "rss_mb": med(rss) / 1024}
    return {"title": f"startup (new -d + kill-server, {STARTUP_N}x, -f /dev/null)", "results": env.per_binary(one)}


def gen_log(path, mb, sgr):
    """Log text of ~100-char lines in a 1 MB block repeated; with sgr, ~20% of the bytes are SGR."""
    rnd = random.Random(1)
    words = ["INFO", "WARN", "worker", "handled", "request", "path=/api/v1/items", "status=200", "bytes=4096", "cache", "miss", "id"]
    block, size, plain, esc = [], 0, 0, 0
    while size < 1 << 20:
        parts = [f"2026-11-01T12:{rnd.randrange(60):02d}:{rnd.randrange(60):02d}.{rnd.randrange(1000):03d}Z"]
        parts += [rnd.choice(words) + str(rnd.randrange(1000)) for _ in range(9)]
        out = []
        for w in parts:
            if sgr and rnd.random() < 0.2:
                a, b = f"\x1b[38;5;{rnd.randrange(16, 232)}m", "\x1b[0m"
                out.append(a + w + b)
                esc += len(a) + len(b)
            else:
                out.append(w)
            plain += len(w) + 1
        line = " ".join(out) + "\r\n"
        block.append(line)
        size += len(line)
    data = "".join(block).encode()
    data = data[: data.rindex(b"\n") + 1]  # whole lines only: the done marker must start a line
    with open(path, "wb") as f:
        for _ in range(mb):
            f.write(data)
    return esc / (esc + plain)


def scenario_throughput(env):
    files, frac = {}, {}
    for kind, sgr in (("plain", False), ("sgr", True)):
        files[kind] = os.path.join(env.work, f"log-{kind}.txt")
        frac[kind] = gen_log(files[kind], THROUGHPUT_MB, sgr)
    results = {}
    for kind in files:
        def one(which, kind=kind):
            size = os.path.getsize(files[kind]) / 1e6
            s = env.server(which)
            s.run("new", "-d", "-s", "keep", "cat")
            s.find_pid()
            try:
                time.sleep(1)
                with Phase(s.pid) as p:
                    s.run("new", "-d", "-s", "t", "-x", "120", "-y", "40",
                          f"cat '{files[kind]}'; printf 'BENCH-%s\\n' DONE; exec cat")
                    deadline = time.monotonic() + 600
                    while "\nBENCH-DONE\n" not in "\n" + s.run("capture-pane", "-p", "-J", "-t", "t"):
                        if time.monotonic() > deadline:
                            raise RuntimeError("throughput: done marker not seen in 600 s")
                        time.sleep(0.2)
                return {"wall_s": p.wall, "cpu_s": p.cpu / 1000, "mb_per_s": size / p.wall, "peak_rss_mb": p.peak / 1024}
            finally:
                s.kill()
        results[kind] = env.per_binary(one)
    return {"title": f"throughput without a client ({THROUGHPUT_MB} MB log text, 120x40 pane, default history; sgr bytes = {frac['sgr']:.0%})",
            "throughput": results}


# ---- output -----------------------------------------------------------------

def fmt(v, nd=1):
    return "-" if v is None else f"{v:.{nd}f}"


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out)


def render(name, res):
    out = [f"### {res['title']}", ""]
    if name == "idle":
        rows = [[b, fmt(m["rss_mb"]), fmt(m["cpu_ms"], 2), fmt(m["wakeups_per_s"], 2)] for b, m in res["results"].items()]
        out.append(table(["binary", "RSS MB", "CPU ms", "wakeups/s"], rows))
    elif name == "scale":
        rows = [[b, fmt(m["rss_mb"]), fmt(m["rss_kb_per_pane"]), fmt(m["peak_rss_mb"]), fmt(m["create_cpu_ms"], 0), fmt(m["create_wall_s"], 2)]
                for b, m in res["results"].items()]
        out.append(table(["binary", "RSS MB (400 panes)", "RSS KB/pane", "peak RSS MB", "create CPU ms", "create wall s"], rows))
    elif name == "history":
        out.append("```\n" + res["raw"].rstrip() + "\n```")
    elif name == "startup":
        rows = [[b, fmt(m["new_ms"]), fmt(m["kill_ms"]), fmt(m["rss_mb"])] for b, m in res["results"].items()]
        out.append(table(["binary", "new -d ms", "kill-server ms", "RSS MB"], rows))
    elif name == "throughput":
        rows = [[k, b, fmt(m["wall_s"], 2), fmt(m["cpu_s"], 2), fmt(m["mb_per_s"]), fmt(m["peak_rss_mb"])]
                for k, per in res["throughput"].items() for b, m in per.items()]
        out.append(table(["input", "binary", "wall s", "server CPU s", "MB/s", "peak RSS MB"], rows))
    return "\n".join(out) + "\n"


def sh(*cmd):
    try:
        return subprocess.run(cmd, capture_output=True, text=True, cwd=ROOT).stdout.strip()
    except OSError:
        return ""


def meta(bins):
    if LINUX:
        cpu = next((l.split(":", 1)[1].strip() for l in open("/proc/cpuinfo") if l.startswith("model name")), "?")
    else:
        cpu = sh("sysctl", "-n", "machinebrand_string") or sh("sysctl", "-n", "machdep.cpu.brand_string")
    dirty = "+dirty" if sh("git", "status", "--porcelain", "--untracked-files=no") else ""
    return {
        "date": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "commit": sh("git", "rev-parse", "--short", "HEAD") + dirty,
        "host": platform.platform(), "cpu": cpu, "cpus": os.cpu_count(), "kernel": platform.release(),
        "load_at_start": os.getloadavg()[0], "runs": RUNS,
        "versions": {k: subprocess.run([b, "-V"], capture_output=True, text=True).stdout.strip() for k, b in bins.items()},
    }


def main():
    scenarios = sys.argv[1:] or ALL
    for s in scenarios:
        if s not in ALL:
            sys.exit(f"unknown scenario {s} (one of: {' '.join(ALL)})")
    if not os.environ.get("NO_BUILD"):
        subprocess.run(["cargo", "build", "--release", "-q", "-p", "vtmux", "--bin", "vtmux"], cwd=ROOT, check=True)
        if "history" in scenarios:
            subprocess.run(["cargo", "build", "--release", "-q", "-p", "vtmux-vt", "--example", "history_retention"], cwd=ROOT, check=True)
    ref = os.environ.get("VTMUX_REFERENCE_TMUX") or os.path.join(
        os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "vtmux/tmux-3.8/bin/tmux")
    if not os.access(ref, os.X_OK):
        sys.exit(f"no reference tmux at {ref} (cargo xtask ref-tmux)")
    bins = {"tmux": ref}
    if os.environ.get("BASE"):
        bins["base"] = os.path.abspath(os.environ["BASE"])
    bins["vtmux"] = os.path.abspath(os.environ.get("VTMUX") or os.path.join(ROOT, "target/release/vtmux"))
    m = meta(bins)
    if m["load_at_start"] >= 4:
        print(f"warning: load average {m['load_at_start']:.1f} >= 4; numbers are noisy", file=sys.stderr)
    env = Env(bins)
    funcs = {"idle": scenario_idle, "scale": scenario_scale, "history": scenario_history,
             "startup": scenario_startup, "throughput": scenario_throughput}
    results, md = {}, []
    try:
        for name in scenarios:
            print(f"== {name}", file=sys.stderr, flush=True)
            try:
                results[name] = funcs[name](env)
            except Skip as e:
                print(f"skip {name}: {e}", file=sys.stderr)
                results[name] = {"title": name, "skipped": str(e)}
                md.append(f"### {name}\n\nSkipped: {e}\n")
                continue
            md.append(render(name, results[name]))
    finally:
        shutil.rmtree(env.work, ignore_errors=True)
    head = (f"{m['date']}, commit {m['commit']}, {m['host']}, {m['cpu']} ({m['cpus']} cpus), load {m['load_at_start']:.1f}, "
            f"median of {RUNS}; " + "; ".join(f"{k}: {v}" for k, v in m["versions"].items()) + "\n")
    text = head + "\n" + "\n".join(md)
    print(text)
    out = os.environ.get("OUT") or os.path.join(ROOT, "target/bench", "bench-" + datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S"))
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out + ".json", "w") as f:
        json.dump({"meta": m, "scenarios": results}, f, indent=2)
    with open(out + ".md", "w") as f:
        f.write(text)
    print(f"wrote {out}.json and {out}.md", file=sys.stderr)


if __name__ == "__main__":
    main()
