#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
# Kill test servers (and their pane programs) that finished probe scripts left
# behind: test-labelled, parented to init, over an hour old; also dead test
# socket files in the real socket dirs. Never fails the gate.
cargo xtask reap || true
# Socket guard (scripts/socket-guard.sh): record the test-labelled entries of
# the real socket dirs now; the end of the script fails on any added.
sockets_before=$(mktemp)
trap 'rm -f "$sockets_before"' EXIT
bash scripts/socket-guard.sh record "$sockets_before"
cargo fmt --all --check
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
fi
cargo test --workspace
VTMUX_CHECK_HARNESS="${VTMUX_CHECK_HARNESS:-1}"
if [ "$VTMUX_CHECK_HARNESS" = 1 ]; then cargo xtask regress && cargo xtask oracle replay; fi
bash scripts/socket-guard.sh verify "$sockets_before"
