#!/usr/bin/env bash
# The slow gate (AGENTS.md, *Gates*), in stages:
#   lint              cargo fmt --check (the Ghostty wrapper too), clippy, the other OS's type-check
#   test              cargo test --workspace, the Ghostty wrapper's tests (allocator_api)
#   regress           cargo xtask regress
#   replay            cargo xtask oracle replay
#   replay-upgrade-3  the replay with a live upgrade after every third step
#   replay-upgrade-1  the same after every step, for the scenarios that once failed only so
# `check.sh` runs them all; `check.sh --only test,regress` the ones named
# (CI runs each stage as a job of its own, .github/workflows/ci.yml, so CI
# and the local gate run the same commands); `check.sh --list` prints them.
# VTMUX_CHECK_HARNESS=0 without `--only` runs lint and test only. Whatever
# the stages, a run reaps leftovers first and ends with the socket guard.
set -euo pipefail
cd "$(dirname "$0")/.."
all_stages="lint test regress replay replay-upgrade-3 replay-upgrade-1"
usage() {
  echo "usage: $0 [--only STAGE[,STAGE...]] [--list]; stages: $all_stages" >&2
  exit 2
}
only=""
while [ $# -gt 0 ]; do
  case "$1" in
  --list)
    # shellcheck disable=SC2086 # one stage a word
    printf '%s\n' $all_stages
    exit 0
    ;;
  --only)
    [ $# -ge 2 ] || usage
    only=$2
    shift 2
    ;;
  --only=*)
    only=${1#--only=}
    shift
    ;;
  *) usage ;;
  esac
done
if [ -n "$only" ]; then
  stages=""
  for s in ${only//,/ }; do
    case " $all_stages " in
    *" $s "*) stages="$stages $s" ;;
    *)
      echo "check: no stage \`$s\`" >&2
      usage
      ;;
    esac
  done
  [ -n "$stages" ] || usage
elif [ "${VTMUX_CHECK_HARNESS:-1}" = 1 ]; then
  stages=" $all_stages"
else
  stages=" lint test"
fi
# want STAGE...: is one of them selected?
want() {
  local s
  for s in "$@"; do
    case "$stages " in *" $s "*) return 0 ;; esac
  done
  return 1
}
echo "check: stages${stages}"
# Kill test servers (and their pane programs) that finished probe scripts left
# behind: test-labelled, parented to init, over an hour old; also dead test
# socket files in the real socket dirs. Never fails the gate.
cargo xtask reap || true
# Socket guard (scripts/socket-guard.sh): record the test-labelled entries of
# the real socket dirs now; the end of the script fails on any added.
sockets_before=$(mktemp)
logs=$(mktemp -d)
pids=()
# A suite still running in the background when the script stops (a failure,
# a timeout, ^C) goes with it: each runs in a process group of its own.
cleanup() {
  local pid
  for pid in ${pids[@]+"${pids[@]}"}; do
    kill -TERM -- "-$pid" 2>/dev/null || true
  done
  rm -rf "$sockets_before" "$logs"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
bash scripts/socket-guard.sh record "$sockets_before"
# The published Ghostty fork is outside the workspace, including its tests.
ghostty_manifest=third_party/libghostty-rs/libghostty-vt/Cargo.toml
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$PWD/target}"
if want lint; then
  echo "check: lint"
  start=$(date +%s)
  cargo fmt --all --check
  cargo fmt --manifest-path "$ghostty_manifest" --check
  cargo clippy --workspace --all-targets -- -D warnings
  # Type-check the other platform too, so a Linux-only API (SOCK_CLOEXEC,
  # O_CLOEXEC on openpt) is caught before CI. Skipped where the target isn't
  # installed (`rustup target add aarch64-apple-darwin` / x86_64-unknown-linux-gnu).
  case "$(uname -s)" in
  Linux) other=aarch64-apple-darwin ;;
  *) other=x86_64-unknown-linux-gnu ;;
  esac
  if rustup target list --installed 2>/dev/null | grep -qx "$other"; then
    cargo check --workspace --all-targets --exclude xtask --target "$other"
  else
    echo "check: no $other target installed: the other OS's type-check skipped"
  fi
  echo "check: lint passed in $(($(date +%s) - start)) s"
fi
# The suites below build nothing once these are built, so they can run at
# once: on a big host (`nproc` 32 or more: it counts the CPUs this process may
# use, so a pinned gate or a cpuset container is its size, where
# `getconf _NPROCESSORS_ONLN` counts the host's) they do, each into its own
# log, printed whole as it ends; on a smaller one (a 16-core laptop, CI's
# runners) one after another, as they always ran: together they pegged a
# 16-core machine (peak 14.5 of 16 CPUs, `cargo test` alone peaks at 6.4).
# They wait on terminals, timers and each
# other's processes far more than they compute, so together they take about
# as long as the longest. VTMUX_CHECK_PARALLEL=1 forces it on, =0 off.
# The builds the selected suites make (`cargo test`; `cargo xtask` builds
# xtask, and regress and replay `cargo build -p vtmux`), each made now and
# then checked fresh: one that is not (a build script that reruns every
# time, two of them that build target/debug/vtmux differently) would rewrite
# the binary under the suites running meanwhile.
builds=()
if want test; then
  builds+=("test --workspace --no-run" "test --manifest-path $ghostty_manifest --features allocator_api --no-run")
fi
if want regress replay replay-upgrade-3 replay-upgrade-1; then
  builds+=("build -p xtask" "build -p vtmux")
fi
for b in ${builds[@]+"${builds[@]}"}; do
  # shellcheck disable=SC2086 # the words of one cargo command
  cargo $b -q
done
for b in ${builds[@]+"${builds[@]}"}; do
  # shellcheck disable=SC2086
  if cargo $b -v 2>&1 | grep -E '^ *(Dirty|Compiling) '; then
    echo "check: \`cargo $b\` builds again right after a build (above)" >&2
    exit 1
  fi
done
parallel="${VTMUX_CHECK_PARALLEL:-auto}"
if [ "$parallel" = auto ]; then
  # macOS has no nproc (and no CPU pinning): its own count.
  cpus=$(nproc 2>/dev/null || sysctl -n hw.logicalcpu 2>/dev/null || getconf _NPROCESSORS_ONLN)
  if [ "$cpus" -ge 32 ]; then parallel=1; else parallel=0; fi
fi
if [ ${#builds[@]} -gt 0 ]; then
  echo "check: suites run $([ "$parallel" = 1 ] && echo 'at once' || echo 'one after another') (VTMUX_CHECK_PARALLEL=${VTMUX_CHECK_PARALLEL:-auto}, ${cpus:-unset} CPUs)"
fi
names=()
starts=()
# Job control: a background job gets a process group of its own (and no
# terminal to read: stdin is /dev/null).
if [ "$parallel" = 1 ]; then set -m; fi
# suite NAME CMD...: run CMD now (serial) or in the background (parallel).
# Bash 3.2 (macOS) has no negative array index, and set -u fails on an
# empty array's "${a[@]}".
suite() {
  local name=$1 start
  shift
  start=$(date +%s)
  if [ "$parallel" = 1 ]; then
    local log="$logs/${#names[@]}"
    {
      status=0
      "$@" </dev/null >"$log" 2>&1 || status=$?
      date +%s >"$log.end"
      exit "$status"
    } &
    pids+=($!)
  else
    echo "check: $name"
    "$@"
    echo "check: $name passed in $(($(date +%s) - start)) s"
  fi
  names+=("$name")
  starts+=("$start")
}
if want test; then
  suite "cargo test" cargo test --workspace
  suite "Ghostty wrapper (allocator_api)" cargo test --manifest-path "$ghostty_manifest" --features allocator_api
fi
if want regress; then suite "regress" cargo xtask regress; fi
if want replay; then suite "oracle replay" cargo xtask oracle replay; fi
# The replay again with the server upgraded onto itself after every third
# step (live-upgrade spec, *Tests*): every scenario must still match. Then
# after every step, for the scenarios that once failed only so.
if want replay-upgrade-3; then
  suite "oracle replay --upgrade-every 3" cargo xtask oracle replay --upgrade-every 3
fi
upgrade_every_1='reply_client_colours,scrollbars_*,tree_preview_start_timer,prompt_flags,passthrough_*,set_titles'
if want replay-upgrade-1; then
  suite "oracle replay --upgrade-every 1" cargo xtask oracle replay --upgrade-every 1 --only "$upgrade_every_1"
fi
if [ "$parallel" = 1 ]; then
  failed=""
  i=0
  while [ "$i" -lt "${#pids[@]}" ]; do
    status=0
    wait "${pids[$i]}" || status=$?
    echo "check: ==== ${names[$i]}: exit $status in $(($(cat "$logs/$i.end") - ${starts[$i]})) s"
    cat "$logs/$i"
    if [ "$status" != 0 ]; then failed="$failed; ${names[$i]}"; fi
    i=$((i + 1))
  done
  if [ -n "$failed" ]; then
    echo "check: failed: ${failed#; }" >&2
    exit 1
  fi
fi
bash scripts/socket-guard.sh verify "$sockets_before"
echo "check: passed:${stages}"
