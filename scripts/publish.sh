#!/usr/bin/env bash
# Package vtmux's crates for crates.io, in the order they publish
# (docs/RELEASING.md).
#
#   scripts/publish.sh [--dry-run] [cargo args]   the default: package and
#       verify every crate, then install vtmux from the .crate files alone,
#       offline, and run it. Publishes nothing.
#   scripts/publish.sh --publish [cargo args]     `cargo publish` each crate in
#       order. The owner's call, after a dry run of the same commit.
#
# Cargo args (`--allow-dirty` while trying it out) go to every `cargo package`
# or `cargo publish`.
#
# Until a crate is on crates.io, nothing that depends on it can resolve it
# there, so `cargo publish --dry-run` works only for vtmux-ghostty-vt-sys
# (no dependency of ours), and the dry run runs it for that one. Every other
# crate goes through `cargo package` (not `--no-verify`: it verifies each by
# building it from its .crate's sources), with the crates before it resolved
# to theirs: the workspace's members
# by cargo's own multi-package packaging, the two crates outside the workspace
# (third_party/libghostty-rs, VENDOR.md) by `[patch.crates-io]` to the
# unpacked .crate. Cargo warns that those patches are unused by the
# workspace's own graph (it uses the paths): expected. Then `cargo install
# --offline` builds vtmux from the unpacked .crate files alone, all of them
# patched in, which is what `cargo install vtmux` builds once they are
# published.
#
# The Ghostty source download (`vtmux-ghostty-vt-sys`'s build script) honours
# GHOSTTY_SOURCE_TARBALL, so a dry run works offline given the tarball.
set -euo pipefail
cd "$(dirname "$0")/.."

mode=dry-run
case "${1:-}" in
--dry-run) shift ;;
--publish)
  mode=publish
  shift
  ;;
esac

# Dependency order: each crate after every crate it depends on. xtask's
# `publish_manifests` test checks this list against the manifests.
crates=(vtmux-ghostty-vt-sys vtmux-ghostty-vt vtmux-vt vtmux-core vtmux-snapshot vtmux-client vtmux-server vtmux)

manifest() {
  case "$1" in
  vtmux-ghostty-vt-sys) echo third_party/libghostty-rs/libghostty-vt-sys/Cargo.toml ;;
  vtmux-ghostty-vt) echo third_party/libghostty-rs/libghostty-vt/Cargo.toml ;;
  *) echo "crates/$1/Cargo.toml" ;;
  esac
}

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"$/\1/p' Cargo.toml)
[ -n "$version" ] || {
  echo "publish: no [workspace.package] version in Cargo.toml" >&2
  exit 1
}

if [ "$mode" = publish ]; then
  for c in "${crates[@]}"; do
    echo "publish: $c $version"
    # Each waits until crates.io serves it, so the next one resolves it.
    cargo publish --manifest-path "$(manifest "$c")" --target-dir target "$@"
  done
  exit 0
fi

# Outside the checkout: inside it, cargo takes the unpacked vtmux for a
# member of this workspace.
check=$(mktemp -d "${TMPDIR:-/tmp}/vtmux-publish.XXXXXX")
trap 'rm -rf "$check"' EXIT
mkdir -p "$check/src"
patches=()
unpack() {
  local crate=target/package/$1-$version.crate
  [ -f "$crate" ] || {
    echo "publish: $crate was not made" >&2
    exit 1
  }
  tar -xzf "$crate" -C "$check/src"
  patches+=(--config "patch.crates-io.$1.path=\"$check/src/$1-$version\"")
}

# The two outside the workspace, one at a time (vtmux-ghostty-vt resolves
# vtmux-ghostty-vt-sys to the one just packaged).
echo "publish: publish --dry-run vtmux-ghostty-vt-sys $version"
cargo publish --dry-run --manifest-path "$(manifest vtmux-ghostty-vt-sys)" --target-dir target "$@"
unpack vtmux-ghostty-vt-sys
echo "publish: package vtmux-ghostty-vt $version"
cargo package --manifest-path "$(manifest vtmux-ghostty-vt)" --target-dir target "${patches[@]}" "$@"
unpack vtmux-ghostty-vt
# The workspace's members, together.
echo "publish: package the workspace's crates $version"
cargo package --workspace --exclude xtask ${patches[@]+"${patches[@]}"} "$@"
for c in "${crates[@]:2}"; do unpack "$c"; done

echo "publish: install vtmux from the .crate files, offline"
cargo install --offline --path "$check/src/vtmux-$version" --root "$check/root" \
  --target-dir "$check/target" "${patches[@]}"
"$check/root/bin/vtmux" -V
for c in "${crates[@]}"; do
  printf 'publish: %-22s %s\n' "$c" "$(du -k "target/package/$c-$version.crate" | cut -f1) KiB"
done
echo "publish: dry run passed; nothing was published"
