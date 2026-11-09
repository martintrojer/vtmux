#!/usr/bin/env bash
# Per-test wall time of one test binary: every test run on its own
# (`--exact`, one thread), P at a time, slowest first. Stable cargo test has
# no per-test times (`--report-time` is nightly) and nextest is not a
# dependency. For test upkeep when the slow gate runs over its budget
# (docs/test-health/2026-11-08-test-suite-speed.md).
#
#   scripts/test-times.sh <test-binary> [P] [filter]
#   scripts/test-times.sh "$(ls -t target/debug/deps/attach-* | grep -v '\.d$' | head -1)" 8
#
# Prints `seconds<TAB>exit<TAB>name` per test and a total; a test's output
# goes to $TMPDIR/test-times/<name>.log. Run from the test's package dir
# (cargo runs tests there) when its tests read files by relative path.
# shellcheck disable=SC2016 # the xargs script expands in its own bash
set -euo pipefail
bin=$1 P=${2:-8} filter=${3:-}
logs="${TMPDIR:-/tmp}/test-times"
mkdir -p "$logs"
out=$(mktemp)
trap 'rm -f "$out"' EXIT
"$bin" --list --format terse | sed -n 's/: test$//p' | grep -e "$filter" |
  xargs -P "$P" -I{} bash -c '
    start=$(date +%s.%N)
    status=0
    timeout -k 5 600 "$0" --exact "$1" --test-threads 1 -q >"$2/$1.log" 2>&1 || status=$?
    printf "%.2f\t%s\t%s\n" "$(echo "$(date +%s.%N) - $start" | bc)" "$status" "$1" >>"$3"
  ' "$bin" {} "$logs" "$out"
sort -rn "$out"
awk -F'\t' '{s += $1; if ($2 != 0) f++} END {printf "tests %d, sum %.1f s, failed %d\n", NR, s, f}' "$out"
