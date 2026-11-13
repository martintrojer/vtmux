#!/usr/bin/env bash
# Download the vtmux 3.8-beta3 release binary for this host, the
# `VTMUX_BETA3_BINARY` of the live-upgrade tests that upgrade from and back
# to it (crates/vtmux/tests/upgrade.rs, `beta3`), check it against the
# release's SHA256SUMS, and print its path. CI's test jobs run it; locally:
#
#   VTMUX_BETA3_BINARY=$(bash scripts/fetch-beta3.sh) cargo test -p vtmux --test upgrade beta3
#
# Unpacks into target/beta3 (kept: a second run downloads nothing).
# VTMUX_BETA3_URL replaces the release's download URL (a mirror).
set -euo pipefail
cd "$(dirname "$0")/.."

tag=v3.8-beta3
url=${VTMUX_BETA3_URL:-https://github.com/martintrojer/vtmux/releases/download/$tag}
case "$(uname -s)-$(uname -m)" in
Linux-x86_64) triple=x86_64-unknown-linux-gnu ;;
Linux-aarch64) triple=aarch64-unknown-linux-gnu ;;
Darwin-arm64) triple=aarch64-apple-darwin ;;
*)
  echo "fetch-beta3: $tag has no build for $(uname -sm)" >&2
  exit 1
  ;;
esac
name=vtmux-$tag-$triple
dir=target/beta3
bin=$dir/$name/vtmux
if [ ! -x "$bin" ]; then
  mkdir -p "$dir"
  curl -fsSL --retry 3 -o "$dir/SHA256SUMS" "$url/SHA256SUMS"
  curl -fsSL --retry 3 -o "$dir/$name.tar.gz" "$url/$name.tar.gz"
  sums=$(grep " $name.tar.gz\$" "$dir/SHA256SUMS")
  if command -v sha256sum >/dev/null; then
    (cd "$dir" && echo "$sums" | sha256sum -c - >&2)
  else
    (cd "$dir" && echo "$sums" | shasum -a 256 -c - >&2)
  fi
  tar -C "$dir" -xzf "$dir/$name.tar.gz"
fi
version=$("$bin" -V)
if [ "$version" != "vtmux 3.8-beta3" ]; then
  echo "fetch-beta3: $bin -V says $version" >&2
  exit 1
fi
echo "$PWD/$bin"
