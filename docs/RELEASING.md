# Releasing

A release is a version, a GitHub release with prebuilt tarballs, and the crates on crates.io. Publishing is the owner's call; everything before it can be tried any time.

## Version

vtmux's version is the tmux release it is tested against (D25): `version` in `[workspace.package]` of `Cargo.toml` is `3.8.0-beta.3`, which `vtmux -V` prints as `3.8-beta3`. A bump changes it in every place a crate names it, which `cargo test -p xtask publish` checks:

- `[workspace.package] version` and the `=` requirements in `[workspace.dependencies]` (`Cargo.toml`);
- `version` of the two forks and the `=` requirements on them (`third_party/libghostty-rs/libghostty-vt-sys/Cargo.toml`, `libghostty-vt/Cargo.toml`, `crates/vtmux-vt/Cargo.toml`);
- `Cargo.lock` (any cargo build rewrites it).

Add the version's section to `CHANGELOG.md`, and the release page's text to `docs/release-notes/<version>.md` (e.g. `3.8-beta2.md`): short, about 1,500 characters, what is new and how to install. The release uses it as its notes; without it, the CHANGELOG section.

## GitHub release

Run `scripts/release-tarball.sh x86_64-unknown-linux-gnu v3.8-beta1` locally, then push the tag `v3.8-beta1`: `.github/workflows/release.yml` builds the tarballs for Linux (x86_64, aarch64; glibc and static musl) and macOS (Apple silicon) and publishes the release with `docs/release-notes/<version>.md` (else the tag's CHANGELOG section) as its notes. `mise use -g github:martintrojer/vtmux` installs from it (the gnu tarball on a glibc host, the musl one on a musl host or with `[matching=musl]`). A musl tarball builds the same way (`rustup target add x86_64-unknown-linux-musl`; `zig cc` links it).

## crates.io

Eight crates, published in this order, each after the crates it depends on:

| Crate | What it is |
|---|---|
| `vtmux-ghostty-vt-sys` | Fork of libghostty-rs's `libghostty-vt-sys`: the FFI bindings, and the build script that downloads the pinned Ghostty, applies `patches/` and builds it with Zig (`third_party/libghostty-rs/VENDOR.md`). MIT |
| `vtmux-ghostty-vt` | Fork of libghostty-rs's `libghostty-vt`: the safe API. MIT |
| `vtmux-vt`, `vtmux-core`, `vtmux-snapshot`, `vtmux-client`, `vtmux-server` | vtmux's own libraries (docs/ARCHITECTURE.md). ISC |
| `vtmux` | The binary: what `cargo install vtmux` builds. ISC |

The libraries promise no API: each depends on the exact version of the others (`=3.8.0-beta.1`), and their descriptions send people to `vtmux`. The upstream names (`libghostty-vt`, `libghostty-vt-sys`) are upstream's. Each crate ships its sources only (`include` in its manifest: no tests, fixtures, examples or oracle traces) and its licence; the largest, `vtmux-server`, is about 800 KiB compressed, against crates.io's 10 MB limit.

### Dry run

```sh
mise exec -- bash scripts/publish.sh            # --dry-run is the default
```

It runs `cargo publish --dry-run` for `vtmux-ghostty-vt-sys` and `cargo package` for the rest, each verified by building it from its `.crate` file. The others can't take `cargo publish --dry-run` until their dependencies are on crates.io (cargo resolves them there), so the script resolves them to the `.crate` files just made: cargo's multi-package packaging for the workspace's members, `[patch.crates-io]` for the two forks, which sit outside the workspace. Then it unpacks every `.crate` into a temporary directory, runs `cargo install --offline` on the unpacked `vtmux` with the others patched in, and runs the binary: the build `cargo install vtmux` makes, from the packaged sources alone. 3 minutes on a 96-core host (about 1 of them each for the two Ghostty builds from unpacked sources; the sys crate's own verify build was cached from an earlier run, a cold one adds another). It needs Zig 0.17, and the Ghostty archive from GitHub or `GHOSTTY_SOURCE_TARBALL` (`git archive --format=tar.gz <commit>` of a ghostty clone gives the same files). Pass `--allow-dirty` to try it on uncommitted work.

### Publish

On the tagged commit, clean, after a dry run of it passed:

```sh
cargo login                                     # once, with a crates.io token
mise exec -- bash scripts/publish.sh --publish
```

`cargo publish` runs for each crate in order and waits until crates.io serves it before the next one. If one fails part way (crates.io limits how many new crates an account publishes in a short time: a 429 says when to retry), run it again: it skips the crates already on crates.io at this version. A version once published can't be replaced, only yanked. A pre-release (`3.8.0-beta.1`) is installed only when named: `cargo install vtmux@3.8.0-beta.1`; bump the README's install line with each beta.

### What `cargo install vtmux` gets

The binary as built from the crates, with cargo's default release profile: the workspace's `[profile.release]` (fat LTO, one codegen unit, stripped) is not part of a published crate, so it is not used. `CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_STRIP=true cargo install --locked vtmux@3.8.0-beta.1` (the current version: a pre-release needs it named) builds what the tarballs ship. The build downloads Ghostty's source (about 40 MB) and Zig's packages for it.
