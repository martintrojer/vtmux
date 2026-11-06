#!/usr/bin/env bash
# Socket guard for scripts/check.sh.
#   socket-guard.sh record FILE   save the test-labelled entries of the real socket dirs
#   socket-guard.sh verify FILE   fail (exit 1, entries on stderr) if any were added since
#
# A server leaves its socket file behind when it exits, so a harness that lets
# one use the real socket dir ($TMUX_TMPDIR or /tmp, under tmux-<uid>/ and
# vtmux-<uid>/) strands a file per run. `cargo xtask sockets` lists the entries
# (`cargo xtask reap` removes the dead ones, so only additions count). On a
# host shared with other worktrees running older tests a listed name may be
# theirs: the label carries the creating test's pid.
set -euo pipefail
cd "$(dirname "$0")/.."
list() { ${XTASK:-cargo xtask} sockets | LC_ALL=C sort; }
case "${1:-}" in
record) list >"$2" ;;
verify)
  added=$(list | LC_ALL=C comm -13 "$2" -)
  if [ -n "$added" ]; then
    echo "check: test sockets leaked into the real socket dir (a harness lost its private TMUX_TMPDIR):" >&2
    echo "$added" >&2
    exit 1
  fi
  ;;
*) echo "usage: $0 record|verify FILE" >&2; exit 2 ;;
esac
