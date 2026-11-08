#!/usr/bin/env python3
"""Chaos driver for live upgrade and vtmux-persist (live-upgrade spec, D27).

Usage: python3 scripts/chaos_upgrade.py [--rounds N] [--seed S] [--persist-every K]
                                         [--busy N] [--keep]

Builds a busy vtmux server (streaming panes, alternate-screen programs, copy
mode, popups, menus, prompts, two control clients, a pipe-pane, attached
clients of several sizes, jobs, hooks, dead panes) and upgrades it at random
moments, alternating between the binary under test and a copy of it, with
pane programs and attached clients stopped (SIGSTOP) and CPU hogs running
at random. Every Kth round it also stops the server (kill-server writing a
`vtmux-persist` `full` or `layout` checkpoint, or SIGKILL) and restarts it
from the checkpoint. Invariants checked:

  - no session, window or pane lost or added, no pane process restarted,
    no hook fired;
  - streaming panes' output lost or repeated in neither the pane (its
    history), the control clients' %output nor the pipe-pane file;
  - every attached client the same process and client record afterwards,
    and in a quiet round (everything stopped) each pane's capture and each
    client's screen as before the upgrade;
  - every client's screen equal to a fresh attach of the same size;
  - a persist restore brings back every session, window and pane id, each
    pane dead with its last screen and history (`full`); after a SIGKILL,
    the last finished checkpoint restores without an error.

Attached clients run in panes of an observer server (the reference tmux
3.8, ~/.cache/vtmux/tmux-3.8/bin/tmux or $VTMUX_REFERENCE_TMUX), whose
`capture-pane -e` is the client's screen. Every server runs under a private
TMUX_TMPDIR and -L label (rvchaos*, rvobs*), killed on exit.

Env: VTMUX (target/debug/vtmux), VTMUX_REFERENCE_TMUX.
"""
import argparse
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
VTMUX = os.environ.get("VTMUX", os.path.join(ROOT, "target/debug/vtmux"))
REF = os.environ.get(
    "VTMUX_REFERENCE_TMUX",
    os.path.join(
        os.environ.get("XDG_CACHE_HOME", os.path.expanduser("~/.cache")),
        "vtmux/tmux-3.8/bin/tmux",
    ),
)
SIZES = [(80, 24), (120, 40), (60, 20), (200, 50)]


class Failure(Exception):
    pass


def log(*a):
    print(time.strftime("%H:%M:%S"), *a, flush=True)


class Server:
    """A server on a private socket dir and label, driven through its CLI."""

    def __init__(self, binary, label, tmp, home, extra_env=None):
        self.binary, self.label, self.tmp, self.home = binary, label, tmp, home
        self.env = {
            "HOME": home,
            "TMUX_TMPDIR": tmp,
            "SHELL": "/bin/sh",
            "TERM": "xterm-256color",
            "LC_ALL": "C.UTF-8",
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        }
        self.env.update(extra_env or {})

    def argv(self, *args):
        return [self.binary, "-L", self.label, "-f", "/dev/null", *args]

    def run(self, *args, check=True, timeout=30):
        try:
            out = subprocess.run(
                self.argv(*args),
                env=self.env,
                cwd=self.home,
                stdin=subprocess.DEVNULL,
                capture_output=True,
                timeout=timeout,
            )
        except subprocess.TimeoutExpired as e:
            raise Failure(f"{self.label}: {args} timed out after {timeout}s") from e
        if check and out.returncode != 0:
            raise Failure(
                f"{self.label}: {args} exit {out.returncode}: "
                f"{out.stderr.decode(errors='replace')!r}"
            )
        return out.stdout.decode(errors="replace")

    def try_run(self, *args, timeout=30):
        try:
            return self.run(*args, check=False, timeout=timeout)
        except Failure:
            return ""

    def shell_argv(self, *args):
        """A command line running this server's client, for an observer pane."""
        env = " ".join(f"{k}={sh_quote(v)}" for k, v in self.env.items())
        return f"exec env -i {env} " + " ".join(sh_quote(a) for a in self.argv(*args))

    def kill(self):
        pid = self.try_run("display", "-p", "#{pid}", timeout=10).strip()
        self.try_run("kill-server", timeout=10)
        if pid.isdigit():
            for _ in range(100):
                if not alive(int(pid)):
                    break
                time.sleep(0.05)
            else:
                os.kill(int(pid), signal.SIGKILL)


def sh_quote(s):
    return "'" + s.replace("'", "'\\''") + "'"


def alive(pid):
    try:
        os.kill(pid, 0)
        return True
    except ProcessLookupError:
        return False
    except PermissionError:
        return True


def wait(what, done, limit=30.0):
    end = time.time() + limit
    while True:
        r = done()
        if r:
            return r
        if time.time() > end:
            raise Failure(f"timed out waiting for {what}")
        time.sleep(0.05)


class Control:
    """A control client; its %output per pane decoded as it comes."""

    def __init__(self, server, session):
        self.proc = subprocess.Popen(
            server.argv("-C", "attach", "-t", session),
            env=server.env,
            cwd=server.home,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        )
        self.output = {}
        self.lines = 0
        self.ended = None
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for raw in self.proc.stdout:
            line = raw.rstrip(b"\n")
            with self.lock:
                self.lines += 1
                if line.startswith(b"%output "):
                    _, pane, data = line.split(b" ", 2)
                    self.output.setdefault(pane.decode(), bytearray()).extend(
                        unescape(data)
                    )
                elif line.startswith(b"%exit"):
                    self.ended = line.decode(errors="replace")
        with self.lock:
            if self.ended is None:
                self.ended = "eof"

    def send(self, line):
        self.proc.stdin.write(line.encode() + b"\n")
        self.proc.stdin.flush()

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=10)
        except subprocess.TimeoutExpired:
            self.proc.kill()
            self.proc.wait()


def unescape(data):
    out = bytearray()
    i = 0
    while i < len(data):
        c = data[i]
        if c == 0x5C and len(data[i + 1 : i + 4]) == 3 and data[i + 1 : i + 4].isdigit():
            out.append(int(data[i + 1 : i + 4], 8))
            i += 4
        else:
            out.append(c)
            i += 1
    return bytes(out)


STREAM_RE = re.compile(rb"S(\d+)-(\d+)\.")


def check_sequence(where, data, tag):
    """`data`'s `S<tag>-<n>.` lines run consecutively, none lost or repeated."""
    nums = [int(m.group(2)) for m in STREAM_RE.finditer(data) if int(m.group(1)) == tag]
    if not nums:
        return 0
    for a, b in zip(nums, nums[1:]):
        if b != a + 1:
            raise Failure(f"{where}: stream {tag} jumps {a} -> {b}")
    return len(nums)


class Chaos:
    def __init__(self, args):
        self.args = args
        self.rng = random.Random(args.seed)
        self.root = tempfile.mkdtemp(prefix="vtc", dir="/tmp")
        home = os.path.join(self.root, "home")
        os.makedirs(home)
        os.makedirs(os.path.join(self.root, "s"))
        os.makedirs(os.path.join(self.root, "o"))
        self.copy = os.path.join(self.root, "vtmux-copy")
        shutil.copy2(VTMUX, self.copy)
        self.binaries = [VTMUX, self.copy]
        self.subject = Server(
            VTMUX,
            f"rvchaos{os.getpid()}",
            os.path.join(self.root, "s"),
            home,
            {"XDG_STATE_HOME": os.path.join(self.root, "state")},
        )
        self.observer = Server(REF, f"rvobs{os.getpid()}", os.path.join(self.root, "o"), home)
        self.controls = []
        self.clients = []  # (observer session, subject session, size)
        self.streams = {}  # pane id -> tag
        self.timers = []
        self.tickers = []  # panes whose screen changes every 0.2 s
        self.pipe_file = os.path.join(self.root, "pipe.out")
        self.pipe_tag = None
        self.hogs = []
        self.stats = {"upgrades": 0, "refused": 0, "quiet": 0, "restores": 0}
        self.next_tag = 0
        self.findings = []

    # --- setup -----------------------------------------------------------

    def stream_cmd(self, tag):
        # Numbered lines with a short rest every 200; two words, so the
        # shell is not left in front (oracle/README.md).
        return [
            "sh",
            "-c",
            f"i=0; while :; do echo S{tag}-$i.; i=$((i+1)); "
            f"case $i in *00) sleep 0.05;; esac; done",
        ]

    def new_stream(self, target, split=False):
        tag = self.next_tag
        self.next_tag += 1
        how = ["splitw", "-d", "-t", target] if split else ["neww", "-d", "-t", target, "-n", f"st{tag}"]
        pane = self.subject.run(*how, "-P", "-F", "#{pane_id}", *self.stream_cmd(tag)).strip()
        self.subject.run("select-pane", "-t", pane, "-T", f"st{tag}")
        self.streams[pane] = tag
        return pane

    def setup(self):
        s = self.subject
        s.run("new", "-d", "-s", "s0", "-n", "cat", "-x", "80", "-y", "24", "cat", "-")
        for args in [
            ("set", "-g", "status-right", "R"),
            ("set", "-g", "history-limit", "120000"),
            ("set", "-g", "automatic-rename", "off"),
            ("set", "-g", "allow-rename", "off"),
            ("set", "-s", "vtmux-persist", "full"),
            ("set", "-g", "remain-on-exit", "on"),
            ("set", "-g", "@hooks", "0"),
            ("set-hook", "-g", "pane-died", "set -ga @hooks d"),
            ("set-hook", "-g", "after-new-window", "set -ga @hooks n"),
            ("set-hook", "-g", "client-attached", "set -ga @hooks a"),
            ("setb", "-b", "named", "buffer data"),
            ("bind", "-n", "F5", "display-popup", "-w", "30", "-h", "8", "-T", "pop", "cat", "-"),
            ("bind", "-n", "F6", "display-menu", "-x", "0", "-y", "0", "-T", "m", "one", "1", "display one", "two", "2", ""),
            ("bind", "-n", "F7", "command-prompt", "-p", "chaos", "display %%"),
            ("setenv", "-g", "CHAOS", "1"),
        ]:
            s.run(*args)
        for k, (w, h) in enumerate(SIZES):
            name = f"s{k}"
            if k:
                s.run("new", "-d", "-s", name, "-n", "cat", "-x", str(w), "-y", str(h), "cat", "-")
            s.run("select-pane", "-t", f"{name}:cat", "-T", f"cat{k}")
            self.new_stream(name)
            if k % 2 == 0:
                self.new_stream(f"{name}:st{self.next_tag - 1}", split=True)
            pane = s.run(
                "neww", "-d", "-t", name, "-n", "alt", "-P", "-F", "#{pane_id}",
                "sh", "-c", "printf '\\033[?1049h\\033[2J\\033[HALT'; i=0; "
                f"while :; do printf '\\033[5;5H%s' $i; echo $i > {self.root}/tick{k}; i=$((i+1)); sleep 0.2; done",
            ).strip()
            s.run("select-pane", "-t", pane, "-T", "alt")
            self.tickers.append((pane, f"{self.root}/tick{k}"))
            pane = s.run(
                "neww", "-d", "-t", name, "-n", "less", "-P", "-F", "#{pane_id}",
                "less", "/etc/services",
            ).strip()
            s.run("select-pane", "-t", pane, "-T", "less")
            pane = s.run(
                "neww", "-d", "-t", name, "-n", "dead", "-P", "-F", "#{pane_id}",
                "sh", "-c", "echo bye; exit 3",
            ).strip()
            s.run("select-pane", "-t", pane, "-T", "dead")
        top = s.run("neww", "-d", "-t", "s1", "-n", "top", "-P", "-F", "#{pane_id}", "top", "-d", "0.3").strip()
        s.run("select-pane", "-t", top, "-T", "top")
        # The pipe-pane: the first stream's output to a file as well.
        first = next(iter(self.streams))
        self.pipe_tag = self.streams[first]
        s.run("pipe-pane", "-t", first, f"cat >> {self.pipe_file}")
        s.run("set", "-s", "vtmux-persist", "full")
        for _ in range(2):
            self.controls.append(Control(s, "s0"))
        o = self.observer
        o.run("start-server", ";", "set", "-g", "status", "off", ";", "set", "-g", "remain-on-exit", "on")
        for k, (w, h) in enumerate(SIZES):
            self.attach(f"c{k}", f"s{k}", (w, h))
        # A read-only client beside c0, of its size.
        self.attach("ro", "s0", SIZES[0], flags=("-r",))
        wait("every client", lambda: len(self.client_rows()) == len(self.clients))

    def attach(self, name, session, size, flags=()):
        w, h = size
        self.observer.run(
            "new", "-d", "-s", name, "-x", str(w), "-y", str(h),
            self.subject.shell_argv("attach", *flags, "-t", session),
        )
        self.clients.append((name, session, size))

    def client_rows(self):
        return [
            l for l in self.subject.run(
                "lsc", "-F",
                "#{client_pid} #{client_name} #{client_created} #{client_session} "
                "#{client_width}x#{client_height} #{client_flags} #{client_key_table} "
                "#{client_control_mode}",
            ).splitlines() if l and not l.endswith(" 1")
        ]

    def client_pid(self, name):
        return int(self.observer.run("display", "-p", "-t", name, "#{pane_pid}").strip())

    def observer_dead(self, name):
        return self.observer.run("display", "-p", "-t", name, "#{pane_dead}").strip() == "1"

    # --- observation -----------------------------------------------------

    def structure(self):
        s = self.subject
        return "\n".join(
            [
                s.run("ls", "-F", "#{session_id} #{session_name} #{session_windows}"),
                s.run("lsw", "-a", "-F", "#{window_id} #{session_name}:#{window_index} #{window_name} #{window_layout}"),
                s.run("lsp", "-a", "-F", "#{pane_id} #{pane_pid} #{pane_dead} #{pane_dead_status} #{pane_title}"),
                # No hook fires for a re-attach (decision 21).
                s.run("show", "-gv", "@hooks"),
            ]
        )

    def pane_pids(self):
        out = {}
        for line in self.subject.run("lsp", "-a", "-F", "#{pane_id} #{pane_dead} #{pane_pid}").splitlines():
            pane, dead, *pid = line.split()
            if dead == "0":
                out[pane] = int(pid[0])
        return out

    def captures(self):
        s = self.subject
        out = {}
        for pane in s.run("lsp", "-a", "-F", "#{pane_id}").split():
            out[pane] = s.run("capture-pane", "-p", "-e", "-t", pane) + s.run(
                "display", "-p", "-t", pane,
                "#{cursor_x},#{cursor_y} #{history_size} #{pane_in_mode} #{pane_mode} "
                "#{alternate_on} #{copy_cursor_x},#{copy_cursor_y} #{scroll_position}",
            )
        return out

    def screen(self, name):
        o = self.observer
        return visible(o.run("capture-pane", "-p", "-e", "-t", name)) + o.run(
            "display", "-p", "-t", name, "#{cursor_x},#{cursor_y} #{cursor_flag}"
        )

    def settled_screen(self, name, quiet=0.4, limit=20):
        """The screen once it stopped changing for `quiet` seconds."""
        end = time.time() + limit
        last, since = self.screen(name), time.time()
        while time.time() < end:
            time.sleep(0.1)
            now = self.screen(name)
            if now != last:
                last, since = now, time.time()
            elif time.time() - since >= quiet:
                return now
        return last

    def same_as_fresh(self, name, session, size):
        """`name`'s screen equals that of a fresh attach of its size."""
        w, h = size
        fresh = "fresh"
        self.observer.run(
            "new", "-d", "-s", fresh, "-x", str(w), "-y", str(h),
            self.subject.shell_argv("attach", "-t", session),
        )
        try:
            end = time.time() + 20
            while True:
                f = self.settled_screen(fresh)
                a = self.screen(name)
                if a == f:
                    return
                if time.time() > end:
                    raise Failure(f"client {name} ({session} {w}x{h}) differs from a fresh attach:\n"
                                  f"--- re-attached\n{a}\n--- fresh\n{f}")
        finally:
            self.observer.try_run("kill-session", "-t", fresh)
            wait("the fresh client to go", lambda: len(self.client_rows()) == len(self.clients))

    # --- chaos -----------------------------------------------------------

    def some_pane(self):
        panes = self.subject.run("lsp", "-a", "-F", "#{pane_id}").split()
        return self.rng.choice(panes)

    def some_client(self):
        rows = self.client_rows()
        return self.rng.choice(rows).split()[1] if rows else None

    def action(self, structural=True, overlays=True):
        s, r = self.subject, self.rng
        choices = [
            "copy", "copy_keys", "keys", "resize", "job", "option", "refresh",
            "stop_client", "stop_pane", "stop_server", "cli_popup",
        ]
        if overlays:
            choices += ["popup", "menu", "prompt", "panes", "tree", "message"]
        if structural:
            # select-window resizes a window that was not current
            # (resize.c `recalculate_size`), so it counts as structural.
            choices += ["new_window", "kill_pane", "respawn", "split", "stream", "select"]
        what = r.choice(choices)
        c = self.some_client()
        if what == "copy":
            s.try_run("copy-mode", "-t", self.some_pane())
        elif what == "copy_keys":
            p = self.some_pane()
            for k in r.sample(["cursor-up", "page-up", "begin-selection", "cursor-left", "history-top",
                               "search-backward-text 1", "start-of-line", "cancel"], 3):
                s.try_run("send-keys", "-t", p, "-X", *k.split(" ", 1))
        elif what == "keys":
            p = r.choice(["s0:cat", "s1:cat", "s2:cat"])
            if s.try_run("display", "-p", "-t", p, "#{pane_in_mode}").strip() == "0":
                self.send_keys(p, f"typed{r.randrange(1000)}", "Enter")
        elif what == "resize":
            name, _, (w, h) = r.choice(self.clients)
            s.try_run("refresh-client", "-c", "x")  # exercises the not-found path
            self.observer.try_run("resize-window", "-t", name, "-x", str(w + r.randrange(-3, 4)), "-y", str(h))
            self.observer.try_run("resize-window", "-t", name, "-x", str(w), "-y", str(h))
            # tmux sends the pane its size on a timer (server-client.c
            # `server_client_check_pane_resize`): wait for the last one.
            pid = str(self.client_pid(name))
            wait("the client's size back", lambda: any(
                row.split()[0] == pid and row.split()[4] == f"{w}x{h}" for row in self.client_rows()))
        elif what == "job":
            s.try_run("run-shell", "-b", f"sleep 0.{r.randrange(10)}; tmux set -g @job {r.randrange(100)}")
        elif what == "option":
            k, v = r.choice([("pane-border-status", "top"), ("pane-border-status", "off"),
                             ("status-style", "bg=blue"), ("status-style", "bg=green"),
                             ("status-position", "top"), ("status-position", "bottom")])
            s.try_run("set", "-g", k, v)
        elif what == "refresh" and c:
            s.try_run("refresh-client", "-t", c)
        elif what == "stop_client":
            name = r.choice(self.clients)[0]
            pid = self.client_pid(name)
            os.kill(pid, signal.SIGSTOP)
            self.later(r.random() * 2, lambda: os.kill(pid, signal.SIGCONT))
        elif what == "stop_server":
            pid = int(s.run("display", "-p", "#{pid}").strip())
            os.kill(pid, signal.SIGSTOP)
            self.later(0.2 + r.random() * 1.3, lambda: os.kill(pid, signal.SIGCONT))
        elif what == "cli_popup" and c:
            # A popup whose command client went (killed while it waited):
            # nothing holds an upgrade for it, which carries it.
            p = subprocess.Popen(s.argv("display-popup", "-c", c, "-x", "2", "-y", "2", "-w", "24", "-h", "6",
                                        "-T", "cli", "cat", "-"),
                                 env=s.env, cwd=s.home, stdin=subprocess.DEVNULL,
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.3)
            p.kill()
            p.wait()
        elif what == "stop_pane":
            pids = list(self.pane_pids().values())
            if pids:
                pid = r.choice(pids)
                try:
                    os.killpg(pid, signal.SIGSTOP)
                    self.later(r.random(), lambda: os.killpg(pid, signal.SIGCONT))
                except ProcessLookupError:
                    pass
        elif what == "select":
            sess = f"s{r.randrange(len(SIZES))}"
            s.try_run("select-window", "-t", f"{sess}:{r.choice(['cat', 'alt', 'less', 'dead', 'st0', 'top'])}")
        elif what in ("popup", "menu", "prompt"):
            # By key, as a user opens them: from a command client they hold
            # it until they close (cmd-display-menu.c), which an upgrade
            # waits for.
            name = r.choice(self.clients)[0]
            self.observer.try_run("send-keys", "-t", name, {"popup": "F5", "menu": "F6", "prompt": "F7"}[what])
        elif what == "panes" and c:
            s.try_run("display-panes", "-t", c, "-d", "0")
        elif what == "tree" and c:
            s.try_run("choose-tree", "-t", s.run("display", "-p", "-c", c, "#{pane_id}").strip())
        elif what == "message" and c:
            s.try_run("display-message", "-c", c, "-d", "500", "msg")
        elif what == "new_window":
            sess = f"s{r.randrange(len(SIZES))}"
            p = s.try_run("neww", "-d", "-t", sess, "-P", "-F", "#{pane_id}", "cat", "-").strip()
            if p:
                s.try_run("select-pane", "-t", p, "-T", "nw")
        elif what == "kill_pane":
            # Only extra cat panes, so the named windows stay.
            panes = [l.split()[0] for l in s.run("lsp", "-a", "-F", "#{pane_id} #{pane_title}").splitlines()
                     if l.split()[1:] == ["nw"]]
            if panes:
                s.try_run("kill-pane", "-t", r.choice(panes))
        elif what == "respawn":
            dead = [l.split()[0] for l in s.run("lsp", "-a", "-F", "#{pane_id} #{pane_dead}").splitlines()
                    if l.endswith(" 1")]
            if dead:
                s.try_run("respawn-pane", "-t", r.choice(dead), "sh", "-c", "echo again; exit 4")
        elif what == "split":
            p = s.try_run("splitw", "-d", "-t", f"s{r.randrange(len(SIZES))}:cat", "-P", "-F", "#{pane_id}", "cat", "-").strip()
            if p:
                s.try_run("select-pane", "-t", p, "-T", "nw")
        elif what == "stream" and len(self.streams) < 12:
            self.new_stream(f"s{r.randrange(len(SIZES))}")
        return what

    def send_keys(self, pane, *keys):
        """send-keys on behalf of a client that may type: without -c the
        current client is the most recent, which may be the read-only
        one, and tmux refuses then (cmd-send-keys.c)."""
        c = self.client_name("c0")
        return self.subject.try_run("send-keys", "-c", c, "-t", pane, *keys)

    def later(self, secs, f):
        def run():
            try:
                f()
            except ProcessLookupError:
                pass

        t = threading.Timer(secs, run)
        t.start()
        self.timers.append(t)

    def settle_timers(self):
        """Every SIGCONT a chaos action put off has been sent."""
        for t in self.timers:
            t.join()
        self.timers = []

    def signal_panes(self, sig):
        self.settle_timers()
        for pid in self.pane_pids().values():
            try:
                os.killpg(pid, sig)
            except (ProcessLookupError, PermissionError):
                pass

    def close_overlays(self, popups=True):
        """Close what a fresh attach would not show: popups (unless not
        `popups`: an upgrade carries them), menus, prompts, tree modes,
        display-panes, messages."""
        s = self.subject
        for row in self.client_rows() if popups else []:
            s.try_run("display-popup", "-C", "-c", row.split()[1])
        # A menu or prompt takes an Escape, else the pane does (copy mode
        # leaves on it, cat echoes it); twice, for a menu under a popup
        # (as tmux 3.8 draws them). The Escapes reach the server through
        # each client's terminal: let them land before a `q` goes to a
        # mode they may have closed (the program under it would get it).
        for _ in range(2):
            for name, _, _ in self.clients:
                self.observer.try_run("send-keys", "-t", name, "Escape")
            time.sleep(0.6)
        # Tree modes close on an upgrade by design (snapshot.rs `mode_out`).
        closes = ("tree-mode", "buffer-mode", "client-mode", "options-mode", "view-mode", "panes-mode")
        for _ in range(10):
            # Modes stack (copy mode over a tree): until none is left.
            open_ = [l.split()[0] for l in s.run("lsp", "-a", "-F", "#{pane_id} #{pane_mode}").splitlines()
                     if l.split()[1:2] and l.split()[1] in closes]
            if not open_:
                break
            for pane in open_:
                self.send_keys(pane, "q")
            time.sleep(0.1)
        time.sleep(0.6)  # a message's 500 ms

    def client_name(self, name):
        pid = self.client_pid(name)
        for row in self.client_rows():
            if row.split()[0] == str(pid):
                return row.split()[1]
        raise Failure(f"observer {name}: client pid {pid} not listed")

    # --- the rounds ------------------------------------------------------

    def hog(self, on):
        for p in self.hogs:
            p.kill()
            p.wait()
        self.hogs = []
        if on:
            for _ in range(self.args.busy):
                self.hogs.append(subprocess.Popen(["sh", "-c", "while :; do :; done"]))

    def upgrade(self, binary, background=False):
        """Run vtmux-upgrade, with chaos actions meanwhile when `background`."""
        stop = threading.Event()
        done = []

        def churn():
            while not stop.is_set():
                try:
                    done.append(self.action(structural=False))
                except Failure as e:
                    done.append(f"error {e}")
                time.sleep(self.rng.random() * 0.2)

        t = threading.Thread(target=churn, daemon=True)
        if background:
            t.start()
        try:
            out = subprocess.run(
                self.subject.argv("vtmux-upgrade", "-b", binary),
                env=self.subject.env, cwd=self.subject.home, capture_output=True, timeout=90,
            )
        finally:
            stop.set()
            if background:
                t.join()
        return out.returncode, (out.stdout + out.stderr).decode(errors="replace"), done

    def check_live(self, when):
        """Every streaming and ticking pane's screen still changes: the
        server reads its output."""
        s = self.subject
        self.check_ticks(when)
        panes = list(self.streams) + [p for p, _ in self.tickers]
        first = {p: s.run("capture-pane", "-p", "-t", p) for p in panes}
        stuck = set(panes)
        end = time.time() + 10
        while stuck and time.time() < end:
            time.sleep(0.2)
            for p in list(stuck):
                if s.run("capture-pane", "-p", "-t", p) != first[p]:
                    stuck.discard(p)
        if stuck:
            info = {p: s.run("display", "-p", "-t", p, "#{pane_pid} #{pane_dead} #{session_name}:#{window_name} #{pane_in_mode}") for p in stuck}
            raise Failure(f"{when}: panes no longer read: {info}")

    def check_ticks(self, when):
        """Each ticking pane shows (about) the count its program is at."""
        s = self.subject
        for pane, f in self.tickers:
            for _ in range(3):
                shown = re.findall(r"\d+", s.run("capture-pane", "-p", "-t", pane))
                with open(f) as fh:
                    at = fh.read().strip()
                if shown and at.isdigit() and int(at) - int(shown[0]) <= 3:
                    break
                time.sleep(0.3)
            else:
                raise Failure(f"{when}: {pane} shows {shown} while its program is at {at}")

    def round(self, n):
        r = self.rng
        self.check_live(f"round {n} start")
        for _ in range(r.randrange(3, 12)):
            what = self.action()
            self.check_ticks(f"round {n} after {what}")
        self.hog(r.random() < 0.5)
        quiet = r.random() < 0.35
        binary = self.binaries[n % 2]
        structure = self.structure()
        clients = sorted(self.client_rows())
        pids = self.pane_pids()
        if quiet:
            # Everything stopped: the panes and the client screens must come
            # back exactly. Overlays the upgrade closes by design go first.
            self.close_overlays(popups=False)
            self.signal_panes(signal.SIGSTOP)
            time.sleep(0.3)
            before = self.captures()
            screens = {name: self.settled_screen(name) for name, _, _ in self.clients}
            again = self.captures()
            for pane, cap in before.items():
                if again.get(pane) != cap:
                    raise Failure(f"pane {pane} changed while stopped, before the upgrade:\n{cap}\n---\n{again.get(pane)}\n"
                                  + self.subject.run("display", "-p", "-t", pane, "#{pane_pid}"))
        code, msg, churned = self.upgrade(binary, background=not quiet)
        while code != 0 and "open: close it" in msg:
            # A key binding's prompt, menu or popup holds its client's
            # queue: refused by design (upgrade.rs `upgrade_blockers`).
            self.stats["refused"] += 1
            log(f"round {n}: refused: {msg.strip()!r}")
            self.close_overlays()
            if quiet:
                before = self.captures()
                screens = {name: self.settled_screen(name) for name, _, _ in self.clients}
            code, msg, churned = self.upgrade(binary, background=False)
        self.hog(False)
        log(f"round {n}: upgrade onto {os.path.basename(binary)} {'quiet' if quiet else 'busy'}: "
            f"exit {code} {msg.strip()!r} churn {churned}")
        if code != 0:
            if "jobs" in msg or "command" in msg or "in flight" in msg or "pending" in msg:
                self.stats["refused"] += 1
            else:
                raise Failure(f"upgrade failed: {msg!r}")
        else:
            self.stats["upgrades"] += 1
        try:
            def back():
                self.check_clients_alive()
                return sorted(self.client_rows()) == clients

            wait("every client back", back, limit=30)
        except Failure as e:
            raise Failure(f"{e}: before {clients}, now {sorted(self.client_rows())}") from e
        if self.structure() != structure:
            raise Failure(f"structure changed:\n--- before\n{structure}\n--- after\n{self.structure()}")
        if self.pane_pids() != pids:
            raise Failure(f"pane pids changed {pids} -> {self.pane_pids()}")
        if quiet:
            self.stats["quiet"] += 1
            after = self.captures()
            for pane, cap in before.items():
                if after.get(pane) != cap:
                    raise Failure(f"pane {pane} changed across a quiet upgrade:\n--- before\n{cap}\n--- after\n{after.get(pane)}")
            for name, scr in screens.items():
                now = self.settled_screen(name)
                if now != scr:
                    raise Failure(f"client {name} screen changed across a quiet upgrade:\n--- before\n{scr}\n--- after\n{now}")
            self.close_overlays()
        else:
            self.close_overlays()
            self.signal_panes(signal.SIGSTOP)
            time.sleep(0.3)
        for name, session, size in self.clients:
            self.same_as_fresh(name, session, size)
        self.check_streams()
        self.signal_panes(signal.SIGCONT)

    def check_clients_alive(self):
        for name, _, _ in self.clients:
            if self.observer_dead(name):
                raise Failure(f"client {name} exited:\n{self.screen(name)}")
        return True

    def check_streams(self):
        s = self.subject
        for pane, tag in self.streams.items():
            hist = s.run("capture-pane", "-p", "-J", "-S", "-", "-t", pane).encode()
            check_sequence(f"pane {pane} history", hist, tag)
        for k, c in enumerate(self.controls):
            with c.lock:
                if c.ended:
                    raise Failure(f"control client {k} ended: {c.ended}")
                for pane, tag in self.streams.items():
                    check_sequence(f"control {k} {pane}", bytes(c.output.get(pane, b"")), tag)
        with open(self.pipe_file, "rb") as f:
            check_sequence("pipe-pane file", f.read(), self.pipe_tag)

    def persist_round(self, n):
        """A checkpoint, then a new server restores it: kill-server writes
        one (`full` or `layout` at random), or the server is killed
        (SIGKILL, maybe while it writes a periodic one) and restores the
        last it finished."""
        s, r = self.subject, self.rng
        mode = r.choice(["full", "layout"])
        # A crash restores the last checkpoint: the first round writes one.
        crash = self.stats["restores"] > 0 and r.random() < 0.3
        s.run("set", "-s", "vtmux-persist", mode)
        self.close_overlays()
        self.signal_panes(signal.SIGSTOP)
        time.sleep(0.3)
        fmt = ("#{session_name}:#{window_index}.#{pane_index} #{pane_id} #{window_id} "
               "#{session_id} #{window_layout}" + (" #{pane_title}" if mode == "full" else ""))
        before = s.run("lsp", "-a", "-F", fmt)
        caps = {p: s.run("capture-pane", "-p", "-e", "-S", "-", "-t", p)
                for p in s.run("lsp", "-a", "-F", "#{pane_id}").split()}
        for c in self.controls:
            c.close()
        self.controls = []
        if crash:
            pid = int(s.run("display", "-p", "#{pid}").strip())
            self.signal_panes(signal.SIGCONT)
            # Pane programs die with their terminal; give the periodic
            # checkpoint (60 s) a chance to be cut short.
            time.sleep(r.random() * 2)
            os.kill(pid, signal.SIGKILL)
            wait("the server gone", lambda: not alive(pid))
        else:
            s.kill()
        for name, _, _ in self.clients:
            self.observer.try_run("kill-session", "-t", name)
        self.clients = []
        s.run("start-server")
        wait("the restore", lambda: s.try_run("ls"))
        errors = [l for l in s.run("show-messages").splitlines()
                  if "vtmux-persist:" in l and "vtmux-persist: restored " not in l]
        if crash:
            # The last checkpoint finished may be older than the state: only
            # that it restores, whole.
            if errors:
                raise Failure(f"restore after a crash: {errors}")
            live = set(s.run("lsp", "-a", "-F", "#{pane_id}").split())
            self.streams = {p: t for p, t in self.streams.items() if p in live}
            self.tickers = [(p, f) for p, f in self.tickers if p in live]
            log(f"round {n}: persist ({mode}) restore after SIGKILL ok: "
                f"{len(s.run('lsp', '-a').splitlines())} panes")
        else:
            after = s.run("lsp", "-a", "-F", fmt)
            if after != before:
                raise Failure(f"restore changed the panes:\n--- before\n{before}\n--- after\n{after}\n{errors}")
            for p, cap in caps.items():
                now = s.run("capture-pane", "-p", "-e", "-S", "-", "-t", p)
                if s.run("display", "-p", "-t", p, "#{pane_dead}").strip() != "1":
                    raise Failure(f"restored pane {p} not dead")
                got = now.rstrip("\n").split("\n")
                if mode == "layout":
                    kept = []
                else:
                    kept = cap.rstrip("\n").split("\n")
                # The dead line goes on the last blank row, or after a scroll.
                if not check_restored(kept, got):
                    raise Failure(f"restored pane {p} lost its screen or history ({mode}):\n"
                                  f"--- before\n{cap[-3000:]}\n--- after\n{now[-3000:]}")
            log(f"round {n}: persist ({mode}) restore of {len(caps)} panes ok")
        self.stats["restores"] += 1
        # Back to a live server: every pane respawned with its saved
        # command, each stream under a new tag (its restored screen holds
        # the old numbers), the pipe-pane started again.
        for line in s.run("lsp", "-a", "-F", "#{pane_id} #{window_name}").splitlines():
            pane, name = line.split()
            if name == "dead" or s.run("display", "-p", "-t", pane, "#{pane_dead}").strip() != "1":
                continue
            if pane in self.streams:
                tag = self.next_tag
                self.next_tag += 1
                self.streams[pane] = tag
                s.run("respawn-pane", "-t", pane, *self.stream_cmd(tag))
            else:
                s.run("respawn-pane", "-t", pane)
        first = next(iter(self.streams))
        self.pipe_tag = self.streams[first]
        s.run("pipe-pane", "-t", first, f"cat >> {self.pipe_file}")
        for _ in range(2):
            self.controls.append(Control(s, "s0"))
        for k, (w, h) in enumerate(SIZES):
            self.attach(f"c{k}", f"s{k}", (w, h))
        # A read-only client beside c0, of its size.
        self.attach("ro", "s0", SIZES[0], flags=("-r",))
        wait("every client", lambda: len(self.client_rows()) == len(self.clients))

    def cleanup(self):
        self.hog(False)
        for c in self.controls:
            c.close()
        self.observer.kill()
        self.subject.kill()
        if not self.args.keep:
            shutil.rmtree(self.root, ignore_errors=True)


SGR_RE = re.compile(r"\x1b\[([0-9;:]*)m")


def visible(capture):
    """`capture-pane -e` output as what it shows: a space's foreground
    colour can't be seen, so it is dropped (a redraw writes spaces under
    whatever colour came before, an erase leaves the default)."""
    out = []
    for line in capture.split("\n"):
        fg, rest, row, at = "", {}, [], 0
        for m in list(SGR_RE.finditer(line)) + [None]:
            text = line[at : m.start() if m else len(line)]
            for ch in text:
                key = tuple(sorted(rest.items()))
                row.append(f"{ch}{key}" if ch == " " else f"{ch}{fg}{key}")
            if m is None:
                break
            at = m.end()
            codes = (m.group(1) or "0").split(";")
            i = 0
            while i < len(codes):
                c = int(codes[i] or 0)
                if c == 0:
                    fg, rest = "", {}
                elif c in (38, 48, 58):
                    n = 3 if codes[i + 1] == "5" else 5
                    val = ";".join(codes[i : i + n])
                    if c == 38:
                        fg = val
                    else:
                        rest[c] = val
                    i += n - 1
                elif 30 <= c <= 37 or 90 <= c <= 97:
                    fg = str(c)
                elif c == 39:
                    fg = ""
                elif 40 <= c <= 47 or 100 <= c <= 107:
                    rest[48] = str(c)
                elif c == 49:
                    rest.pop(48, None)
                elif c == 59:
                    rest.pop(58, None)
                elif 21 <= c <= 29:
                    rest.pop(c - 20 if c != 22 else 1, None)
                    if c == 22:
                        rest.pop(2, None)
                else:
                    rest[c] = 1
                i += 1
        # Trailing default spaces are no content.
        while row and row[-1] == " ()":
            row.pop()
        out.append("".join(ch[0] for ch in row) + "\t" + str(hash("|".join(row))))
    return "\n".join(out) + "\n"


def check_restored(kept, got):
    """A restored screen holds the saved rows, the dead line after them."""
    while kept and kept[-1] == "":
        kept.pop()
    return got[: len(kept)] == kept or got[: len(kept) - 1] == kept[1:]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--rounds", type=int, default=20)
    ap.add_argument("--seed", type=int, default=int(time.time()))
    ap.add_argument("--persist-every", type=int, default=0)
    ap.add_argument("--busy", type=int, default=(os.cpu_count() or 2) // 2)
    ap.add_argument("--keep", action="store_true")
    args = ap.parse_args()
    log(f"seed {args.seed}")
    chaos = Chaos(args)
    try:
        chaos.setup()
        for n in range(args.rounds):
            if args.persist_every and n % args.persist_every == args.persist_every - 1:
                chaos.persist_round(n)
            else:
                chaos.round(n)
        log(f"ok {chaos.stats}")
    except Failure as e:
        log(f"FAILED (seed {args.seed}, root {chaos.root}): {e}")
        log(f"stats {chaos.stats}")
        chaos.args.keep = True
        if os.environ.get("CHAOS_HOLD"):
            input("held; press enter to clean up")
        return 1
    finally:
        chaos.cleanup()
    return 0


if __name__ == "__main__":
    sys.exit(main())
