#!/usr/bin/env bash
# Ghostty's own Zig tests on the tree vtmux builds: the libghostty-vt source
# the sys crate's build script unpacked and patched (OUT_DIR/ghostty-src,
# libghostty-vt-sys/patches), or the GHOSTTY_SOURCE_DIR tree. The filters are
# the modules the patches change (the terminal core, its C API, kitty
# graphics, Unicode) and every test a patch adds ("vtmux" in its name): an
# upstream test a patch breaks fails here, not only once someone runs the
# suite by hand. Debug, as upstream runs it (some tests need its safety
# checks). check.sh's `ghostty-zig` stage.
#
# Upstream's "temporary file outside directory prefix is rejected" (kitty)
# fails when the tree is under /tmp or /dev/shm, which Ghostty trusts as
# temporary directories: keep CARGO_TARGET_DIR elsewhere.
set -euo pipefail
cd "$(dirname "$0")/.."
target="${CARGO_TARGET_DIR:-$PWD/target}"
# The build script's OUT_DIR, from cargo's report of running it.
out_dir=$(cargo build -p vtmux-ghostty-vt-sys --message-format=json 2>/dev/null |
  grep '"reason":"build-script-executed"' | grep 'vtmux-ghostty-vt-sys' |
  sed -n 's/.*"out_dir":"\([^"]*\)".*/\1/p' | tail -n 1)
if [ -n "${GHOSTTY_SOURCE_DIR:-}" ]; then
  src=$GHOSTTY_SOURCE_DIR
else
  [ -n "$out_dir" ] || {
    echo "ghostty-zig-tests: no OUT_DIR for vtmux-ghostty-vt-sys" >&2
    exit 1
  }
  src=$out_dir/ghostty-src
fi
deps=${GHOSTTY_ZIG_SYSTEM_DIR:-${out_dir:+$out_dir/ghostty-deps}}
[ -f "$src/build.zig" ] || {
  echo "ghostty-zig-tests: no Ghostty source at $src" >&2
  exit 1
}
system=()
if [ -n "$deps" ] && [ -d "$deps" ]; then system=(--system "$deps"); fi
filters=()
for f in terminal.Terminal. terminal.PageList. terminal.Screen. terminal.page. \
  terminal.stream terminal.formatter. terminal.c. terminal.kitty. unicode. vtmux; do
  filters+=("-Dtest-filter=$f")
done
cache="$target/ghostty-zig-tests"
mkdir -p "$cache"
echo "ghostty-zig-tests: $src"
cd "$src"
ZIG_GLOBAL_CACHE_DIR="$cache/global" zig build test-lib-vt \
  --cache-dir "$cache/local" -Demit-lib-vt=true -Demit-xcframework=false \
  -Dapp-runtime=none ${system[@]+"${system[@]}"} "${filters[@]}" --summary line
