#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
VTMUX_CHECK_HARNESS="${VTMUX_CHECK_HARNESS:-0}"
if [ "$VTMUX_CHECK_HARNESS" = 1 ]; then cargo xtask regress && cargo xtask oracle replay; fi
