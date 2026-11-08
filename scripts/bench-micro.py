#!/usr/bin/env python3
"""Micro-benchmarks of the vtmux server against tmux 3.8 that bench.py does not cover
(docs/PERFORMANCE.md, *Micro-benchmarks*). Reuses bench.py's servers, clients and probes.

Usage: mise exec -- python3 scripts/bench-micro.py <scenario>...
Scenarios:
  flood       FLOOD_PANES (8) panes in one window each `cat` FLOOD_MB (20) MB of log text at once,
              one 200x50 client (sync on): wall, server CPU, terminal bytes
  many_panes  MANY_PANES ("50 100") panes of agent_tui.py, 2 per window, one 200x50 client on window 0:
              server CPU per 10 s, frames, RSS
  clients     CLIENTS_N ("16 32") 120x40 clients on one session of 4 agent_tui.py panes: server CPU, RSS
  status      STATUS_WINDOWS (20) windows of idle `cat`, status-interval 1, a heavy status format
              (window and session loops, conditionals, strftime, #{E:}), STATUS_CLIENTS (4) clients:
              server CPU over STATUS_SECS (20)
  jobs        status-interval 1 with STATUS_JOBS (8) different #() jobs in status-right, 2 clients:
              server CPU (and jobs started) over STATUS_SECS
  memgrowth   MEM_PANES (10) agent_tui.py panes, one 200x50 client, MEM_SECS (1800) s, server RSS every
              MEM_SAMPLE (60) s, for both binaries at once
Env: VTMUX, VTMUX_REFERENCE_TMUX, BASE, RUNS (3), as bench.py. NO_BUILD is implied: build first.
"""
import os
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import bench  # noqa: E402
from bench import (CLIENT_COLS, CLIENT_ROWS, Attached, Client, Env, Phase, _logfile, _trigger,  # noqa: E402
                   _wait_markers, attach_all, cpu_ms, med, pin_terminal, rss_kb)

EMITTER = os.path.join(bench.ROOT, "scripts", "agent_tui.py")


def env_list(name, default):
    return [int(x) for x in os.environ.get(name, default).split()]


def wait_clients(s, n):
    deadline = time.monotonic() + 30
    while len(s.run("list-clients", "-F", "x").split()) < n:
        if time.monotonic() > deadline:
            raise RuntimeError("clients did not attach")
        time.sleep(0.05)


def agent_cmd(i):
    return f"exec python3 '{EMITTER}' {i + 1}"


def agent_session(s, n, per_window=2):
    s.run("new", "-d", "-s", "a", "-n", "w0", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS - 1), agent_cmd(0))
    s.find_pid()
    pin_terminal(s)
    s.run("set", "-g", "default-size", f"{CLIENT_COLS}x{CLIENT_ROWS - 1}", ";", "set", "-g", "automatic-rename", "off")
    s.run("set", "-g", "pane-border-format", "agent")
    for i in range(1, n):
        if i % per_window:
            s.run("splitw", "-d", "-t", f"a:w{i // per_window}", agent_cmd(i))
            s.run("select-layout", "-t", f"a:w{i // per_window}", "tiled")
        else:
            s.run("neww", "-d", "-t", "a:", "-n", f"w{i // per_window}", agent_cmd(i))


def steady(s, clients, secs):
    with Attached(s, clients) as a:
        time.sleep(secs)
        a.term_bytes = sum(c.bytes for c in clients) - a.b0
        a.bsu = sum(c.bsu for c in clients) - a.f0
    return {"server_cpu_s_per_10s": a.p.cpu / 100 / a.p.wall, "frames_per_s": a.bsu / a.p.wall,
            "term_kb_per_s": a.term_bytes / 1024 / a.p.wall, "rss_mb": rss_kb(s.pid) / 1024,
            "wakeups_per_s": None if a.p.wakeups is None else a.p.wakeups / a.p.wall}


def scenario_flood(env):
    n, mb = int(os.environ.get("FLOOD_PANES", "8")), int(os.environ.get("FLOOD_MB", "20"))
    path = _logfile(env, "plain", mb)

    def one(which):
        s = env.server(which)
        s.run("new", "-d", "-s", "f", "-x", str(CLIENT_COLS), "-y", str(CLIENT_ROWS - 1), _trigger(path))
        s.find_pid()
        clients = []
        try:
            pin_terminal(s)
            for _ in range(n - 1):
                s.run("splitw", "-d", "-t", "f:0", _trigger(path), ";", "select-layout", "-t", "f:0", "tiled")
            ids = s.run("list-panes", "-t", "f:0", "-F", "#{pane_id}").split()
            clients = attach_all(s, "f", [(CLIENT_COLS, CLIENT_ROWS)], True)
            time.sleep(1)
            with Attached(s, clients) as a:
                t0 = time.monotonic()
                cmd = []
                for i in ids:
                    cmd += ["send-keys", "-t", i, "Enter", ";"]
                s.run(*cmd[:-1])
                _wait_markers(s, ids, "flood")
                wall = time.monotonic() - t0
                a.finish()
            return {"wall_s": wall, "server_cpu_s": a.p.cpu / 1000, "term_mb": a.term_bytes / 1e6,
                    "sync_frames": a.bsu, "peak_rss_mb": a.p.peak / 1024}
        finally:
            s.kill()
            for c in clients:
                c.close()
    return {f"{n} panes x {mb} MB": env.per_binary(one)}


def scenario_many_panes(env):
    out = {}
    for n in env_list("MANY_PANES", "50 100"):
        def one(which, n=n):
            s = env.server(which)
            clients = []
            try:
                agent_session(s, n)
                clients = [Client(s, "a", CLIENT_COLS, CLIENT_ROWS, True)]
                wait_clients(s, 1)
                time.sleep(3)
                return steady(s, clients, 10)
            finally:
                s.kill()
                for c in clients:
                    c.close()
        out[f"{n} agent panes / 1 client"] = env.per_binary(one)
    return out


def scenario_clients(env):
    out = {}
    for nc in env_list("CLIENTS_N", "16 32"):
        def one(which, nc=nc):
            s = env.server(which)
            clients = []
            try:
                agent_session(s, 4, per_window=4)
                s.run("set", "-g", "window-size", "smallest")
                clients = [Client(s, "a", 120, 40, True) for _ in range(nc)]
                wait_clients(s, nc)
                time.sleep(3)
                return steady(s, clients, 10)
            finally:
                s.kill()
                for c in clients:
                    c.close()
        out[f"4 agent panes / {nc} clients"] = env.per_binary(one)
    return out


HEAVY_STATUS_LEFT = ("#[fg=green]#{session_name}#[default] #{?client_prefix,#[reverse]PFX#[noreverse],} "
                     "#{S:#{?session_attached,*,}#{session_name}#{?#{==:#{session_name},#{client_session}},<,} }")
HEAVY_STATUS_RIGHT = ("#{?window_zoomed_flag,Z,} #{=/20/...:pane_title} #{pane_current_command} "
                      "#{E:@mystyle}#{t:client_activity} %Y-%m-%d %H:%M:%S "
                      "#{W:#{?window_active,[#{window_index}],#{window_index}}} #{P:#{pane_index}}")


def status_server(env, which, windows):
    s = env.server(which)
    s.run("new", "-d", "-s", "s", "-n", "w0", "-x", "200", "-y", "49", "cat", "-")
    s.find_pid()
    pin_terminal(s)
    for i in range(1, windows):
        s.run("neww", "-d", "-t", "s:", "-n", f"w{i}", "cat", "-")
    for i in range(4):
        s.run("new", "-d", "-s", f"o{i}", "-n", "x", "cat", "-")
    s.run("set", "-g", "@mystyle", "#[fg=colour#{window_index}]#{session_windows}")
    s.run("set", "-g", "status-left-length", "200", ";", "set", "-g", "status-right-length", "300")
    s.run("set", "-g", "status-left", HEAVY_STATUS_LEFT)
    s.run("set", "-g", "window-status-format", "#I:#W#{?window_flags,#{window_flags}, }#{?#{>:#{window_panes},1},+,}")
    s.run("set", "-g", "window-status-current-format", "#[bold]#I:#W#F#[nobold]")
    return s


def run_status(env, windows, nclients, setup, secs):
    def one(which):
        s = status_server(env, which, windows)
        clients = []
        try:
            setup(s)
            clients = [Client(s, "s", CLIENT_COLS, CLIENT_ROWS, True) for _ in range(nclients)]
            wait_clients(s, nclients)
            time.sleep(3)
            return steady(s, clients, secs)
        finally:
            s.kill()
            for c in clients:
                c.close()
    return env.per_binary(one)


def scenario_status(env):
    windows, nc = int(os.environ.get("STATUS_WINDOWS", "20")), int(os.environ.get("STATUS_CLIENTS", "4"))
    secs = float(os.environ.get("STATUS_SECS", "20"))

    def setup(s):
        s.run("set", "-g", "status-right", HEAVY_STATUS_RIGHT, ";", "set", "-g", "status-interval", "1")
    return {f"heavy status, {windows} windows, {nc} clients, interval 1": run_status(env, windows, nc, setup, secs)}


def scenario_jobs(env):
    n, secs = int(os.environ.get("STATUS_JOBS", "8")), float(os.environ.get("STATUS_SECS", "20"))

    def setup(s):
        right = " ".join(f"#(echo job{i} $(date +%%S))" for i in range(n))
        s.run("set", "-g", "status-right", right, ";", "set", "-g", "status-interval", "1")
    return {f"{n} #() jobs, interval 1, 2 clients": run_status(env, 5, 2, setup, secs)}


def scenario_memgrowth(env):
    n = int(os.environ.get("MEM_PANES", "10"))
    secs, step = float(os.environ.get("MEM_SECS", "1800")), float(os.environ.get("MEM_SAMPLE", "60"))
    servers, clients, series = {}, [], {}
    try:
        for which in env.bins:
            s = env.server(which)
            agent_session(s, n)
            servers[which] = s
            clients.append(Client(s, "a", CLIENT_COLS, CLIENT_ROWS, True))
            wait_clients(s, 1)
        t0 = time.monotonic()
        c0 = {w: cpu_ms(s.pid) for w, s in servers.items()}
        while True:
            t = time.monotonic() - t0
            for w, s in servers.items():
                series.setdefault(w, []).append((round(t), rss_kb(s.pid) / 1024, (cpu_ms(s.pid) - c0[w]) / 1000))
            print(" ".join(f"{w} {series[w][-1]}" for w in servers), file=sys.stderr, flush=True)
            if t >= secs:
                break
            time.sleep(min(step, secs - t))
    finally:
        for s in servers.values():
            s.kill()
        for c in clients:
            c.close()
    return {"series": series}


SCEN = {"flood": scenario_flood, "many_panes": scenario_many_panes, "clients": scenario_clients,
        "status": scenario_status, "jobs": scenario_jobs, "memgrowth": scenario_memgrowth}


def main():
    names = sys.argv[1:]
    for n in names:
        if n not in SCEN:
            sys.exit(f"unknown scenario {n} (one of: {' '.join(SCEN)})")
    ref = os.environ.get("VTMUX_REFERENCE_TMUX") or os.path.expanduser("~/.cache/vtmux/tmux-3.8/bin/tmux")
    bins = {"tmux": ref}
    if os.environ.get("BASE"):
        bins["base"] = os.path.abspath(os.environ["BASE"])
    bins["vtmux"] = os.path.abspath(os.environ.get("VTMUX") or os.path.join(bench.ROOT, "target/release/vtmux"))
    env = Env(bins)
    print(f"load {os.getloadavg()[0]:.1f}, runs {bench.RUNS}", flush=True)
    try:
        for name in names:
            res = SCEN[name](env)
            print(f"== {name}", flush=True)
            if name == "memgrowth":
                for w, ser in res["series"].items():
                    print(f"{w}: " + " ".join(f"{t}s={r:.1f}MB/{c:.1f}s" for t, r, c in ser), flush=True)
                continue
            for row, per in res.items():
                for which, m in per.items():
                    print(f"{row:45} {which:6} " + " ".join(f"{k}={v:.3g}" if isinstance(v, float) else f"{k}={v}"
                                                         for k, v in m.items()), flush=True)
    finally:
        import shutil
        shutil.rmtree(env.work, ignore_errors=True)


if __name__ == "__main__":
    main()
