#!/usr/bin/env bash
# Scrollback memory benchmark (docs/specs/2026-10-31-history-memory.md):
# vtmux against the reference tmux 3.8, one server per binary and scenario.
#
# Scenarios (name cols rows history-limit generator):
#   spec    120x40,  limit 100000, 150k log lines of ~100 characters
#   narrow  80x24,   limit 10000,  12k lines, every cell its own truecolour
#   wide    500x24,  limit 10000,  12k lines of ~450 characters
#   flood   120x40,  limit 2000,   1M log lines of ~60 characters (the
#           output time is the number: pruning cost; not run by default)
#   flood100k  the same with limit 100000
#   big     120x40,  limit 1000000, 1.5M log lines of the spec kind (not run
#           by default; give it SETTLE=900: the compression pass is ten times
#           the spec one)
#
# Per binary it reports: server RSS 0.5 s after the output (rss_out, before
# any compression), after IDLE seconds of idle (rss_mb, the criterion 1
# number; VmRSS, or ps rss on macOS) and once it stops moving (settled, with
# the seconds since the output ended: vtmux compresses 5 s after the output
# stops and a pass takes a few seconds more), #{history_size}, and the
# seconds from new-session until the pane printed DONE. For vtmux it adds
# the history rows Ghostty retains (the `history_retention` example fed the
# same bytes), the number criterion 2 bounds.
#
# Other modes (spec, criteria 3 and 6), first argument:
#   copymode [scenario...]  `copy-mode` entry time on the history as the output
#           left it and again on fully compressed history (criterion 3; the
#           bar is compressed <= 2x uncompressed), then in the same copy mode
#           `search-backward` for a string that is nowhere in the history (a
#           scan of all of it) and `history-top`; on the compressed server the
#           RSS in copy mode after those, and settled again after `cancel`.
#           Default scenarios: spec narrow.
#   session [scenario]  SESSION_PANES (default 10) windows of one session,
#           each pane writing the scenario's output at the same time (default
#           session100k: 100k log lines each, limit 100000, 120x40): total
#           server RSS right after the output, after IDLE and settled.
#   steps [scenario...]  the `compress_steps` example: step count, longest step
#           (criterion 6 bounds it at 5 ms) and capture identity.
#   latency [scenario...]  busy-pane PTY-to-client latency, alternating
#           NOCOMPRESS (a build without compression: scripts/bench-nocompress.patch)
#           and VTMUX, PAIRS pairs (default 4), for GAP_MS 2 (the gate command;
#           the loop is never quiet, so the history panes compress only once
#           starved, from about 8 s after the fill: compress.rs `STARVE`) and
#           60 (quiet gaps: compression runs in them); see
#           scripts/bench-compress-latency.py. Criterion 6 bounds the p95 rise
#           at 10%. Scenarios spec, narrow (default spec).
#
# Usage: scripts/bench-history-memory.sh [scenario...]
#        scripts/bench-history-memory.sh copymode|steps|latency|session [scenario...]
# Env: VTMUX (default target/release/vtmux), VTMUX_REFERENCE_TMUX (default
# the `cargo xtask ref-tmux` cache), IDLE (default 10), RUNS (default 1:
# runs per binary, the fastest is shown), BASE (another vtmux binary to
# compare, e.g. a build of the previous commit; shown as `base`), DEADLINE (default 600:
# seconds to wait for one run's DONE marker before failing), SETTLE (default 60:
# seconds to wait for the RSS to stop moving), COPIES (default 3: servers, hence
# copy-mode entries per state), PAIRS, NOCOMPRESS, LAT_SECS (default 30: seconds per
# latency run; GAP_MS 2 compresses from about 8 s on). Run cargo through
# `mise exec --`: the script builds the release binaries first.
#
# The script can be sourced (it then only defines functions); scripts/bench.sh's
# `history` scenario runs it as a command and parses the table.

gen_log() {
  awk -v n="$1" 'BEGIN { for (i = 0; i < n; i++)
    printf "2026-10-31T12:%02d:%02d.%03dZ INFO  worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=%d\n",
      (i / 60000) % 60, (i / 1000) % 60, i % 1000, i % 16, i * 2654435761 % 4294967296, i, (i * 37) % 100000 }'
}
gen_spec() { gen_log 150000; }
gen_big() { gen_log 1500000; }
gen_session100k() { gen_log 100000; }
gen_narrow() {
  awk 'BEGIN { for (i = 0; i < 12000; i++) { s = sprintf("%05d", i)
    for (j = 0; j < 70; j++) s = s sprintf("\033[38;2;%d;%d;%dm%c", i % 256, j, (i * 7) % 256, 65 + j % 26)
    print s "\033[m" } }'
}
gen_flood() {
  awk 'BEGIN { for (i = 1; i <= 1000000; i++)
    printf "line %d some log text here status=200 path=/api/v1/items/%d\n", i, i }'
}
gen_flood100k() { gen_flood; }
gen_wide() {
  awk 'BEGIN { for (i = 0; i < 12000; i++) { s = sprintf("%05d", i)
    while (length(s) < 450) s = s " " i; print s } }'
}

rss_kb() {
  if [ -r "/proc/$1/status" ]; then
    awk '/^VmRSS:/ { print $2 }' "/proc/$1/status"
  else
    ps -o rss= -p "$1" | tr -d ' '
  fi
}

now() { date +%s.%N 2>/dev/null || date +%s; }

# settle PID MIN_WAIT: after MIN_WAIT s, wait until the RSS has moved by at
# most 512 KB between polls for 5 s (or SETTLE s have passed). Prints the RSS in KB.
settle() {
  local pid=$1 last=-1 cur stable=0 start
  sleep "$2"
  start=$SECONDS
  while [ $((SECONDS - start)) -lt "${SETTLE:-60}" ]; do
    cur=$(rss_kb "$pid")
    if [ "$last" -ge 0 ] && [ $((cur > last ? cur - last : last - cur)) -le 512 ]; then
      stable=$((stable + 1))
    else
      stable=0
    fi
    last=$cur
    [ "$stable" -ge 10 ] && break
    sleep 0.5
  done
  echo "$cur"
}

# run BIN LABEL COLS ROWS LIMIT FILE
#   -> "rss_idle_kb history_size seconds rss_out_kb rss_settled_kb settle_s"
# (settle_s counts from the end of the output)
run() {
  local bin=$1 label=$2 cols=$3 rows=$4 limit=$5 file=$6
  local t=(env -u TMUX -u TMUX_PANE -u EDITOR -u VISUAL SHELL=/bin/sh
    TMUX_TMPDIR="$work" "$bin" -L "$label" -f /dev/null)
  local start end pid
  start=$(now)
  "${t[@]}" new -d -s keep cat \; set -g history-limit "$limit" \; \
    new -d -s b -x "$cols" -y "$rows" "cat '$file'; echo DONE; exec cat"
  # A dead server makes capture-pane fail forever: give up loudly after DEADLINE s.
  local give_up=$((SECONDS + ${DEADLINE:-600}))
  until "${t[@]}" capture-pane -p -t b 2>/dev/null | grep -q '^DONE$'; do
    if [ "$SECONDS" -ge "$give_up" ]; then
      echo "bench-history-memory.sh: $label: no DONE marker within ${DEADLINE:-600} s (server dead?)" >&2
      exit 1
    fi
    sleep 0.05
  done
  end=$(now)
  pid=$("${t[@]}" display -p -t b '#{pid}')
  sleep 0.5
  local out idle settled
  out=$(rss_kb "$pid")
  sleep "$IDLE"
  idle=$(rss_kb "$pid")
  hs=$("${t[@]}" display -p -t b '#{history_size}')
  settled=$(settle "$pid" 0)
  echo "$idle $hs $(echo "$end - $start" | bc) $out $settled $(echo "$(now) - $end" | bc | cut -d. -f1)"
  "${t[@]}" kill-server
}

# fill BIN LABEL COLS ROWS LIMIT FILE: start a server with the file's output
# in pane b and wait for it; sets t (the command prefix) and pid.
fill() {
  local bin=$1 label=$2 cols=$3 rows=$4 limit=$5 file=$6
  t=(env -u TMUX -u TMUX_PANE -u EDITOR -u VISUAL SHELL=/bin/sh
    TMUX_TMPDIR="$work" "$bin" -L "$label" -f /dev/null)
  "${t[@]}" new -d -s keep cat \; set -g history-limit "$limit" \; \
    new -d -s b -x "$cols" -y "$rows" "cat '$file'; echo DONE; exec cat"
  local give_up=$((SECONDS + ${DEADLINE:-600}))
  until "${t[@]}" capture-pane -p -t b 2>/dev/null | grep -q '^DONE$'; do
    if [ "$SECONDS" -ge "$give_up" ]; then
      echo "bench-history-memory.sh: $label: no DONE marker within ${DEADLINE:-600} s (server dead?)" >&2
      exit 1
    fi
    sleep 0.05
  done
  pid=$("${t[@]}" display -p -t b '#{pid}')
}

# ms_of CMD...: wall milliseconds of one command
ms_of() {
  local a b
  a=$(now)
  "$@" > /dev/null
  b=$(now)
  echo "($b - $a) * 1000" | bc -l | awk '{ printf "%.0f\n", $1 }'
}

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

# copymode_run BIN LABEL COLS ROWS LIMIT FILE -> one line:
#   "noise_ms uncompressed_ms compressed_ms rss_uncompressed_kb rss_compressed_kb
#    search_u_ms top_u_ms search_c_ms top_c_ms rss_in_copy_c_kb rss_after_c_kb"
# Two servers, since a history read restores the pages for good. First: the
# `copy-mode` entry comes right after the output, before the 5 s idle delay,
# so nothing is compressed yet. Second: the entry comes after the RSS stopped
# falling; the RSS shown is taken just before the entry. On each, the entry is
# followed by a search-backward for a string that is not in the history (it
# scans every line, wrapping) and history-top; on the second the RSS is taken
# in copy mode after them, and settled again after cancel.
ABSENT='no-such-text-in-the-history'
copymode_run() {
  local bin=$1 label=$2 cols=$3 rows=$4 limit=$5 file=$6 noise un co rss_u rss_c i su tu sc tc rss_in rss_after
  fill "$bin" "$label-u" "$cols" "$rows" "$limit" "$file"
  noise=$(for i in 1 2 3 4 5; do ms_of "${t[@]}" display -p ok; done | sort -n | head -1)
  rss_u=$(rss_kb "$pid")
  un=$(ms_of "${t[@]}" copy-mode -t b)
  su=$(ms_of "${t[@]}" send -t b -X search-backward "$ABSENT")
  tu=$(ms_of "${t[@]}" send -t b -X history-top)
  "${t[@]}" kill-server
  fill "$bin" "$label-c" "$cols" "$rows" "$limit" "$file"
  rss_c=$(settle "$pid" 6)
  co=$(ms_of "${t[@]}" copy-mode -t b)
  sc=$(ms_of "${t[@]}" send -t b -X search-backward "$ABSENT")
  tc=$(ms_of "${t[@]}" send -t b -X history-top)
  rss_in=$(rss_kb "$pid")
  "${t[@]}" send -t b -X cancel
  rss_after=$(settle "$pid" 6)
  "${t[@]}" kill-server
  echo "$noise $un $co $rss_u $rss_c $su $tu $sc $tc $rss_in $rss_after"
}

scenario_params() {
  case $1 in
    spec) cols=120 rows=40 limit=100000 ;;
    narrow) cols=80 rows=24 limit=10000 ;;
    wide) cols=500 rows=24 limit=10000 ;;
    flood) cols=120 rows=40 limit=2000 ;;
    flood100k) cols=120 rows=40 limit=100000 ;;
    big) cols=120 rows=40 limit=1000000 ;;
    session100k) cols=120 rows=40 limit=100000 ;;
    *) echo "unknown scenario $1" >&2; exit 2 ;;
  esac
}

mode_copymode() {
  local s n copies=${COPIES:-3}
  [ $# -gt 0 ] || set -- spec narrow
  printf '%-7s %-7s %7s %9s %9s %9s %6s %9s %9s %9s %9s %9s %9s %9s %9s\n' scenario binary limit noise_ms uncomp_ms comp_ms ratio rss_u_mb rss_c_mb \
    search_u top_u search_c top_c rss_cm_mb rss_end_mb
  for s in "$@"; do
    scenario_params "$s"
    file="$work/$s.txt"
    "gen_$s" > "$file"
    for which in tmux vtmux; do
      case $which in tmux) bin=$REF ;; *) bin=$VTMUX ;; esac
      : > "$work/cm.txt"
      for n in $(seq "$copies"); do
        copymode_run "$bin" "cm-$which-$$-$n" $cols $rows $limit "$file" >> "$work/cm.txt"
      done
      col() { awk -v c="$1" '{ print $c }' "$work/cm.txt" | median; }
      noise=$(col 1) un=$(col 2) co=$(col 3) ru=$(col 4) rc=$(col 5)
      printf '%-7s %-7s %7s %9s %9s %9s %6.2f %9.1f %9.1f %9s %9s %9s %9s %9.1f %9.1f\n' "$s" $which $limit "$noise" "$un" "$co" \
        "$(echo "$co / ($un + ($un == 0))" | bc -l)" "$(echo "$ru / 1024" | bc -l)" "$(echo "$rc / 1024" | bc -l)" \
        "$(col 6)" "$(col 7)" "$(col 8)" "$(col 9)" "$(echo "$(col 10) / 1024" | bc -l)" "$(echo "$(col 11) / 1024" | bc -l)"
      awk -v s="$s" -v w="$which" '{ printf "  run %s %s: entry %s ms uncompressed, %s ms compressed (RSS %.0f -> %.0f MB); search %s / %s ms, top %s / %s ms; RSS in copy mode %.1f MB, after cancel %.1f MB\n", s, w, $2, $3, $4 / 1024, $5 / 1024, $6, $8, $7, $9, $10 / 1024, $11 / 1024 }' "$work/cm.txt"
    done
  done
}

mode_session() {
  local s=${1:-session100k} n=${SESSION_PANES:-10} which bin i start end out idle settled hs secs
  scenario_params "$s"
  file="$work/$s.txt"
  "gen_$s" > "$file"
  binaries=(tmux vtmux)
  [ -z "${BASE:-}" ] || binaries=(tmux base vtmux)
  printf '%-11s %-7s %5s %7s %8s %8s %10s %8s %10s %10s %8s\n' scenario binary panes limit hist_sum output_s rss_out_mb rss_mb settled_mb per_pane_mb settle_s
  for which in "${binaries[@]}"; do
    case $which in tmux) bin=$REF ;; base) bin=$BASE ;; *) bin=$VTMUX ;; esac
    t=(env -u TMUX -u TMUX_PANE -u EDITOR -u VISUAL SHELL=/bin/sh
      TMUX_TMPDIR="$work" "$bin" -L "sess-$which-$$" -f /dev/null)
    local cmd="cat '$file'; echo DONE; exec cat" args=()
    for i in $(seq 2 "$n"); do args+=(\; neww -d -t b: "$cmd"); done
    start=$(now)
    "${t[@]}" new -d -s keep cat \; set -g history-limit "$limit" \; \
      new -d -s b -x "$cols" -y "$rows" "$cmd" "${args[@]}"
    local give_up=$((SECONDS + ${DEADLINE:-600}))
    for i in $(seq 0 $((n - 1))); do
      until "${t[@]}" capture-pane -p -t "b:$i" 2>/dev/null | grep -q '^DONE$'; do
        if [ "$SECONDS" -ge "$give_up" ]; then
          echo "bench-history-memory.sh: session: no DONE marker in b:$i within ${DEADLINE:-600} s" >&2
          exit 1
        fi
        sleep 0.05
      done
    done
    end=$(now)
    pid=$("${t[@]}" display -p -t b '#{pid}')
    sleep 0.5
    out=$(rss_kb "$pid")
    sleep "$IDLE"
    idle=$(rss_kb "$pid")
    hs=$("${t[@]}" list-panes -s -t b -F '#{history_size}' | awk '{ s += $1 } END { print s }')
    settled=$(settle "$pid" 0)
    secs=$(echo "$(now) - $end" | bc | cut -d. -f1)
    "${t[@]}" kill-server
    printf '%-11s %-7s %5s %7s %8s %8.2f %10.1f %8.1f %10.1f %10.1f %8s\n' "$s" $which "$n" $limit "$hs" "$(echo "$end - $start" | bc)" \
      "$(echo "$out / 1024" | bc -l)" "$(echo "$idle / 1024" | bc -l)" "$(echo "$settled / 1024" | bc -l)" \
      "$(echo "$settled / 1024 / $n" | bc -l)" "$secs"
  done
}

mode_steps() {
  local s
  [ $# -gt 0 ] || set -- spec narrow wide
  cargo build --release -q -p vtmux-vt --example compress_steps
  echo "scenario steps max_step_ms total_ms pages rss_before_mb rss_after_mb identical (in-process, one PaneTerm)"
  for s in "$@"; do
    scenario_params "$s"
    file="$work/$s.txt"
    "gen_$s" > "$file"
    echo "$s $(target/release/examples/compress_steps $cols $rows $limit "$file")"
  done
}

mode_latency() {
  local s gap n pairs=${PAIRS:-4} lat=scripts/bench-compress-latency.py
  [ -n "${NOCOMPRESS:-}" ] || { echo "latency: set NOCOMPRESS to a build without compression (scripts/bench-nocompress.patch)" >&2; exit 2; }
  [ $# -gt 0 ] || set -- spec
  for s in "$@"; do
    for gap in 2 60; do
      for n in $(seq "$pairs"); do
        # Alternate the order, so a drifting host load hits both sides alike.
        if [ $((n % 2)) = 1 ]; then order=("$VTMUX" "$NOCOMPRESS"); else order=("$NOCOMPRESS" "$VTMUX"); fi
        for bin in "${order[@]}"; do
          side=with
          [ "$bin" = "$NOCOMPRESS" ] && side=without
          echo "== $s GAP_MS=$gap pair $n $side load $(cut -d' ' -f1 /proc/loadavg 2>/dev/null || uptime)" >&2
          GAP_MS=$gap python3 "$lat" "$bin" "$s" 2 "${LAT_SECS:-30}" > "$work/lat-$s-$gap-$side-$n.json"
        done
      done
      echo "--- $s GAP_MS=$gap, $pairs alternating pairs, 2 panes, ${LAT_SECS:-30} s"
      python3 "$lat" pooled "$work"/lat-"$s"-"$gap"-with-*.json -- "$work"/lat-"$s"-"$gap"-without-*.json
      python3 "$lat" compare "$work"/lat-"$s"-"$gap"-with-*.json -- "$work"/lat-"$s"-"$gap"-without-*.json
      [ -z "${KEEP_RAW:-}" ] || cp "$work"/lat-"$s"-"$gap"-*.json "$KEEP_RAW"/
    done
  done
}

main() {
  set -euo pipefail
  cd "$(dirname "${BASH_SOURCE[0]}")/.."

  IDLE="${IDLE:-10}"
  VTMUX="${VTMUX:-target/release/vtmux}"
  REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
  [ -x "$REF" ] || { echo "no reference tmux at $REF (cargo xtask ref-tmux)" >&2; exit 1; }

  cargo build --release -q -p vtmux --bin vtmux
  cargo build --release -q -p vtmux-vt --example history_retention
  RETENTION=target/release/examples/history_retention

  work="$(mktemp -d)"
  trap 'rm -rf "$work"' EXIT

  case "${1:-}" in
    copymode | steps | latency | session) local mode=$1; shift; "mode_$mode" "$@"; return ;;
  esac
  scenarios=("$@")
  [ ${#scenarios[@]} -gt 0 ] || scenarios=(spec narrow wide)
  # Runs per binary; the table shows the fastest output time (and its RSS).
  RUNS="${RUNS:-1}"

  printf '%-7s %-9s %6s %7s %10s %9s %8s %8s %10s %10s %8s\n' scenario binary size limit rss_mb hist_size retained output_s rss_out_mb settled_mb settle_s
  for s in "${scenarios[@]}"; do
    scenario_params "$s"
    file="$work/$s.txt"
    "gen_$s" > "$file"
    binaries=(tmux vtmux)
    [ -z "${BASE:-}" ] || binaries=(tmux base vtmux)
    for which in "${binaries[@]}"; do
      case $which in tmux) bin=$REF ;; base) bin=$BASE ;; *) bin=$VTMUX ;; esac
      secs=
      for n in $(seq "$RUNS"); do
        read -r rss1 hs1 secs1 out1 set1 settle1 < <(run "$bin" "bench-$which-$$-$n" $cols $rows $limit "$file")
        if [ -z "$secs" ] || [ "$(echo "$secs1 < $secs" | bc)" = 1 ]; then
          rss=$rss1 hs=$hs1 secs=$secs1 out=$out1 set=$set1 settle=$settle1
        fi
      done
      retained=-
      if [ $which = vtmux ]; then
        read -r hs2 retained < <("$RETENTION" $cols $rows $limit "$file")
        [ "$hs2" = "$hs" ] || retained="$retained(hs=$hs2)"
      fi
      printf '%-7s %-9s %6s %7s %10.1f %9s %8s %8.2f %10.1f %10.1f %8s\n' "$s" $which "${cols}x$rows" $limit \
        "$(echo "$rss / 1024" | bc -l)" "$hs" "$retained" "$secs" \
        "$(echo "$out / 1024" | bc -l)" "$(echo "$set / 1024" | bc -l)" "$settle"
    done
  done
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then main "$@"; fi
