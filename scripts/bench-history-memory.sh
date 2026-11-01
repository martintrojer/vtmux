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
#
# Per binary it reports: server RSS after the output and IDLE seconds of
# idle (VmRSS, or ps rss on macOS), #{history_size}, and the seconds from
# new-session until the pane printed DONE. For vtmux it adds the history
# rows Ghostty retains (the `history_retention` example fed the same
# bytes), the number criterion 2 bounds.
#
# Usage: scripts/bench-history-memory.sh [scenario...]
# Env: VTMUX (default target/release/vtmux), VTMUX_REFERENCE_TMUX (default
# the `cargo xtask ref-tmux` cache), IDLE (default 10), RUNS (default 1:
# runs per binary, the fastest is shown), BASE (another vtmux binary to
# compare, e.g. a build of the previous commit; shown as `base`). Run cargo through
# `mise exec --`: the script builds the release binaries first.
set -euo pipefail
cd "$(dirname "$0")/.."

IDLE="${IDLE:-10}"
VTMUX="${VTMUX:-target/release/vtmux}"
REF="${VTMUX_REFERENCE_TMUX:-${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux}"
[ -x "$REF" ] || { echo "no reference tmux at $REF (cargo xtask ref-tmux)" >&2; exit 1; }

cargo build --release -q -p vtmux --bin vtmux
cargo build --release -q -p vtmux-vt --example history_retention
RETENTION=target/release/examples/history_retention

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

gen_spec() {
  awk 'BEGIN { for (i = 0; i < 150000; i++)
    printf "2026-10-31T12:%02d:%02d.%03dZ INFO  worker[%d] handled request id=%08x path=/api/v1/items/%d status=200 bytes=%d\n",
      (i / 60000) % 60, (i / 1000) % 60, i % 1000, i % 16, i * 2654435761 % 4294967296, i, (i * 37) % 100000 }'
}
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

# run BIN LABEL COLS ROWS LIMIT FILE -> "rss_kb history_size seconds"
run() {
  local bin=$1 label=$2 cols=$3 rows=$4 limit=$5 file=$6
  local t=(env -u TMUX -u TMUX_PANE -u EDITOR -u VISUAL SHELL=/bin/sh
    TMUX_TMPDIR="$work" "$bin" -L "$label" -f /dev/null)
  local start end pid
  start=$(now)
  "${t[@]}" new -d -s keep cat \; set -g history-limit "$limit" \; \
    new -d -s b -x "$cols" -y "$rows" "cat '$file'; echo DONE; exec cat"
  until "${t[@]}" capture-pane -p -t b | grep -q '^DONE$'; do sleep 0.05; done
  end=$(now)
  sleep "$IDLE"
  pid=$("${t[@]}" display -p -t b '#{pid}')
  echo "$(rss_kb "$pid") $("${t[@]}" display -p -t b '#{history_size}') $(echo "$end - $start" | bc)"
  "${t[@]}" kill-server
}

scenarios=("$@")
[ ${#scenarios[@]} -gt 0 ] || scenarios=(spec narrow wide)
# Runs per binary; the table shows the fastest output time (and its RSS).
RUNS="${RUNS:-1}"

printf '%-7s %-9s %6s %7s %10s %9s %8s %8s\n' scenario binary size limit rss_mb hist_size retained output_s
for s in "${scenarios[@]}"; do
  case $s in
    spec) cols=120 rows=40 limit=100000 ;;
    narrow) cols=80 rows=24 limit=10000 ;;
    wide) cols=500 rows=24 limit=10000 ;;
    flood) cols=120 rows=40 limit=2000 ;;
    flood100k) cols=120 rows=40 limit=100000 ;;
    *) echo "unknown scenario $s" >&2; exit 2 ;;
  esac
  file="$work/$s.txt"
  "gen_$s" > "$file"
  binaries=(tmux vtmux)
  [ -z "${BASE:-}" ] || binaries=(tmux base vtmux)
  for which in "${binaries[@]}"; do
    case $which in tmux) bin=$REF ;; base) bin=$BASE ;; *) bin=$VTMUX ;; esac
    secs=
    for n in $(seq "$RUNS"); do
      read -r rss1 hs1 secs1 < <(run "$bin" "bench-$which-$$-$n" $cols $rows $limit "$file")
      if [ -z "$secs" ] || [ "$(echo "$secs1 < $secs" | bc)" = 1 ]; then
        rss=$rss1 hs=$hs1 secs=$secs1
      fi
    done
    retained=-
    if [ $which = vtmux ]; then
      read -r hs2 retained < <("$RETENTION" $cols $rows $limit "$file")
      [ "$hs2" = "$hs" ] || retained="$retained(hs=$hs2)"
    fi
    printf '%-7s %-9s %6s %7s %10.1f %9s %8s %8.2f\n' "$s" $which "${cols}x$rows" $limit \
      "$(echo "$rss / 1024" | bc -l)" "$hs" "$retained" "$secs"
  done
done
