"""Performance regression guard: `python3 scripts/bench.py --smoke` (docs/PERFORMANCE.md).

A short subset of the bench scenarios (about 2 minutes), compared against perf/baseline.json
(Linux only), which is recorded on the CI runner: the perf-smoke job uploads its JSON as an
artifact and a refresh is a copy of that file.

Gated metrics (all vtmux):
  idle_rss_mb         RSS of a server with one session (scenario_idle)
  scale_rss_mb        RSS with 400 idle panes (scenario_scale)
  history_spec_rss_mb RSS after 150k lines of scrollback (bench-history-memory.sh spec)
      limit = baseline + max(25%, 8 MB). RSS is nearly deterministic for one binary and input.
  cpu_ratio_plain, cpu_ratio_sgr
      server CPU of vtmux / tmux 3.8 for the same pane output (throughput scenario, no client),
      measured in the same run. Limit = baseline ratio * 1.5.

Why a ratio and not cachegrind instruction counts: both binaries run in the same job, so the ratio
cancels the machine's speed and its noisy neighbours without a valgrind install, it works on macOS
too, and it measures the real server (threads, syscalls, the PTY), not a foreground -D instance.
The cost is a coarser threshold (+50%), which only catches a real slowdown, not drift.

Exit status: 1 when a gated metric is over its limit, else 0 (also without a baseline: the
result is then only recorded). Env: OUT (result JSON, default target/bench/smoke.json),
GITHUB_STEP_SUMMARY (the table is appended), BASELINE (default perf/baseline.json), NO_BUILD=1, VTMUX,
VTMUX_REFERENCE_TMUX.
"""
import json
import os
import re
import shutil
import subprocess
import sys

import bench

ROOT = bench.ROOT
BASELINE = os.environ.get("BASELINE") or os.path.join(ROOT, "perf", "baseline.json")
RSS_PCT, RSS_FLOOR_MB, RATIO_PCT = 0.25, 8.0, 0.5
GATED = {  # name -> kind
    "idle_rss_mb": "rss", "scale_rss_mb": "rss", "history_spec_rss_mb": "rss",
    "cpu_ratio_plain": "ratio", "cpu_ratio_sgr": "ratio",
}


def limit(kind, base):
    if kind == "rss":
        return base + max(base * RSS_PCT, RSS_FLOOR_MB)
    return base * (1 + RATIO_PCT)


def measure(bins):
    """Run the scenarios; returns (gated metrics, informational metrics)."""
    only = lambda which: {which: bins[which]}  # noqa: E731
    m, info = {}, {}
    bench.RUNS, bench.IDLE_SECS = 1, 3
    env = bench.Env(only("vtmux"))
    try:
        for name, fn in (("idle", bench.scenario_idle), ("scale", bench.scenario_scale)):
            print(f"== smoke {name}", file=sys.stderr, flush=True)
            r = fn(env)["results"]["vtmux"]
            m[f"{name}_rss_mb"] = r["rss_mb"]
            if name == "scale":
                info["scale_rss_kb_per_pane"] = r["rss_kb_per_pane"]
        print("== smoke history", file=sys.stderr, flush=True)
        os.environ["HISTORY_SCENARIOS"] = "spec"
        henv = bench.Env(bins)
        try:
            h = bench.scenario_history(henv)["history"]["spec"]
        finally:
            shutil.rmtree(henv.work, ignore_errors=True)
        m["history_spec_rss_mb"] = h["vtmux"]["rss_mb"]
        info["history_spec_rss_mb_tmux"] = h["tmux"]["rss_mb"]
        print("== smoke throughput", file=sys.stderr, flush=True)
        bench.RUNS, bench.THROUGHPUT_MB = 3, 30
        tenv = bench.Env(bins)
        try:
            t = bench.scenario_throughput(tenv)["throughput"]
        finally:
            shutil.rmtree(tenv.work, ignore_errors=True)
        for kind, per in t.items():
            m[f"cpu_ratio_{kind}"] = per["vtmux"]["cpu_s"] / per["tmux"]["cpu_s"]
            info[f"cpu_s_{kind}_vtmux"] = per["vtmux"]["cpu_s"]
            info[f"cpu_s_{kind}_tmux"] = per["tmux"]["cpu_s"]
    finally:
        shutil.rmtree(env.work, ignore_errors=True)
    return m, info


def git(*args):
    return bench.sh("git", *args)


def staleness():
    """PERFORMANCE.md's numbers are stale when the commit it records is older than the last
    change to perf/baseline.json (that change is not an ancestor of the recorded commit)."""
    try:
        text = open(os.path.join(ROOT, "docs", "PERFORMANCE.md")).read()
    except OSError:
        return None
    mt = re.search(r"commit `([0-9a-f]{7,40})`", text)
    changed = git("log", "-1", "--format=%H", "--", "perf/baseline.json")
    if not mt or not changed:
        return None
    if subprocess.run(["git", "cat-file", "-e", mt.group(1) + "^{commit}"], cwd=ROOT, capture_output=True).returncode:
        return None
    ok = subprocess.run(["git", "merge-base", "--is-ancestor", changed, mt.group(1)], cwd=ROOT).returncode == 0
    return None if ok else (
        f"docs/PERFORMANCE.md records commit {mt.group(1)}, older than the last change to perf/baseline.json "
        f"({changed[:7]}): its numbers (and the README's) are stale, re-run the full bench")


def report(m, info, base):
    rows, bad = [], []
    for name, kind in GATED.items():
        v, b = m[name], (base or {}).get(name)
        unit = "MB" if kind == "rss" else "x"
        if b is None:
            rows.append(f"| {name} | {v:.2f} {unit} | - | - | no baseline |")
            continue
        lim = limit(kind, b)
        ok = v <= lim
        if not ok:
            bad.append(name)
        rows.append(f"| {name} | {v:.2f} {unit} | {b:.2f} | {lim:.2f} | {'ok' if ok else '**REGRESSION**'} ({(v / b - 1) * 100:+.0f}%) |")
    text = "| metric | now | baseline | limit | |\n|---|---|---|---|---|\n" + "\n".join(rows) + "\n"
    text += "\ninfo: " + ", ".join(f"{k} {v:.2f}" for k, v in info.items()) + "\n"
    return text, bad


def main():
    if not bench.LINUX:
        print("smoke: Linux only (the baseline is recorded on the Linux CI runner)")
        return 0
    if not os.environ.get("NO_BUILD"):
        subprocess.run(["cargo", "build", "--release", "-q", "-p", "vtmux", "--bin", "vtmux"], cwd=ROOT, check=True)
        subprocess.run(["cargo", "build", "--release", "-q", "-p", "vtmux-vt", "--example", "history_retention"], cwd=ROOT, check=True)
    ref = os.environ.get("VTMUX_REFERENCE_TMUX") or os.path.join(
        os.environ.get("XDG_CACHE_HOME") or os.path.expanduser("~/.cache"), "vtmux/tmux-3.8/bin/tmux")
    if not os.access(ref, os.X_OK):
        sys.exit(f"no reference tmux at {ref} (cargo xtask ref-tmux)")
    vt = os.path.abspath(os.environ.get("VTMUX") or os.path.join(ROOT, "target/release/vtmux"))
    bins = {"tmux": ref, "vtmux": vt}
    meta = bench.meta(bins)
    if meta["load_at_start"] >= 4:
        print(f"warning: load average {meta['load_at_start']:.1f} >= 4; the CPU ratio is noisy", file=sys.stderr)
    m, info = measure(bins)
    base = None
    if os.path.exists(BASELINE):
        with open(BASELINE) as f:
            base = json.load(f)["metrics"]
    text, bad = report(m, info, base)
    if base is None:
        text += "\nno perf/baseline.json: result recorded only. Copy the JSON artifact of a CI run there.\n"
    stale = staleness()
    if stale:
        text += f"\nSTALE: {stale}\n"
    head = f"smoke run: {meta['date']}, commit {meta['commit']}, {meta['cpu']} ({meta['cpus']} cpus), load {meta['load_at_start']:.1f}\n\n"
    print(head + text)
    out = os.environ.get("OUT") or os.path.join(ROOT, "target", "bench", "smoke.json")
    os.makedirs(os.path.dirname(out), exist_ok=True)
    with open(out, "w") as f:
        json.dump({"meta": meta, "metrics": m, "info": info}, f, indent=2)
        f.write("\n")
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as f:
            f.write("### perf smoke\n\n" + head + text)
    if bad:
        print(f"PERF REGRESSION: {', '.join(bad)}", file=sys.stderr)
        return 1
    return 0
