#!/usr/bin/env bash
# Build a release tarball of vtmux for this host: what .github/workflows/release.yml
# runs on each runner, and what to run locally to try a release before tagging.
#
#   scripts/release-tarball.sh <target-name> [tag]
#
# <target-name> is the Rust target triple that names the tarball
# (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin),
# so `mise use github:martintrojer/vtmux` finds the asset for its platform.
# With a tag (v3.8-beta1), the built binary's version must match it (D25: the tag
# is `v` + what `vtmux -V` prints). Writes target/dist/vtmux-v<version>-<target>.tar.gz
# and its .sha256, and prints the tarball's path.
set -euo pipefail
cd "$(dirname "$0")/.."

target=${1:?usage: release-tarball.sh <target-name> [tag]}
tag=${2:-}

cargo build --release --locked -p vtmux
bin=target/release/vtmux

# D25: as vtmux, `-V` prints `vtmux 3.8-beta1`; as tmux, `tmux 3.8`.
version=$("$bin" -V)
case "$version" in
"vtmux "*) version=${version#vtmux } ;;
*) echo "unexpected vtmux -V: $version" >&2 && exit 1 ;;
esac
if [ -n "$tag" ] && [ "$tag" != "v$version" ]; then
  echo "tag $tag does not match the built version $version (expected v$version)" >&2
  exit 1
fi

name=vtmux-v$version-$target
stage=target/dist/$name
rm -rf "$stage" "$stage.tar.gz" "$stage.tar.gz.sha256"
mkdir -p "$stage/licenses"
cp "$bin" "$stage/vtmux"
strip "$stage/vtmux"
# Stripping a Mach-O breaks its ad-hoc signature, and arm64 macOS kills an
# unsigned binary at exec: sign it again.
if [ "$(uname -s)" = Darwin ]; then
  codesign --force --sign - "$stage/vtmux"
fi

# The stripped binary runs, under both names.
stripped_v=$("$stage/vtmux" -V)
if [ "$stripped_v" != "vtmux $version" ]; then
  echo "stripped binary: unexpected vtmux -V: $stripped_v" >&2 && exit 1
fi
ln -s vtmux "$stage/tmux"
tmux_v=$("$stage/tmux" -V)
rm "$stage/tmux"
case "$tmux_v" in
"tmux "*) ;;
*) echo "unexpected tmux -V: $tmux_v" >&2 && exit 1 ;;
esac

cp LICENSE README.md CHANGELOG.md "$stage/"
# libghostty-vt is linked in statically: ship its licence and Ghostty's.
cp third_party/libghostty-rs/LICENSE "$stage/licenses/libghostty-rs-LICENSE"
# The newest build directory, in case an older one is left from another pin.
# shellcheck disable=SC2012
ghostty_license=$(ls -t target/release/build/libghostty-vt-sys-*/out/ghostty-src/LICENSE | head -1)
cp "$ghostty_license" "$stage/licenses/ghostty-LICENSE"

tar -C target/dist -czf "$stage.tar.gz" "$name"
if command -v sha256sum >/dev/null; then
  (cd target/dist && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
else
  (cd target/dist && shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256")
fi
echo "$stage.tar.gz"
