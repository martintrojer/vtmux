#!/usr/bin/env bash
# Build a release tarball of vtmux for this host: what .github/workflows/release.yml
# runs on each runner, and what to run locally to try a release before tagging.
#
#   scripts/release-tarball.sh <target-name> [tag]
#
# <target-name> is the Rust target triple that names the tarball
# (x86_64-unknown-linux-gnu, aarch64-unknown-linux-gnu, aarch64-apple-darwin,
# x86_64-unknown-linux-musl, aarch64-unknown-linux-musl), so `mise use
# github:martintrojer/vtmux` finds the asset for its platform. A gnu or
# darwin target is this host's own build; a musl one is a static binary of
# the same CPU (`rustup target add <target>` first), linked by `zig cc`
# against Zig's own musl.
# With a tag (v3.8-beta1), the built binary's version must match it (D25: the tag
# is `v` + what `vtmux -V` prints). Writes target/dist/vtmux-v<version>-<target>.tar.gz
# and its .sha256, and prints the tarball's path.
set -euo pipefail
cd "$(dirname "$0")/.."

target=${1:?usage: release-tarball.sh <target-name> [tag]}
tag=${2:-}

case "$target" in
*-linux-musl)
  # Rust's musl target links its own crt objects and libc.a by default;
  # Zig's musl (the one libghostty-vt is built against) instead, through
  # `zig cc`, and no self-contained objects to clash with it. `zig cc`
  # refuses the Cortex-A53 erratum flag rustc gives aarch64 (as
  # cargo-zigbuild, it is dropped).
  linker=$PWD/target/zig-cc-$target
  mkdir -p target
  cat >"$linker" <<EOF
#!/bin/sh
for a; do
  shift
  case \$a in -Wl,--fix-cortex-a53-843419) ;; *) set -- "\$@" "\$a" ;; esac
done
exec zig cc -target ${target%%-*}-linux-musl "\$@"
EOF
  chmod +x "$linker"
  env_target=$(echo "$target" | tr a-z- A-Z_)
  export "CARGO_TARGET_${env_target}_LINKER=$linker"
  export "CARGO_TARGET_${env_target}_RUSTFLAGS=-C link-self-contained=no"
  cargo build --release --locked -p vtmux --target "$target"
  bin=target/$target/release/vtmux
  # Static: no interpreter, no shared libraries.
  case "$(readelf -l -d "$bin")" in
  *INTERP* | *NEEDED*) echo "$bin is not static" >&2 && exit 1 ;;
  esac
  ;;
*)
  cargo build --release --locked -p vtmux
  bin=target/release/vtmux
  ;;
esac

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
# The release profile strips already (Cargo.toml); this keeps the tarball
# stripped under a CARGO_PROFILE_RELEASE_STRIP override.
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
# libghostty-vt is linked in statically: ship its licence and Ghostty's (the
# pinned commit's, kept beside the patches).
cp third_party/libghostty-rs/LICENSE "$stage/licenses/libghostty-rs-LICENSE"
cp third_party/libghostty-rs/libghostty-vt-sys/LICENSE-ghostty "$stage/licenses/ghostty-LICENSE"

tar -C target/dist -czf "$stage.tar.gz" "$name"
if command -v sha256sum >/dev/null; then
  (cd target/dist && sha256sum "$name.tar.gz" >"$name.tar.gz.sha256")
else
  (cd target/dist && shasum -a 256 "$name.tar.gz" >"$name.tar.gz.sha256")
fi
echo "$stage.tar.gz"
