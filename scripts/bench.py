#!/usr/bin/env python3
"""Memory and CPU of the vtmux server against tmux 3.8 (docs/PERFORMANCE.md).

Usage: mise exec -- python3 scripts/bench.py [scenario...]
       mise exec -- python3 scripts/bench.py --smoke    regression guard (scripts/bench_smoke.py)
Scenarios: idle scale history startup throughput owner_idle owner_startup throughput_attached render many_clients
           agent_panes key_echo upgrade (default: all), and trickle (by name only: about 22 min).

Every server runs on a private socket (-L plus TMUX_TMPDIR=<tempdir>, -f /dev/null);
its pid comes from `display -p '#{pid}'` (never pgrep) and is checked gone after
kill-server. Only the server process is measured, never its pane children:
  Linux  CPU     /proc/<pid>/stat utime+stime (fields 14+15) deltas per phase; idle and
                 the attached scenarios use the nanosecond /proc/<pid>/task/*/schedstat
                 sum when present
         RSS     settled VmRSS; per-phase peak VmHWM after `echo 5 > clear_refs`
         wakeups voluntary_ctxt_switches deltas (all threads)
  macOS  CPU/RSS `ps -o rss=,time=` sampled every 100 ms (peak = max sample)
Medians over RUNS runs. A Markdown table goes to stdout, a JSON file to $OUT.json.

Env: RUNS (3), IDLE_SECS (60), VTMUX (target/release/vtmux), VTMUX_REFERENCE_TMUX (the
`cargo xtask ref-tmux` cache), BASE (another vtmux binary, shown as `base`), OUT
(default target/bench/bench-<utc>), NO_BUILD=1 (skip cargo build), STARTUP_N (100),
THROUGHPUT_MB (100), OWNER_STARTUP_N (20), OWNER_TMUX_CONF / OWNER_PLUGINS / OWNER_MUCREW (the
owner's config, plugin and mu-crew dirs that owner_* copy; default ~/.tmux.conf, ~/.tmux/plugins,
~/.local/share/mu-crew-dotfiles), HISTORY_SCENARIOS ("spec narrow wide"), HISTORY_TIMEOUT (3600 s), TRICKLE_RATES ("10 1000 fill": lines/s
per case; `fill` is 150k lines at once), TRICKLE_SECS (1200), TRICKLE_IDLE_SECS (120), TRICKLE_SAMPLE (10 s),
TRICKLE_LIMIT (100000), TRICKLE_PANES ("1": streaming panes per case, one window each; "1 5 20" runs each count), TRICKLE_PREFILL (1: every rate case first writes the 150k lines of `fill`, so the history starts at
the limit; 0: the trickle alone), BLEND=1 (the attached scenarios
with `vtmux-pane-inactive-blend 'fg=bg/25,bg=bg/25'` set and the reader answering OSC 10/11, D26;
tmux ignores the option), AGENT_PANES ("5 10 20"),
AGENT_CLIENTS ("1 2"), AGENT_SECS (10), KEY_ECHO_N (200), UPGRADE_PANES ("1 10"), UPGRADE_LINES ("10000 100000"),
UPGRADE_IDLE_LIMIT (180 s: the longest `idle` waits for the compressor), UPGRADE_STATES ("fresh idle"; add
"fresh-rows idle-rows" for an upgrade onto a binary that reports another Ghostty patch stack, so the history
goes as rows; a state with "-alt", as "fresh-alt-rows", has an alternate screen up over the history),
UPGRADE_LIMIT (100000: the panes' history-limit). Run cargo through `mise exec --`.
"""
import ctypes
import fcntl
import hashlib
import json
import os
import platform
import pty
import random
import re
import resource
import signal
import shutil
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
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


class _RUsageInfoV0(ctypes.Structure):
    _fields_ = [("uuid", ctypes.c_uint8 * 16)] + [(n, ctypes.c_uint64) for n in (
        "user_time", "system_time", "pkg_idle_wkups", "interrupt_wkups", "pageins", "wired_size",
        "resident_size", "phys_footprint", "proc_start_abstime", "proc_exit_abstime")]


def footprint_kb(pid):
    """macOS only: the physical footprint (what Activity Monitor calls Memory), from proc_pid_rusage.
    Darwin's resident size keeps counting pages released with MADV_FREE_REUSABLE (how Ghostty gives
    compressed history pages back, mem.zig) until the kernel reuses them; the footprint drops at once.
    On Linux, VmRSS already drops (MADV_DONTNEED): None."""
    if LINUX:
        return None
    r = _RUsageInfoV0()
    libc = ctypes.CDLL("/usr/lib/libSystem.B.dylib")
    if libc.proc_pid_rusage(int(pid), 0, ctypes.byref(r)) != 0:
        return None
    return r.phys_footprint // 1024


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

    def run(self, *args, check=True, full=False):
        r = subprocess.run([self.bin, "-L", self.label, "-f", "/dev/null", *args],
                           env=self.env, capture_output=True, text=True, timeout=120)
        if check and r.returncode:
            raise RuntimeError(f"{self.bin} {' '.join(args[:6])}: {r.stderr.strip()}")
        return r if full else r.stdout

    def find_pid(self):
        self.pid = int(self.run("display", "-p", "#{pid}"))
        return self.pid

    def kill(self):
        """kill-server, then wait for the pid to go; returns (ms until kill-server
        returned, ms from then until the pid was gone), the pid polled every 0.5 ms."""
        t0 = time.monotonic()
        self.run("kill-server", check=False)
        t1 = time.monotonic()
        deadline = t1 + 10
        while self.pid and time.monotonic() < deadline:
            try:
                os.kill(self.pid, 0)
            except ProcessLookupError:
                return (t1 - t0) * 1000, (time.monotonic() - t1) * 1000
            time.sleep(0.0005)
        if self.pid:
            raise RuntimeError(f"server pid {self.pid} still alive after kill-server ({self.bin})")
        return (t1 - t0) * 1000, 0.0


class Env:
    def __init__(self, bins):
        self.bins = bins
        self.work = tempfile.mkdtemp(prefix="vtmux-bench-")
        self.n = 0

    def next(self):
        self.n += 1
        return self.n

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
    try:  # the script has its own per-run deadline; this bounds the whole scenario
        r = subprocess.run(["bash", script, *scen], env=e, capture_output=True, text=True, cwd=ROOT,
                           timeout=float(os.environ.get("HISTORY_TIMEOUT", "3600")))
    except subprocess.TimeoutExpired:
        raise RuntimeError("bench-history-memory.sh: no result within HISTORY_TIMEOUT s")
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
        new, kill, gone, rss = [], [], [], []
        for _ in range(STARTUP_N):
            s = env.server(which)
            t0 = time.monotonic()
            s.run("new", "-d", "-s", "x")
            t1 = time.monotonic()
            s.find_pid()
            rss.append(rss_kb(s.pid))
            ret_ms, gone_ms = s.kill()
            new.append((t1 - t0) * 1000)
            kill.append(ret_ms)
            gone.append(ret_ms + gone_ms)
        return {"new_ms": med(new), "kill_ms": med(kill), "gone_ms": med(gone), "rss_mb": med(rss) / 1024}
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


# ---- owner config scenarios (owner_idle, owner_startup) --------------------------
# The owner's real ~/.tmux.conf, TPM plugins and mu-crew-dotfiles, copied into a hermetic
# $HOME per run (HOME=<work>/<label>/home) so nothing touches ~ or the owner's own server.
# The binary under test is symlinked as <work>/<label>/bin/tmux, first on PATH, because the
# config, TPM and the poller all run `tmux` from PATH (vtmux run as tmux: D20).
# Changes to the copy: @fingers-skip-wizard 1 on top (no install wizard) and
# @mu_crew_probes "uptime" (the cheapest probe: a /proc read every 60 s, no subprocess; the
# owner's real list is "murmur_remote memory cpu uptime muzzle").

OWNER_PROBES = "uptime"
OWNER_STARTUP_N = int(os.environ.get("OWNER_STARTUP_N", "20"))


def _git_rev(path):
    rev = sh("git", "-C", path, "rev-parse", "--short", "HEAD") or "no git"
    return rev + ("+dirty" if sh("git", "-C", path, "status", "--porcelain", "--untracked-files=no") else "")


def owner_sources():
    """Where the owner's files are, and the revisions recorded in the output."""
    if not LINUX:
        raise Skip("the owner config scenarios run on Linux only (the poller, TPM and paths are the Linux ones)")
    home = os.path.expanduser("~")
    conf = os.path.realpath(os.environ.get("OWNER_TMUX_CONF") or os.path.join(home, ".tmux.conf"))
    src = {"conf": conf,
           "plugins": os.environ.get("OWNER_PLUGINS") or os.path.join(home, ".tmux/plugins"),
           "mucrew": os.environ.get("OWNER_MUCREW") or os.path.join(home, ".local/share/mu-crew-dotfiles")}
    for k, v in src.items():
        if not os.path.exists(v):
            raise Skip(f"owner {k} not found at {v}")
    with open(conf, "rb") as f:
        sha = hashlib.sha256(f.read()).hexdigest()[:12]
    revs = {"tmux.conf": f"{_git_rev(os.path.dirname(conf))} sha256:{sha}", "mu-crew-dotfiles": _git_rev(src["mucrew"])}
    for d in sorted(os.listdir(src["plugins"])):
        revs[f"plugin {d}"] = _git_rev(os.path.join(src["plugins"], d))
    src["revs"] = revs
    return src


def _environ_has(pid, marker):
    try:
        with open(f"/proc/{pid}/environ", "rb") as f:
            return marker in f.read().split(b"\0")
    except OSError:
        return False


def procs_with_home(home):
    """(pid, argv) of every process whose environment has HOME=<home>: everything the hermetic
    server started (its panes, run-shell jobs, the poller). Matched by environment, never by
    name, so neither the bench's own shell nor the owner's real processes can match."""
    marker = f"HOME={home}".encode()
    found = []
    for d in os.listdir("/proc"):
        if d.isdigit() and int(d) != os.getpid() and _environ_has(d, marker):
            try:
                with open(f"/proc/{d}/cmdline", "rb") as f:
                    argv = f.read().replace(b"\0", b" ").decode(errors="replace").strip()
            except OSError:
                continue
            found.append((int(d), argv))
    return found


def proc_cpu_ms(pid, children=False):
    """utime+stime of the process; with children also cutime+cstime (its reaped children)."""
    with open(f"/proc/{pid}/stat") as f:
        fields = f.read().rsplit(")", 1)[1].split()
    n = 4 if children else 2
    return sum(int(x) for x in fields[11:11 + n]) * 1000 / CLK_TCK


class OwnerServer(Server):
    """A server started with the owner's config in a hermetic HOME."""

    def __init__(self, binary, work, label, src):
        root = os.path.join(work, label)
        self.home = os.path.join(root, "home")
        bindir = os.path.join(root, "bin")
        os.makedirs(bindir)
        os.symlink(binary, os.path.join(bindir, "tmux"))
        super().__init__(os.path.join(bindir, "tmux"), work, label)
        self.root = root
        ignore = shutil.ignore_patterns(".git", "target", "node_modules")
        shutil.copytree(src["plugins"], os.path.join(self.home, ".tmux/plugins"), symlinks=True, ignore=ignore)
        shutil.copytree(src["mucrew"], os.path.join(self.home, ".local/share/mu-crew-dotfiles"), symlinks=True, ignore=ignore)
        with open(src["conf"]) as f:
            conf = f.read()
        conf, n = re.subn(r'^set -g @mu_crew_probes .*$', f'set -g @mu_crew_probes "{OWNER_PROBES}"', conf, flags=re.M)
        if n != 1:
            raise RuntimeError(f"expected one `set -g @mu_crew_probes` line in {src['conf']}, found {n}")
        with open(os.path.join(self.home, ".tmux.conf"), "w") as f:
            f.write("set -g @fingers-skip-wizard 1\n" + conf)
        for k in [k for k in self.env if k.startswith("XDG_")]:
            del self.env[k]
        self.env.update(HOME=self.home, PATH=bindir + os.pathsep + self.env.get("PATH", ""))

    def run(self, *args, check=True):
        # no -f: the config is the one in $HOME, found the way a user's tmux finds it
        r = subprocess.run([self.bin, "-L", self.label, *args], env=self.env, capture_output=True, text=True, timeout=120)
        if check and r.returncode:
            raise RuntimeError(f"{self.bin} {' '.join(args[:6])}: {r.stderr.strip()}")
        return r.stdout

    def poller_pid(self):
        """The poller's pid from its pidfile (under the hermetic HOME), or None."""
        d = os.path.join(self.home, ".local/state/mu-crew")
        try:
            for name in os.listdir(d):
                if name.startswith("poller-") and name.endswith(".pid"):
                    pid = int(open(os.path.join(d, name)).read().strip())
                    if os.path.exists(f"/proc/{pid}"):
                        return pid
        except (OSError, ValueError):
            pass
        return None

    def cleanup(self):
        """Wait up to 5 s for everything carrying our HOME to exit by itself (the poller leaves
        when its server is gone), then SIGKILL what is left and return it (empty = clean)."""
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline and procs_with_home(self.home):
            time.sleep(0.05)
        left = procs_with_home(self.home)
        for p, _ in left:
            try:
                os.kill(p, signal.SIGKILL)
            except ProcessLookupError:
                pass
        while procs_with_home(self.home):
            time.sleep(0.02)
        shutil.rmtree(self.root, ignore_errors=True)
        return left


def scenario_owner_idle(env):
    src = owner_sources()
    leftovers = []

    def one(which):
        s = OwnerServer(env.bins[which], env.work, f"o{os.getpid()}i{env.next()}", src)
        try:
            s.run("new", "-d", "-s", "idle", "cat")
            s.find_pid()
            probes = s.run("show", "-gv", "@mu_crew_probes").strip()
            if probes != OWNER_PROBES:
                raise RuntimeError(f"owner config not loaded: @mu_crew_probes = {probes!r}")
            deadline = time.monotonic() + 15
            while not s.poller_pid() and time.monotonic() < deadline:
                time.sleep(0.1)
            poller = s.poller_pid()
            if not poller:
                raise RuntimeError("the mu-crew poller did not start")
            time.sleep(10)  # settle: TPM's background init and the poller's first rounds
            pc0 = proc_cpu_ms(poller, children=True)
            with Phase(s.pid, fine=True) as p:
                time.sleep(IDLE_SECS)
            if s.poller_pid() != poller:
                raise RuntimeError("the poller died during the idle phase")
            return {"rss_mb": rss_kb(s.pid) / 1024, "cpu_ms": p.cpu, "wakeups_per_s": p.wakeups / p.wall,
                    "poller_cpu_ms": proc_cpu_ms(poller, children=True) - pc0}
        finally:
            try:
                s.kill()
            finally:
                leftovers.extend(s.cleanup())
    res = env.per_binary(one)
    return {"title": f"owner_idle (owner config, 1 session, {IDLE_SECS:g} s)", "results": res,
            "config": src["revs"], "leftovers": [f"{p} {a}" for p, a in leftovers]}


def scenario_owner_startup(env):
    src = owner_sources()
    leftovers = []

    def one(which):
        ms = []
        for _ in range(OWNER_STARTUP_N):
            s = OwnerServer(env.bins[which], env.work, f"o{os.getpid()}s{env.next()}", src)
            try:
                t0 = time.monotonic()
                out = s.run("start-server", ";", "display", "-p", "ok")  # the config is sourced before the first command runs
                ms.append((time.monotonic() - t0) * 1000)
                if out.strip() != "ok":
                    raise RuntimeError(f"owner_startup: display -p ok printed {out!r}")
                # the server may already have exited (no session): that is fine
                s.run("kill-server", check=False)
            finally:
                leftovers.extend(s.cleanup())
        return {"median_ms": med(ms), "min_ms": min(ms), "max_ms": max(ms), "n": len(ms)}
    res = {w: one(w) for w in env.bins}
    return {"title": f"owner_startup (start-server with the owner config until `display -p ok` answers, {OWNER_STARTUP_N}x)",
            "results": res, "config": src["revs"], "leftovers": [f"{p} {a}" for p, a in leftovers]}


def _owner_footer(res):
    left = res["leftovers"]
    out = [f"\nProcesses still carrying the hermetic HOME 5 s after kill-server (the bench SIGKILLed them), summed over all runs: {len(left)}"]
    out += [f"- {x}" for x in left[:10]]
    out.append("\nConfig snapshot: " + "; ".join(f"{k} {v}" for k, v in res["config"].items()))
    return "\n".join(out) + "\n"


def render_owner_idle(res):
    rows = [[b, fmt(m["rss_mb"]), fmt(m["cpu_ms"], 2), fmt(m["wakeups_per_s"], 2), fmt(m["poller_cpu_ms"], 0)]
            for b, m in res["results"].items()]
    return (f"### {res['title']}\n\n"
            + table(["binary", "RSS MB", "server CPU ms", "server wakeups/s", "poller CPU ms (outside the server)"], rows)
            + "\n\nThe poller column is the mu-crew-poller process itself plus the `tmux` clients it ran, not the server.\n"
            + _owner_footer(res))


def render_owner_startup(res):
    rows = [[b, fmt(m["median_ms"]), fmt(m["min_ms"]), fmt(m["max_ms"]), str(m["n"])] for b, m in res["results"].items()]
    return f"### {res['title']}\n\n" + table(["binary", "median ms", "min ms", "max ms", "runs"], rows) + "\n" + _owner_footer(res)


OWNER_FUNCS = {"owner_idle": scenario_owner_idle, "owner_startup": scenario_owner_startup}
OWNER_RENDER = {"owner_idle": render_owner_idle, "owner_startup": render_owner_startup}
ALL += list(OWNER_FUNCS)


# ---- attached-client scenarios (perf_attached) -------------------------------
# A real client on a PTY whose master side is drained by a Python thread (never script(1)):
# it counts the bytes the terminal would receive and counts the synchronized-update begin
# sequences (ESC[?2026h). Both binaries get the same TERM, the same explicit
# default-terminal/terminal-features, and the reader answers the DECRQM 2026 query the same
# way for both (sync on) or not at all (sync off); vtmux and tmux 3.8 both enable
# synchronized output only from that reply (render.rs:79, input.rs:215; tmux tty-keys.c
# tty_keys_sync), so the two modes are separate rows.

CLIENT_COLS, CLIENT_ROWS = 200, 50
RENDER_FRAMES = int(os.environ.get("RENDER_FRAMES", "3000"))
MANY_MB = int(os.environ.get("MANY_MB", "20"))
MANY_SIZES = [(80, 24), (100, 30), (120, 40), (132, 43), (160, 50), (200, 50), (90, 35), (240, 60)]
PANE_W, PANE_H = 95, 22  # render frames fit the smallest tiled pane of a 200x49 window
QUERY = b"\x1b[?2026$p"
BSU = b"\x1b[?2026h"
BLEND = os.environ.get("BLEND") == "1"
COLOURS_QUERY = b"\x1b]11;?\x1b\\"
COLOURS_REPLY = b"\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\"


def pin_terminal(s, **opts):
    """Identical terminal-related options for both binaries, set before any client attaches."""
    s.run("set", "-s", "default-terminal", "tmux-256color")
    s.run("set", "-s", "terminal-features", "xterm*:clipboard:ccolour:cstyle:focus:title")
    s.run("set", "-g", "window-size", opts.get("window_size", "latest"))
    # status-right defaults to the host name and a clock: host-dependent, time-varying bytes
    s.run("set", "-g", "status-right", "bench")
    s.run("set", "-g", "status-interval", "0")
    if BLEND:
        # D26; tmux has no such option, so its error is ignored.
        s.run("set", "-g", "vtmux-pane-inactive-blend", "fg=bg/25,bg=bg/25", check=False)


class Client:
    """`attach` on a fresh PTY of the given size, drained by a thread."""

    def __init__(self, server, session, cols, rows, sync):
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.reply = sync
        self.replied = False
        self.coloured = False
        self.bytes = 0
        self.bsu = 0
        self.proc = subprocess.Popen(
            [server.bin, "-L", server.label, "-f", "/dev/null", "attach", "-t", session],
            stdin=slave, stdout=slave, stderr=slave, env=server.env, start_new_session=True, close_fds=True)
        os.close(slave)
        self.pid = self.proc.pid
        self.thread = threading.Thread(target=self._drain, daemon=True)
        self.thread.start()

    def _drain(self):
        tail = b""
        while True:
            try:
                b = os.read(self.master, 65536)
            except OSError:
                break
            if not b:
                break
            self.bytes += len(b)
            data = tail + b
            # Carry one byte less than a BSU, so one that ends a read is
            # not counted again with the next; one split across two is
            # counted once.
            self.bsu += (tail[-(len(BSU) - 1):] + b).count(BSU)
            tail = data[-(len(COLOURS_QUERY) - 1):]
            if self.reply and not self.replied and QUERY in data:
                self.replied = True
                os.write(self.master, b"\x1b[?2026;2$y")
            if BLEND and not self.coloured and COLOURS_QUERY in data:
                self.coloured = True
                os.write(self.master, COLOURS_REPLY)

    def close(self):
        """After kill-server the client exits on its own; this reaps it and the reader."""
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()
        self.thread.join(5)
        os.close(self.master)


def quiet(clients, secs=0.3, limit=10):
    """Wait until no client received a byte for `secs`."""
    end = time.monotonic() + limit
    last, since = [c.bytes for c in clients], time.monotonic()
    while time.monotonic() < end:
        time.sleep(0.05)
        now = [c.bytes for c in clients]
        if now != last:
            last, since = now, time.monotonic()
        elif time.monotonic() - since >= secs:
            return
    raise RuntimeError("clients never went quiet")


def attach_all(s, session, sizes, sync):
    clients = [Client(s, session, c, r, sync) for c, r in sizes]
    deadline = time.monotonic() + 15
    while len(s.run("list-clients", "-F", "x").split()) < len(clients):
        if time.monotonic() > deadline:
            raise RuntimeError("clients did not attach")
        time.sleep(0.05)
    quiet(clients)
    if sync and not all(c.replied for c in clients):
        raise RuntimeError("the DECRQM 2026 query never arrived (sync-on row would not be sync-on)")
    return clients


class Attached:
    """Server CPU/RSS plus summed client CPU and terminal bytes over a with-block."""

    def __init__(self, s, clients):
        self.s, self.clients = s, clients

    def __enter__(self):
        self.b0 = sum(c.bytes for c in self.clients)
        self.f0 = sum(c.bsu for c in self.clients)
        self.cc0 = sum(cpu_ms(c.pid, fine=True) for c in self.clients)
        self.p = Phase(self.s.pid, fine=True).__enter__()
        return self

    def finish(self):
        """Call at the end of the measured work, before the with-block exits."""
        quiet(self.clients)
        self.client_cpu = sum(cpu_ms(c.pid, fine=True) for c in self.clients) - self.cc0
        self.term_bytes = sum(c.bytes for c in self.clients) - self.b0
        self.bsu = sum(c.bsu for c in self.clients) - self.f0

    def __exit__(self, *exc):
        self.p.__exit__(*exc)
        return False


def _logfile(env, kind, mb):
    cache = env.__dict__.setdefault("logs", {})
    if (kind, mb) not in cache:
        path = os.path.join(env.work, f"alog-{kind}-{mb}.txt")
        gen_log(path, mb, kind == "sgr")
        cache[(kind, mb)] = path
    return cache[(kind, mb)]


def _wait_markers(s, targets, what):
    deadline = time.monotonic() + 600
    pending = list(targets)
    while pending:
        pending = [t for t in pending if "\nBENCH-DONE\n" not in "\n" + s.run("capture-pane", "-p", "-J", "-t", t)]
        if pending:
            if time.monotonic() > deadline:
                raise RuntimeError(f"{what}: done marker not seen in 600 s")
            time.sleep(0.2)


def _trigger(path, wait=True):
    """A pane command that (optionally) waits for Enter, then does the fixed work, prints the marker and idles."""
    return f"{'read x; ' if wait else ''}cat '{path}'; printf 'BENCH-%s\\n' DONE; exec cat"


def scenario_throughput_attached(env):
    out = {}
    for kind in ("plain", "sgr"):
        path = _logfile(env, kind, THROUGHPUT_MB)
        size = os.path.getsize(path) / 1e6
        for sync in (False, True):
            def one(which, path=path, size=size, sync=sync):
                s = env.server(which)
                s.run("new", "-d", "-s", "t", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS), "cat")
                s.find_pid()
                clients = []
                try:
                    pin_terminal(s)
                    clients = attach_all(s, "t", [(CLIENT_COLS, CLIENT_ROWS)], sync)
                    time.sleep(1)
                    with Attached(s, clients) as a:
                        t0 = time.monotonic()
                        s.run("neww", "-t", "t:", "-n", "w", _trigger(path, wait=False))
                        _wait_markers(s, ["t:w"], "throughput_attached")
                        wall = time.monotonic() - t0
                        a.finish()
                    return {"wall_s": wall, "server_cpu_s": a.p.cpu / 1000, "client_cpu_s": a.client_cpu / 1000,
                            "term_bytes_per_mb": a.term_bytes / size, "sync_frames": a.bsu, "peak_rss_mb": a.p.peak / 1024}
                finally:
                    s.kill()
                    for c in clients:
                        c.close()
            out[(kind, sync)] = env.per_binary(one)
    return {"title": f"throughput_attached ({THROUGHPUT_MB} MB log, one {CLIENT_COLS}x{CLIENT_ROWS} client, window-size latest)",
            "rows": {f"{k} / sync {'on' if y else 'off'}": v for (k, y), v in out.items()}}


def gen_frames(path, n, w, h, seed):
    """n full repaints of a w x h screen as cursor-addressed rows, ending in a screen clear:
    a static label, a changing value and a coloured bar per row (a top/htop-like redraw)."""
    rnd = random.Random(seed)
    words = ["cpu", "mem", "net", "disk", "proc", "load", "swap", "irq", "tasks", "temp"]
    labels = [f"{rnd.choice(words)}{r:02d} {rnd.choice(words)}-{rnd.randrange(1000):03d} {rnd.choice(words)}" for r in range(h)]
    rows = []
    with open(path, "w", newline="") as f:
        for k in range(n):
            for r in range(h):
                val = (k * (r + 3) + seed * 7) % 1000
                col = 32 if val < 500 else 33 if val < 800 else 31
                rows.append(f"\x1b[{r + 1};1H{labels[r]} {val:4d} \x1b[{col}m{'#' * (val * 40 // 1000):<40}\x1b[0m\x1b[K")
            if len(rows) > 20000:
                f.write("".join(rows))
                rows = []
        f.write("".join(rows) + "\x1b[2J\x1b[H")


def scenario_render(env):
    paths = []
    for i in range(4):
        p = os.path.join(env.work, f"frames{i}.txt")
        gen_frames(p, RENDER_FRAMES, PANE_W, PANE_H, i + 1)
        paths.append(p)
    mb = sum(os.path.getsize(p) for p in paths) / 1e6
    out = {}
    for sync in (False, True):
        def one(which, sync=sync):
            s = env.server(which)
            s.run("new", "-d", "-s", "r", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS), _trigger(paths[0]))
            s.find_pid()
            clients = []
            try:
                pin_terminal(s)
                for p in paths[1:]:
                    s.run("splitw", "-d", "-t", "r:0", _trigger(p))
                s.run("select-layout", "-t", "r:0", "tiled")
                dims = [tuple(map(int, d.split("x"))) for d in s.run("list-panes", "-t", "r:0", "-F", "#{pane_width}x#{pane_height}").split()]
                if len(dims) != 4 or min(w for w, _ in dims) < PANE_W or min(h for _, h in dims) < PANE_H:
                    raise RuntimeError(f"render: pane sizes {dims} are smaller than {PANE_W}x{PANE_H}")
                ids = s.run("list-panes", "-t", "r:0", "-F", "#{pane_id}").split()
                clients = attach_all(s, "r", [(CLIENT_COLS, CLIENT_ROWS)], sync)
                time.sleep(1)
                with Attached(s, clients) as a:
                    t0 = time.monotonic()
                    cmd = []
                    for i in ids:
                        cmd += ["send-keys", "-t", i, "Enter", ";"]
                    s.run(*cmd[:-1])
                    _wait_markers(s, ids, "render")
                    wall = time.monotonic() - t0
                    a.finish()
                return {"wall_s": wall, "server_cpu_s": a.p.cpu / 1000, "client_cpu_s": a.client_cpu / 1000,
                        "term_mb": a.term_bytes / 1e6, "sync_frames": a.bsu, "peak_rss_mb": a.p.peak / 1024}
            finally:
                s.kill()
                for c in clients:
                    c.close()
        out[sync] = env.per_binary(one)
    return {"title": f"render (4 panes in a {CLIENT_COLS}x{CLIENT_ROWS} client, each cat'ing {RENDER_FRAMES} cursor-addressed {PANE_W}x{PANE_H} frames; {mb:.0f} MB of input)",
            "input_mb": mb, "rows": {f"sync {'on' if y else 'off'}": v for y, v in out.items()}}


def scenario_many_clients(env):
    path = _logfile(env, "plain", MANY_MB)
    out = {}
    for sync in (False, True):
        def one(which, sync=sync):
            s = env.server(which)
            s.run("new", "-d", "-s", "m", "-x", "200", "-y", "60", _trigger(path))
            s.find_pid()
            clients = []
            try:
                pin_terminal(s, window_size="smallest")
                s.run("splitw", "-d", "-t", "m:0", "cat")
                clients = attach_all(s, "m", MANY_SIZES, sync)
                size = s.run("display", "-p", "-t", "m:0", "#{window_width}x#{window_height}").strip()
                want = f"{min(c for c, _ in MANY_SIZES)}x{min(r for _, r in MANY_SIZES) - 1}"  # smallest client, minus the status line
                if size != want:
                    raise RuntimeError(f"many_clients: window is {size}, expected {want} from window-size smallest")
                time.sleep(1)
                rss_attached = rss_kb(s.pid)
                pane = s.run("list-panes", "-t", "m:0", "-F", "#{pane_id}").split()[0]
                with Attached(s, clients) as a:
                    t0 = time.monotonic()
                    s.run("send-keys", "-t", pane, "Enter")
                    _wait_markers(s, [pane], "many_clients")
                    wall = time.monotonic() - t0
                    a.finish()
                return {"wall_s": wall, "server_cpu_s": a.p.cpu / 1000, "client_cpu_s": a.client_cpu / 1000,
                        "term_mb": a.term_bytes / 1e6, "rss_attached_mb": rss_attached / 1024,
                        "rss_mb": rss_kb(s.pid) / 1024, "peak_rss_mb": a.p.peak / 1024}
            finally:
                s.kill()
                for c in clients:
                    c.close()
        out[sync] = env.per_binary(one)
    return {"title": f"many_clients (1 session, {len(MANY_SIZES)} clients {', '.join(f'{c}x{r}' for c, r in MANY_SIZES)}, window-size smallest, {MANY_MB} MB flood in one of 2 panes)",
            "rows": {f"sync {'on' if y else 'off'}": v for y, v in out.items()}}


AGENT_PANES = [int(n) for n in os.environ.get("AGENT_PANES", "5 10 20").split()]
AGENT_CLIENTS = [int(n) for n in os.environ.get("AGENT_CLIENTS", "1 2").split()]
AGENT_SECS = float(os.environ.get("AGENT_SECS", "10"))


def scenario_agent_panes(env):
    """N panes running scripts/agent_tui.py (pi-like synchronized redraws at real speed),
    2 panes per window; AGENT_CLIENTS attached 200x50 clients (sync on), the first on
    window 0, the second on a session that has window 1 linked. Server CPU over AGENT_SECS
    of steady state."""
    emitter = os.path.join(ROOT, "scripts", "agent_tui.py")
    out = {}
    for n in AGENT_PANES:
        for nc in AGENT_CLIENTS:
            def one(which, n=n, nc=nc):
                s = env.server(which)
                cmd = lambda i: f"exec python3 '{emitter}' {i + 1}"  # noqa: E731
                s.run("new", "-d", "-s", "a", "-n", "w0", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS - 1), cmd(0))
                s.find_pid()
                clients = []
                try:
                    pin_terminal(s)
                    s.run("set", "-g", "default-size", f"{CLIENT_COLS}x{CLIENT_ROWS - 1}", ";", "set", "-g", "automatic-rename", "off")
                    s.run("set", "-g", "pane-border-format", "agent")
                    for i in range(1, n):
                        if i % 2:
                            s.run("splitw", "-d", "-h", "-t", f"a:w{i // 2}", cmd(i))
                        else:
                            s.run("neww", "-d", "-t", "a:", "-n", f"w{i // 2}", cmd(i))
                    panes = len(s.run("list-panes", "-a", "-F", "x").split())
                    if panes != n:
                        raise RuntimeError(f"agent_panes: {panes} panes, expected {n}")
                    # session b shows window 1 (a linked window, not a session group: the old vtmux has no groups)
                    w = "w1" if n > 2 else "w0"
                    s.run("new", "-d", "-s", "b", "-n", "x", "cat", ";", "link-window", "-s", f"a:{w}", "-t", "b:9", ";",
                          "kill-window", "-t", "b:x", ";", "select-window", "-t", "b:9")
                    # not attach_all: the panes never go quiet
                    clients = [Client(s, sess, CLIENT_COLS, CLIENT_ROWS, True) for sess in ("a", "b")[:nc]]
                    deadline = time.monotonic() + 15
                    while len(s.run("list-clients", "-F", "x").split()) < nc:
                        if time.monotonic() > deadline:
                            raise RuntimeError("agent_panes: clients did not attach")
                        time.sleep(0.05)
                    time.sleep(2)  # first full frames, then steady state
                    with Attached(s, clients) as a:
                        time.sleep(AGENT_SECS)
                        a.client_cpu = sum(cpu_ms(c.pid, fine=True) for c in clients) - a.cc0
                        a.term_bytes = sum(c.bytes for c in clients) - a.b0
                        a.bsu = sum(c.bsu for c in clients) - a.f0
                    if nc and not all(c.replied for c in clients):
                        raise RuntimeError("agent_panes: the DECRQM 2026 query never arrived")
                    return {"server_cpu_pct": a.p.cpu / 10 / a.p.wall, "client_cpu_pct": a.client_cpu / 10 / a.p.wall,
                            "term_kb_per_s": a.term_bytes / 1024 / a.p.wall, "frames_per_s": a.bsu / a.p.wall,
                            "wakeups_per_s": None if a.p.wakeups is None else a.p.wakeups / a.p.wall,
                            "rss_mb": rss_kb(s.pid) / 1024}
                finally:
                    s.kill()
                    for c in clients:
                        c.close()
            out[(n, nc)] = env.per_binary(one)
    return {"title": f"agent_panes (N panes of scripts/agent_tui.py, pi-like synchronized redraws, 2 panes per window; "
                     f"{'/'.join(map(str, AGENT_CLIENTS))} {CLIENT_COLS}x{CLIENT_ROWS} clients with sync on, the second on window 1; {AGENT_SECS:g} s)",
            "rows": {f"{n} panes / {nc} clients": v for (n, nc), v in out.items()}}


KEY_ECHO_N = int(os.environ.get("KEY_ECHO_N", "200"))
KEY_ECHO_GAP = 0.03
# The echo pane: raw mode, each key byte k ('a'..'x') answered with `\r ESC[K` and a Greek letter
# (U+03B1 + k - 'a'), whose UTF-8 bytes appear nowhere in the flood, the status line or escape sequences.
KEY_ECHO_PROG = r'''
import os, sys, tty
tty.setraw(0)
while True:
    b = os.read(0, 1)
    if not b:
        break
    os.write(1, b"\r\x1b[K" + chr(0x3B1 + b[0] - 97).encode())
'''


class EchoClient(Client):
    """A Client whose reader thread stamps the arrival of an expected byte string."""

    def __init__(self, *a, **kw):
        self.want, self.sent, self.lat, self.got = None, 0.0, [], threading.Event()
        self._tail = b""
        super().__init__(*a, **kw)

    def _drain(self):
        while True:
            try:
                b = os.read(self.master, 65536)
            except OSError:
                break
            if not b:
                break
            now = time.monotonic()
            self.bytes += len(b)
            self.bsu += (self._tail[-(len(BSU) - 1):] + b).count(BSU)
            data = self._tail + b
            self._tail = data[-8:]
            if self.reply and not self.replied and QUERY in data:
                self.replied = True
                os.write(self.master, b"\x1b[?2026;2$y")
            w = self.want
            if w is not None and w in data:
                self.want = None
                self.lat.append((now - self.sent) * 1000)
                self.got.set()

    def key(self, k):
        """Type one key and wait (up to 1 s) for its echo; None if it never came."""
        self._tail = b""
        self.got.clear()
        self.want = chr(0x3B1 + ord(k) - 97).encode()
        self.sent = time.monotonic()
        os.write(self.master, k.encode())
        if not self.got.wait(1.0):
            self.want = None
            return None
        return self.lat[-1]


def scenario_key_echo(env):
    """Key-to-echo latency with a 200x50 client attached (sync on): window split -h, the right
    (active) pane a raw echo program, the left `yes` or idle `cat`; KEY_ECHO_N keys 30 ms apart,
    each echo's arrival stamped in the reader thread. p50 / p90 / max ms of each run."""
    prog = os.path.join(env.work, "echo.py")
    with open(prog, "w") as f:
        f.write(KEY_ECHO_PROG)
    py = shutil.which("python3") or sys.executable
    out = {}
    for flood in ("none", "yes"):
        def one(which, flood=flood):
            s = env.server(which)
            s.run("new", "-d", "-s", "k", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS - 1), "exec yes" if flood == "yes" else "exec cat")
            s.find_pid()
            clients = []
            try:
                pin_terminal(s)
                s.run("set", "-s", "escape-time", "0", ";", "set", "-g", "automatic-rename", "off")
                s.run("splitw", "-h", "-t", "k:0", f"exec '{py}' '{prog}'")
                time.sleep(0.5)  # the echo program is in raw mode
                clients = [EchoClient(s, "k", CLIENT_COLS, CLIENT_ROWS, True)]
                deadline = time.monotonic() + 15
                while len(s.run("list-clients", "-F", "x").split()) < 1:
                    if time.monotonic() > deadline:
                        raise RuntimeError("key_echo: the client did not attach")
                    time.sleep(0.05)
                time.sleep(1)
                c = clients[0]
                lat, lost = [], 0
                for i in range(KEY_ECHO_N):
                    v = c.key(chr(97 + i % 24))
                    if v is None:
                        lost += 1
                    else:
                        lat.append(v)
                    time.sleep(KEY_ECHO_GAP)
                if len(lat) < KEY_ECHO_N * 0.9:
                    raise RuntimeError(f"key_echo: {lost} of {KEY_ECHO_N} echoes never arrived")
                lat.sort()
                return {"p50_ms": lat[len(lat) // 2], "p90_ms": lat[int(len(lat) * 0.9)], "max_ms": lat[-1], "lost": lost}
            finally:
                s.kill()
                for c in clients:
                    c.close()
        out[flood] = env.per_binary(one)
    return {"title": f"key_echo ({KEY_ECHO_N} keys 30 ms apart into a raw echo pane, one {CLIENT_COLS}x{CLIENT_ROWS} client, sync on, "
                     f"the other pane idle or running `yes`; arrival stamped in the reader thread)",
            "rows": {f"flood {k}": v for k, v in out.items()}}


ATTACHED = {"throughput_attached": scenario_throughput_attached, "render": scenario_render, "many_clients": scenario_many_clients,
            "agent_panes": scenario_agent_panes, "key_echo": scenario_key_echo}
ALL += list(ATTACHED)


# ---- trickle: scrollback memory of a pane that never goes idle ---------------

TRICKLE_GEN = r'''
import sys, time
rate, secs = float(sys.argv[1]), float(sys.argv[2])
tick = max(0.01, 1.0 / rate)          # 1000 lines/s: 10 lines every 10 ms; 10 lines/s: one every 100 ms
per = max(1, round(rate * tick))
t0 = time.monotonic()
i = n = 0
out = sys.stdout
while True:
    now = time.monotonic()
    if now - t0 >= secs:
        break
    for _ in range(per):
        out.write("2026-10-31T12:%02d:%02d.%03dZ INFO  worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=%d\n"
                  % ((i // 60000) % 60, (i // 1000) % 60, i % 1000, i % 16, i * 2654435761 % 4294967296, i, (i * 37) % 100000))
        i += 1
    out.flush()
    n += 1
    time.sleep(max(0.0, t0 + n * tick - time.monotonic()))  # absolute schedule: no drift
'''


def scenario_trickle(env):
    """Server RSS while one pane writes log lines at a steady rate, so its history never goes idle
    (docs/specs/2026-10-31-history-memory.md, compress.rs `STARVE`). Every case (binary x rate, plus `fill`:
    150k lines at once, then idle; with TRICKLE_PREFILL the rate cases write those 150k lines first too) runs on
    its own server at the same time. RSS, #{history_size} and the server's CPU time (and on macOS the footprint,
    see footprint_kb) are sampled every TRICKLE_SAMPLE s for TRICKLE_SECS of output and TRICKLE_IDLE_SECS after."""
    rates = os.environ.get("TRICKLE_RATES", "10 1000 fill").split()
    secs = float(os.environ.get("TRICKLE_SECS", "1200"))
    idle = float(os.environ.get("TRICKLE_IDLE_SECS", "120"))
    every = float(os.environ.get("TRICKLE_SAMPLE", "10"))
    limit = os.environ.get("TRICKLE_LIMIT", "100000")
    prefill = os.environ.get("TRICKLE_PREFILL", "1") != "0"
    counts = [int(n) for n in os.environ.get("TRICKLE_PANES", "1").split()]
    gen = os.path.join(env.work, "trickle.py")
    with open(gen, "w") as f:
        f.write(TRICKLE_GEN)
    fill = os.path.join(env.work, "fill.txt")
    with open(fill, "w") as f:
        for i in range(150000):
            f.write("2026-10-31T12:%02d:%02d.%03dZ INFO  worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=%d\n"
                    % ((i // 60000) % 60, (i // 1000) % 60, i % 1000, i % 16, i * 2654435761 % 4294967296, i, (i * 37) % 100000))
    py = shutil.which("python3") or sys.executable
    cases, servers = [], []
    try:
        for which in env.bins:
            for panes in counts:
                for rate in rates:
                    s = env.server(which)
                    pre = f"cat '{fill}'; " if rate == "fill" or prefill else ""
                    cmd = pre + ("exec cat" if rate == "fill" else f"'{py}' '{gen}' {rate} {secs}; exec cat")
                    # One window per streaming pane, all 120x40 (new -x/-y sets the session's default-size).
                    more = [a for _ in range(panes - 1) for a in (";", "neww", "-d", "-t", "t", cmd)]
                    s.run("new", "-d", "-s", "keep", "cat", ";", "set", "-g", "history-limit", limit, ";",
                          "new", "-d", "-s", "t", "-x", "120", "-y", "40", cmd, *more)
                    s.find_pid()
                    servers.append(s)
                    cases.append({"binary": which, "rate": rate, "panes": panes, "samples": []})
        t0 = time.monotonic()
        k = 0
        while True:
            t = k * every
            time.sleep(max(0.0, t0 + t - time.monotonic()))
            for s, c in zip(servers, cases):
                hs = s.run("display", "-p", "-t", "t:0", "#{history_size}", check=False).strip()
                c["samples"].append([round(time.monotonic() - t0, 1), rss_kb(s.pid), int(hs or -1), footprint_kb(s.pid),
                                     round(cpu_ms(s.pid), 1)])
            print(f"trickle t={t:.0f}s load {os.getloadavg()[0]:.1f} " +
                  " ".join(f"{_case(c, '/')}={_mem(c['samples'][-1])}" for c in cases),
                  file=sys.stderr, flush=True)
            if t >= secs + idle:
                break
            k += 1
    finally:
        for s in servers:
            s.kill()
    per = "" if counts == [1] else ", each case N panes doing it at once (xN), hsize of the first"
    return {"title": f"trickle (120x40, history-limit {limit}, {'150k lines then ' if prefill else ''}{secs:g} s of output then {idle:g} s idle, "
                     f"RSS every {every:g} s{per})", "secs": secs, "cases": cases}


# ---- live upgrade (vtmux only) -------------------------------------------------

UPGRADE_PANES = [int(n) for n in os.environ.get("UPGRADE_PANES", "1 10").split()]
UPGRADE_LINES = [int(n) for n in os.environ.get("UPGRADE_LINES", "10000 100000").split()]
UPGRADE_IDLE_LIMIT = float(os.environ.get("UPGRADE_IDLE_LIMIT", "180"))
UPGRADE_STATES = os.environ.get("UPGRADE_STATES", "fresh idle").split()
UPGRADE_LIMIT = int(os.environ.get("UPGRADE_LIMIT", "100000"))
RESTARTED = b"\x1b[?996n"  # Caps::restart: a re-attached client's terminal started again
STATUS_MARK = b"bench"  # pin_terminal's status-right


class UpgradeClient(Client):
    """A Client whose reader thread stamps its redraw after an upgrade: the terminal's
    restart (RESTARTED), then the status line (STATUS_MARK)."""

    def __init__(self, *a, **kw):
        self.armed, self.restarted, self.redrawn = False, False, None
        self._tail = b""
        super().__init__(*a, **kw)

    def arm(self):
        self._tail, self.restarted, self.redrawn = b"", False, None
        self.armed = True

    def _drain(self):
        while True:
            try:
                b = os.read(self.master, 65536)
            except OSError:
                break
            if not b:
                break
            now = time.monotonic()
            self.bytes += len(b)
            if self.reply and not self.replied and QUERY in self._tail + b:
                self.replied = True
                os.write(self.master, b"\x1b[?2026;2$y")
            if not self.armed:
                self._tail = (self._tail + b)[-8:]
                continue
            data = self._tail + b
            self._tail = data[-8:]
            if not self.restarted and RESTARTED in data:
                self.restarted = True
                data = data[data.index(RESTARTED):]
            if self.restarted and self.redrawn is None and STATUS_MARK in data:
                self.redrawn = now
                self.armed = False


class ControlReader:
    """A `-C attach` client whose output a thread reads and drops."""

    def __init__(self, s, session):
        self.proc = subprocess.Popen([s.bin, "-L", s.label, "-f", "/dev/null", "-C", "attach", "-t", session],
                                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, env=s.env)
        self.lines = 0
        self.thread = threading.Thread(target=self._drain, daemon=True)
        self.thread.start()

    def _drain(self):
        for _ in self.proc.stdout:
            self.lines += 1

    def close(self):
        try:
            self.proc.stdin.close()
            self.proc.wait(timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            self.proc.kill()
            self.proc.wait()
        self.thread.join(5)


def gen_lines(path, n):
    """n lines of 100 printable columns, every tenth with SGR colours (seeded, so every run alike)."""
    rnd = random.Random(n)
    words = ["build", "test", "ok", "error:", "warning", "src/main.rs", "fn", "let", "=>", "0x1f", "passed"]
    with open(path, "w") as f:
        for i in range(n):
            text = f"{i:07d} " + " ".join(rnd.choice(words) for _ in range(20))
            text = text[:100].ljust(100)
            if i % 10 == 0:
                text = f"\x1b[3{i % 8}m{text[:50]}\x1b[m{text[50:]}"
            f.write(text + "\n")


def _wait_history(s, n, rows, limit=600):
    """Every pane's history full: at least min(lines, history-limit) - rows, and unchanged for 1 s."""
    want = n - rows - 2
    deadline, last, since = time.monotonic() + limit, None, time.monotonic()
    while time.monotonic() < deadline:
        sizes = [int(x) for x in s.run("list-panes", "-a", "-F", "#{history_size}").split()]
        if sizes != last:
            last, since = sizes, time.monotonic()
        elif all(h >= want for h in sizes) and time.monotonic() - since >= 1:
            return sum(sizes)
        time.sleep(0.2)
    raise RuntimeError(f"upgrade: history never filled ({last}, want {want} each)")


def _wait_rss_settled(pid, secs=3.0, limit=UPGRADE_IDLE_LIMIT):
    """Idle past the compressor's 5 s of quiet history, then until the server's RSS has not
    fallen by 0.5% in `secs` (its passes are done). Seconds idle, None at `limit`."""
    deadline = time.monotonic() + limit
    time.sleep(6)
    low, since = rss_kb(pid), time.monotonic()
    while time.monotonic() < deadline:
        time.sleep(0.25)
        now = rss_kb(pid)
        if now < low * 0.995:
            low, since = now, time.monotonic()
        elif time.monotonic() - since >= secs:
            return time.monotonic() - (deadline - limit)
    return None


def scenario_upgrade(env):
    """vtmux-upgrade onto its own binary (live-upgrade spec, decision 13), vtmux only: N panes
    (one window each, CLIENT_COLS x CLIENT_ROWS, history-limit UPGRADE_LIMIT) that printed L lines of
    100 columns (every tenth coloured), one attached client and two control clients. `fresh`:
    as soon as the history is full; `idle`: after the compressor has finished (RSS settled).
    answered = until `vtmux-upgrade -b <same binary>` returns (the new image serves);
    redrawn = until the attached client got its terminal restart and then the status line.
    The snapshot size is taken by a second upgrade right after, through a wrapper that counts
    the dry run's snapshot descriptor."""
    if "vtmux" not in env.bins:
        raise Skip("no vtmux binary")
    rows = CLIENT_ROWS - 1
    out, refused = {}, {}
    for n in UPGRADE_PANES:
        for lines in UPGRADE_LINES:
            path = os.path.join(env.work, f"lines-{lines}.txt")
            if not os.path.exists(path):
                gen_lines(path, lines)
            for state in UPGRADE_STATES:
                case = f"{n} x {lines // 1000}k {state}"

                def one(which, n=n, lines=lines, state=state, path=path, case=case):
                    s = env.server(which)
                    s.run("new", "-d", "-s", "u", "-n", "w0", "-x", str(CLIENT_COLS), "-y", str(rows), "cat")
                    s.find_pid()
                    clients, controls = [], []
                    sizefile = os.path.join(env.work, f"size-{s.label}")
                    wrapper = os.path.join(env.work, f"wrap-{s.label}")
                    as_rows = state.endswith("-rows")
                    alt = "-alt" in state
                    with open(wrapper, "w") as f:
                        f.write(f"#!/bin/sh\nif [ \"$1\" = --vtmux-check-snapshot ]; then wc -c </dev/fd/$2 >'{sizefile}'; fi\n"
                                + ("if [ \"$1\" = --vtmux-patch-stack ]; then echo 0; exit 0; fi\n" if as_rows else "")
                                + f"exec '{s.bin}' \"$@\"\n")
                    os.chmod(wrapper, 0o755)
                    # `-rows`: the upgrade goes onto the wrapper, which reports another patch stack.
                    target = wrapper if as_rows else s.bin
                    try:
                        pin_terminal(s)
                        s.run("set", "-g", "history-limit", str(UPGRADE_LIMIT), ";", "set", "-g", "automatic-rename", "off",
                              ";", "set", "-g", "default-size", f"{CLIENT_COLS}x{rows}")
                        # `-alt`: an editor up over the history (?1049h), as vim is.
                        fill = f"cat '{path}'; " + ("printf '\\033[?1049h'; " if alt else "") + "exec cat"
                        s.run("respawn-pane", "-k", "-t", "u:w0", "sh", "-c", fill)
                        for i in range(1, n):
                            s.run("neww", "-d", "-t", "u:", "-n", f"w{i}", "sh", "-c", fill)
                        clients = [UpgradeClient(s, "u", CLIENT_COLS, CLIENT_ROWS, True)]
                        controls = [ControlReader(s, "u") for _ in range(2)]
                        deadline = time.monotonic() + 15
                        while len(s.run("list-clients", "-F", "x").split()) < 3:
                            if time.monotonic() > deadline:
                                raise RuntimeError("upgrade: clients did not attach")
                            time.sleep(0.05)
                        history = _wait_history(s, min(lines, UPGRADE_LIMIT), rows)
                        idle_s = _wait_rss_settled(s.pid) if state.startswith("idle") else 0.0
                        quiet(clients)
                        panes = s.run("list-panes", "-a", "-F", "#{pane_pid}")
                        rss_before = rss_kb(s.pid)
                        c = clients[0]
                        c.arm()
                        t0 = time.monotonic()
                        r = s.run("vtmux-upgrade", "-b", target, check=False, full=True)
                        answered = time.monotonic()
                        if r.returncode:
                            refused.setdefault(case, []).append(r.stderr.strip().replace(s.bin, "<vtmux>"))
                            return {"history_lines": history, "idle_s": idle_s, "answered_ms": None, "redrawn_ms": None,
                                    "snapshot_mb": None, "rss_before_mb": rss_before / 1024, "rss_after_mb": None}
                        deadline = answered + 30
                        while c.redrawn is None and time.monotonic() < deadline:
                            time.sleep(0.001)
                        if c.redrawn is None:
                            raise RuntimeError("upgrade: the client never redrew")
                        if s.find_pid() != s.pid or s.run("list-panes", "-a", "-F", "#{pane_pid}") != panes:
                            raise RuntimeError("upgrade: the server or a pane process changed")
                        rss_after = rss_kb(s.pid)
                        quiet(clients)
                        s.run("vtmux-upgrade", "-b", wrapper)
                        with open(sizefile) as f:
                            size = int(f.read().strip())
                        return {"history_lines": history, "idle_s": idle_s, "answered_ms": (answered - t0) * 1000,
                                "redrawn_ms": (c.redrawn - t0) * 1000, "snapshot_mb": size / 1e6,
                                "rss_before_mb": rss_before / 1024, "rss_after_mb": rss_after / 1024}
                    finally:
                        s.kill()
                        for c in clients:
                            c.close()
                        for c in controls:
                            c.close()
                out[case] = env_with(env, {"vtmux": env.bins["vtmux"]}).per_binary(one)
    return {"title": f"upgrade (vtmux-upgrade onto the same binary: N panes x L lines, one window each, "
                     f"1 attached {CLIENT_COLS}x{CLIENT_ROWS} client with sync on, 2 control clients; target < 1000 ms, decision 13)",
            "rows": out, "refused": refused}


def env_with(env, bins):
    """`env` measuring only `bins` (the upgrade has no tmux side)."""
    e = Env.__new__(Env)
    e.__dict__.update(env.__dict__)
    e.bins = bins
    return e


ALL.append("upgrade")
OPTIONAL = ["trickle"]  # long (about 22 min): run by name only


def _case(c, sep=" "):
    """binary, rate and (with more than one) the pane count of a trickle case."""
    n = c.get("panes", 1)
    return f"{c['binary']}{sep}{c['rate']}" + (f"{sep}x{n}" if n != 1 else "")


def _mem(sample):
    """RSS MB, with the macOS footprint after a slash."""
    fp = "" if sample[3] is None else f"/{sample[3] / 1024:.1f}"
    return f"{sample[1] / 1024:.1f}{fp}"


def render_trickle(res):
    cases = res["cases"]
    what = "RSS MB (hsize)" if LINUX else "RSS/footprint MB (hsize)"
    header = ["t s"] + [f"{_case(c)} {what}" for c in cases]
    rows = []
    n = min(len(c["samples"]) for c in cases)
    step = max(1, n // 40)  # about 40 rows; the JSON has every sample
    for i in sorted(set(range(0, n, step)) | {n - 1}):
        rows.append([f"{cases[0]['samples'][i][0]:.0f}"] +
                    [f"{_mem(c['samples'][i])} ({c['samples'][i][2]})" for c in cases])
    rows.append(["server CPU s"] + [f"{c['samples'][n - 1][4] / 1000:.1f}" for c in cases])
    return f"### {res['title']}\n\n" + table(header, rows) + "\n"


def render_attached(name, res):
    cols = {
        "throughput_attached": [("wall s", "wall_s", 2), ("server CPU s", "server_cpu_s", 2), ("client CPU s", "client_cpu_s", 2), ("total CPU s", "total_cpu_s", 2),
                                ("term bytes / MB in", "term_bytes_per_mb", 0), ("sync frames", "sync_frames", 0), ("peak RSS MB", "peak_rss_mb", 1)],
        "render": [("wall s", "wall_s", 2), ("server CPU s", "server_cpu_s", 2), ("client CPU s", "client_cpu_s", 2), ("total CPU s", "total_cpu_s", 2),
                   ("terminal MB", "term_mb", 2), ("sync frames", "sync_frames", 0), ("peak RSS MB", "peak_rss_mb", 1)],
        "many_clients": [("wall s", "wall_s", 2), ("server CPU s", "server_cpu_s", 2), ("clients CPU s (sum)", "client_cpu_s", 2), ("total CPU s", "total_cpu_s", 2),
                         ("terminal MB (sum)", "term_mb", 1), ("RSS MB (idle, attached)", "rss_attached_mb", 1),
                         ("RSS MB (after)", "rss_mb", 1), ("peak RSS MB", "peak_rss_mb", 1)],
        "agent_panes": [("server CPU %", "server_cpu_pct", 1), ("clients CPU % (sum)", "client_cpu_pct", 1),
                        ("terminal KB/s (sum)", "term_kb_per_s", 1), ("sync frames/s (sum)", "frames_per_s", 0),
                        ("server wakeups/s", "wakeups_per_s", 0), ("RSS MB", "rss_mb", 1)],
        "key_echo": [("p50 ms", "p50_ms", 2), ("p90 ms", "p90_ms", 2), ("max ms", "max_ms", 2), ("lost", "lost", 0)],
        "upgrade": [("history lines", "history_lines", 0), ("idle s", "idle_s", 1), ("answered ms", "answered_ms", 0),
                    ("redrawn ms", "redrawn_ms", 0), ("snapshot MB", "snapshot_mb", 1), ("RSS MB before", "rss_before_mb", 1),
                    ("RSS MB after", "rss_after_mb", 1)],
    }[name]
    for per in res["rows"].values():
        for m in per.values():
            if "server_cpu_s" in m:
                m["total_cpu_s"] = m["server_cpu_s"] + m["client_cpu_s"]
    rows = [[k, b, *[fmt(m[key], nd) for _, key, nd in cols]] for k, per in res["rows"].items() for b, m in per.items()]
    notes = "".join(f"\n{k}: {len(v)} of {RUNS} runs refused: {'; '.join(sorted(set(v)))}\n"
                    for k, v in res.get("refused", {}).items())
    return f"### {res['title']}\n\n" + table(["case", "binary", *[c[0] for c in cols]], rows) + "\n" + notes


# ---- output -----------------------------------------------------------------

def fmt(v, nd=1):
    return "-" if v is None else f"{v:.{nd}f}"


def table(header, rows):
    out = ["| " + " | ".join(header) + " |", "|" + "|".join("---" for _ in header) + "|"]
    out += ["| " + " | ".join(r) + " |" for r in rows]
    return "\n".join(out)


def render(name, res):
    if name in OWNER_RENDER:
        return OWNER_RENDER[name](res)
    if name in ATTACHED or name == "upgrade":
        return render_attached(name, res)
    if name == "trickle":
        return render_trickle(res)
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
        rows = [[b, fmt(m["new_ms"]), fmt(m["kill_ms"]), fmt(m["gone_ms"]), fmt(m["rss_mb"])] for b, m in res["results"].items()]
        out.append(table(["binary", "new -d ms", "kill-server returns ms", "pid gone ms", "RSS MB"], rows))
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
    if sys.argv[1:] == ["--smoke"]:
        import bench_smoke  # the regression guard lives in scripts/bench_smoke.py

        sys.exit(bench_smoke.main())
    scenarios = sys.argv[1:] or ALL
    for s in scenarios:
        if s not in ALL + OPTIONAL:
            sys.exit(f"unknown scenario {s} (one of: {' '.join(ALL + OPTIONAL)})")
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
    funcs.update(OWNER_FUNCS)
    funcs.update(ATTACHED)
    funcs["trickle"] = scenario_trickle
    funcs["upgrade"] = scenario_upgrade
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
