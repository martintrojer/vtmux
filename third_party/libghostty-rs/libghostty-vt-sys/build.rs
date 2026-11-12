use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// vtmux: the libghostty-vt release this crate builds (a `vt-<version>`
/// tag of Ghostty; VENDOR.md says how to move to the next one).
const GHOSTTY_VERSION: &str = "1.0.1";

/// vtmux: where the release files are downloaded from, unless
/// `GHOSTTY_RELEASE_URL` names a mirror of the directory, or
/// `GHOSTTY_SOURCE_TARBALL` and `GHOSTTY_DEPS_TARBALL` copies already
/// downloaded. Each release is `<url>/<version>/`.
const GHOSTTY_RELEASES: &str = "https://release.files.ghostty.org/libghostty-vt";

/// vtmux: SHA-256 of `libghostty-vt-<version>.tar.gz` (the release's
/// `SHA256SUMS`, which its minisign key signs), checked before anything is
/// extracted. Bump with `GHOSTTY_VERSION`.
const GHOSTTY_SOURCE_SHA256: &str =
    "333dfce97641ef9b536bb9e08907ad86abaaada50e8f7417b70d9284adef37aa";

/// vtmux: SHA-256 of `libghostty-vt-<version>-deps.tar.gz`, every Zig
/// package the library needs, unpacked by package hash: `zig build
/// --system` reads it, so Zig fetches nothing.
const GHOSTTY_DEPS_SHA256: &str =
    "91a4e10960a52bf5bf4b15aa2a2cddaff8a89d6cd903d3055bda4fd17de3356d";

/// vtmux: the Zig the release's `build.zig.zon` asks for
/// (`minimum_zig_version`; Zig breaks its build API between minor versions,
/// so only this major.minor builds it).
const ZIG_VERSION: &str = "0.17";

/// File name of the static archive on Windows. Ghostty installs it under this
/// name for every Windows ABI so it does not collide with `ghostty-vt.lib`,
/// the import library for `ghostty-vt.dll`. Validation and link emission must
/// agree on it, or the build silently links the import library instead.
const WINDOWS_STATIC_LIB_FILE: &str = "ghostty-vt-static.lib";

#[derive(Clone, Copy)]
enum LinkMode {
    Dynamic,
    Static,
}

impl LinkMode {
    fn current() -> Self {
        if cfg!(feature = "link-dynamic") {
            Self::Dynamic
        } else {
            Self::Static
        }
    }

    fn artifact_kind(self) -> &'static str {
        match self {
            Self::Dynamic => "shared library",
            Self::Static => "static library",
        }
    }

    fn matches_library(self, target: &str, file_name: &str) -> bool {
        match self {
            Self::Dynamic => {
                if target.contains("darwin") {
                    file_name.starts_with("libghostty-vt") && file_name.ends_with(".dylib")
                } else if target.contains("windows") {
                    file_name == "ghostty-vt.lib"
                        || file_name == "ghostty-vt.dll"
                        || file_name == "libghostty-vt.dll.lib"
                        || file_name == "libghostty-vt.dll.a"
                } else {
                    file_name == "libghostty-vt.so" || file_name.starts_with("libghostty-vt.so.")
                }
            }
            Self::Static => {
                if target.contains("windows") {
                    file_name == WINDOWS_STATIC_LIB_FILE
                } else {
                    file_name == "libghostty-vt.a"
                }
            }
        }
    }

    #[cfg(feature = "pkg-config")]
    fn pkg_config_name(self) -> &'static str {
        match self {
            Self::Dynamic => "libghostty-vt",
            Self::Static => "libghostty-vt-static",
        }
    }
}

fn main() {
    // docs.rs has no Zig toolchain. The checked-in bindings in src/bindings.rs
    // are enough for generating documentation, so skip the entire native
    // build when running under docs.rs.
    if env::var("DOCS_RS").is_ok() {
        return;
    }

    emit_patch_stack();

    let link_mode = LinkMode::current();
    // Cargo always sets TARGET for build scripts, so read it once here and
    // hand it to whichever path runs rather than re-reading it per call site.
    let target = env::var("TARGET").expect("TARGET must be set");

    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_CPU");
    println!("cargo:rerun-if-env-changed=LIBGHOSTTY_VT_SYS_OPTIMIZE");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_DIR");
    println!("cargo:rerun-if-env-changed=GHOSTTY_SOURCE_TARBALL");
    println!("cargo:rerun-if-env-changed=GHOSTTY_DEPS_TARBALL");
    println!("cargo:rerun-if-env-changed=GHOSTTY_RELEASE_URL");
    println!("cargo:rerun-if-env-changed=GHOSTTY_ZIG_SYSTEM_DIR");
    println!("cargo:rerun-if-env-changed=TARGET");
    println!("cargo:rerun-if-env-changed=HOST");
    println!("cargo:rerun-if-env-changed=DEBUG");
    println!("cargo:rerun-if-env-changed=OPT_LEVEL");
    // vtmux: relative to this package. Upstream's `crates/libghostty-vt-sys/`
    // prefix (its repo layout) named a file that never exists here, so
    // cargo reran the script and rebuilt everything above it every time.
    println!("cargo:rerun-if-changed=build.rs");
    emit_rerun_inputs();

    // An explicit source override should stay authoritative even when the
    // pkg-config feature is enabled, so local Ghostty checkouts remain easy to
    // test against.
    if env::var_os("GHOSTTY_SOURCE_DIR").is_some() {
        build_vendored(link_mode, &target);
        return;
    }

    // When the pkg-config feature is enabled, prefer an installed library over
    // fetching Ghostty. libghostty is pre-1.0, so this crate intentionally does
    // not promise compatibility with every installed C API revision.
    #[cfg(feature = "pkg-config")]
    if try_pkg_config(link_mode, &target) {
        return;
    }

    build_vendored(link_mode, &target);
}

/// vtmux: what else the library depends on, beyond `build.rs` and `patches/`
/// (`local_patches`): the tree `GHOSTTY_SOURCE_DIR` names (an edit there is
/// the local-ghostty workflow), and the Zig compiler (the repo's `mise.toml`
/// pin, and the `zig` executable on PATH: mise puts each version in a
/// directory of its own, so a bump changes that path). A path that does not
/// exist counts as changed to cargo, so each is named only if it exists.
fn emit_rerun_inputs() {
    let watch = |p: &Path| println!("cargo:rerun-if-changed={}", p.display());
    if let Some(dir) = env::var_os("GHOSTTY_SOURCE_DIR") {
        // Entry by entry: a directory is scanned whole, and `.git` and the
        // Zig caches change without the sources doing so.
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.filter_map(Result::ok) {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if !matches!(&*name, ".git" | ".zig-cache" | "zig-out") {
                    watch(&e.path());
                }
            }
        }
    }
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    if let Some(mise) = manifest
        .ancestors()
        .map(|d| d.join("mise.toml"))
        .find(|p| p.exists())
    {
        watch(&mise);
    }
    if let Some(path) = env::var_os("PATH") {
        if let Some(zig) = env::split_paths(&path)
            .map(|d| d.join("zig"))
            .find(|p| p.is_file())
        {
            watch(&zig);
        }
    }
}

/// vtmux: a hash of what decides Ghostty's native page layout in this
/// build: the pinned release, every local patch, the Zig compiler's
/// version (`Page` is not an `extern` struct, so a compiler may order its
/// fields differently at the same size), the Zig optimize mode and the
/// target (`LIBGHOSTTY_VT_PATCH_STACK`, read by `ffi::PATCH_STACK`).
/// Compressed-page blobs (patch 0017) are only read back by a binary with
/// the same value. FNV-1a, so it does not depend on the Rust version.
fn emit_patch_stack() {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &b in bytes {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x0100_0000_01b3);
    };
    eat(GHOSTTY_VERSION.as_bytes());
    eat(GHOSTTY_SOURCE_SHA256.as_bytes());
    if let Ok(dir) = env::var("GHOSTTY_SOURCE_DIR") {
        eat(b"source-dir");
        eat(dir.as_bytes());
    }
    for (_, text) in local_patches() {
        eat(text.as_bytes());
    }
    // The `zig` this script builds with (`build_vendored`). No zig: no
    // build either, so the empty version never names a library.
    let zig = Command::new("zig")
        .arg("version")
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| out.stdout)
        .unwrap_or_default();
    eat(b"zig");
    eat(zig.trim_ascii());
    eat(zig_optimize_mode().as_bytes());
    eat(env::var("TARGET").unwrap_or_default().as_bytes());
    println!("cargo:rustc-env=LIBGHOSTTY_VT_PATCH_STACK={h:016x}");
}

/// Build libghostty-vt from source via zig. The zig build itself generates
/// shared and static artifacts plus pkg-config files in `share/pkgconfig/`.
fn build_vendored(link_mode: LinkMode, target: &str) {
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR must be set"));
    let host = env::var("HOST").expect("HOST must be set");
    check_zig();

    // Locate ghostty source: env override > fetch into OUT_DIR. A fetched
    // release brings its Zig packages (`deps`); a local checkout fetches
    // its own unless GHOSTTY_ZIG_SYSTEM_DIR names them.
    let (ghostty_dir, deps) = match env::var("GHOSTTY_SOURCE_DIR") {
        Ok(dir) => {
            let p = PathBuf::from(dir);
            assert!(
                p.join("build.zig").exists(),
                "GHOSTTY_SOURCE_DIR does not contain build.zig: {}",
                p.display()
            );
            (p, None)
        }
        Err(_) => {
            let (src, deps) = fetch_ghostty(&out_dir);
            (src, Some(deps))
        }
    };

    // Build libghostty-vt via zig.
    let install_prefix = out_dir.join("ghostty-install");
    let zig_cache_dir = out_dir.join("zig-cache");
    let zig_global_cache_dir = out_dir.join("zig-global-cache");

    let optimize = zig_optimize_mode();
    let cpu = env::var("LIBGHOSTTY_VT_SYS_CPU").unwrap_or_else(|_| "baseline".to_owned());
    assert!(
        !cpu.is_empty(),
        "LIBGHOSTTY_VT_SYS_CPU must not be empty when set"
    );

    let mut build = Command::new("zig");
    build
        .arg("build")
        .arg("-Demit-lib-vt=true")
        .arg(format!("-Doptimize={optimize}"))
        // Cargo artifacts may run on older CPUs than the build host. Without
        // an explicit CPU model, Zig may emit host-specific instructions that
        // make distributed binaries fail with an illegal instruction. Users
        // building for a known machine can explicitly request `native` or a
        // named Zig CPU model through LIBGHOSTTY_VT_SYS_CPU.
        .arg(format!("-Dcpu={cpu}"))
        .arg("-Demit-xcframework=false")
        .arg("-Dapp-runtime=none")
        .arg("--prefix")
        .arg(&install_prefix)
        .arg("--cache-dir")
        .arg(&zig_cache_dir)
        .current_dir(&ghostty_dir);

    // Package managers can provide Ghostty's Zig package cache ahead of time
    // and ask Zig to resolve packages from that immutable store path instead
    // of fetching during this Cargo build script.
    //
    // vtmux: a fetched release always builds this way, on its unpacked
    // `-deps` tarball, so Zig makes no network request. Zig 0.17 has no
    // `--global-cache-dir` flag (it stops at it) and crashes when `--system`
    // meets the user's own global cache, so the cache is OUT_DIR's, through
    // the environment.
    let zig_system_dir = match env::var("GHOSTTY_ZIG_SYSTEM_DIR") {
        Ok(dir) => {
            assert!(
                !dir.is_empty(),
                "GHOSTTY_ZIG_SYSTEM_DIR must not be empty when set"
            );
            let dir = PathBuf::from(dir);
            assert!(
                dir.exists(),
                "GHOSTTY_ZIG_SYSTEM_DIR does not exist: {}",
                dir.display()
            );
            Some(dir)
        }
        Err(_) => deps,
    };
    if let Some(dir) = zig_system_dir {
        build
            .arg("--system")
            .arg(&dir)
            .env("ZIG_GLOBAL_CACHE_DIR", &zig_global_cache_dir);
    }

    // Only pass -Dtarget when cross-compiling. For native builds, let zig
    // auto-detect the host (matches how ghostty's own CMakeLists.txt works).
    if target != host {
        let zig_target = zig_target(target);
        build.arg(format!("-Dtarget={zig_target}"));
    }

    run(build, "zig build");

    let lib_dir = install_prefix.join("lib");
    let include_dir = install_prefix.join("include");
    let search_dirs = library_search_dirs(target, &install_prefix);
    warn_unused_xcframework(&lib_dir);

    let has_requested_library = search_dirs.iter().any(|dir| {
        std::fs::read_dir(dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", dir.display()))
            .any(|entry| {
                let entry = entry.unwrap_or_else(|error| {
                    panic!("failed to read entry from {}: {error}", dir.display())
                });
                let file_name = entry.file_name();
                let Some(file_name) = file_name.to_str() else {
                    return false;
                };

                link_mode.matches_library(target, file_name)
            })
    });
    assert!(
        has_requested_library,
        "expected libghostty-vt {} in one of {:?}",
        link_mode.artifact_kind(),
        search_dirs
    );
    assert!(
        include_dir.join("ghostty").join("vt.h").exists(),
        "expected header at {}",
        include_dir.join("ghostty").join("vt.h").display()
    );

    for dir in &search_dirs {
        println!("cargo:rustc-link-search=native={}", dir.display());
    }
    match link_mode {
        LinkMode::Dynamic => println!("cargo:rustc-link-lib=dylib=ghostty-vt"),
        LinkMode::Static => emit_static_link_lib(target),
    }
    // vtmux: the headers these bindings must match, for the wrapper's ABI
    // test (`libghostty-vt/tests/abi.rs`, `INCLUDE_DIR`).
    println!(
        "cargo:rustc-env=LIBGHOSTTY_VT_INCLUDE_DIR={}",
        include_dir.display()
    );
    emit_include_metadata(&[include_dir]);
}

/// Emit the link directive for the static archive.
///
/// Zig names static archives `<name>.lib` on every Windows ABI, so neither
/// name rustc derives from a plain `static=ghostty-vt` finds the archive
/// there: MSVC resolves it to `ghostty-vt.lib`, the DLL import library, which
/// links but leaves a load-time dependency on `ghostty-vt.dll`; the GNU
/// targets look for `libghostty-vt.a`, which no Windows build produces, and
/// fail outright. Link the exact file name Ghostty installs instead, which
/// `+verbatim` passes through to rustc's archive lookup untouched.
fn emit_static_link_lib(target: &str) {
    if target.contains("windows") {
        println!("cargo:rustc-link-lib=static:+verbatim={WINDOWS_STATIC_LIB_FILE}");
    } else {
        println!("cargo:rustc-link-lib=static=ghostty-vt");
    }
}

fn warn_unused_xcframework(lib_dir: &Path) {
    let xcframework = lib_dir.join("ghostty-vt.xcframework");
    if xcframework.exists() {
        println!(
            "cargo:warning=unused libghostty-vt XCFramework emitted at {}; Cargo links the dylib or archive directly",
            xcframework.display()
        );
    }
}

#[cfg(feature = "pkg-config")]
fn try_pkg_config(link_mode: LinkMode, target: &str) -> bool {
    let mut config = pkg_config::Config::new();
    let lib = match link_mode {
        LinkMode::Dynamic => config.probe(link_mode.pkg_config_name()),
        LinkMode::Static => config
            .statik(true)
            .cargo_metadata(false)
            .probe(link_mode.pkg_config_name()),
    };
    let lib = match lib {
        Ok(lib) => lib,
        Err(_) => return false,
    };

    if let LinkMode::Static = link_mode {
        emit_static_pkg_config_metadata(&lib, target);
    }
    emit_include_metadata(&lib.include_paths);
    true
}

#[cfg(feature = "pkg-config")]
fn emit_static_pkg_config_metadata(lib: &pkg_config::Library, target: &str) {
    for path in &lib.link_paths {
        println!("cargo:rustc-link-search=native={}", path.display());
    }
    for path in &lib.link_files {
        if let Some(parent) = path.parent() {
            println!("cargo:rustc-link-search=native={}", parent.display());
        }
    }
    for path in &lib.framework_paths {
        println!("cargo:rustc-link-search=framework={}", path.display());
    }
    for framework in &lib.frameworks {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    emit_static_link_lib(target);
    for library in &lib.libs {
        if library != "ghostty-vt" {
            println!("cargo:rustc-link-lib={library}");
        }
    }
    for args in &lib.ld_args {
        if !args.is_empty() {
            println!("cargo:rustc-link-arg=-Wl,{}", args.join(","));
        }
    }
}

fn emit_include_metadata(include_paths: &[PathBuf]) {
    if include_paths.is_empty() {
        return;
    }

    let joined = env::join_paths(include_paths)
        .unwrap_or_else(|error| panic!("failed to join include paths for cargo metadata: {error}"));
    println!("cargo:include={}", joined.to_string_lossy());
}

/// Decide which Zig `OptimizeMode` to pass to `zig build`.
///
/// The `LIBGHOSTTY_VT_SYS_OPTIMIZE` environment variable overrides this unconditionally; accepted
/// values are the four Zig `OptimizeMode` names (`Debug`, `ReleaseSafe`, `ReleaseFast`,
/// `ReleaseSmall`).
///
/// Defaults to `ReleaseFast` for optimized builds. If `DEBUG` is `true` (as cargo sets for the
/// `dev` profile), `Debug` mode is used. Otherwise, if `OPT_LEVEL` is `s` or `z`, `ReleaseSmall`
/// is used.
fn zig_optimize_mode() -> &'static str {
    if let Ok(override_mode) = env::var("LIBGHOSTTY_VT_SYS_OPTIMIZE") {
        return match override_mode.as_str() {
            "Debug" => "Debug",
            "ReleaseSafe" => "ReleaseSafe",
            "ReleaseFast" => "ReleaseFast",
            "ReleaseSmall" => "ReleaseSmall",
            other => panic!(
                "LIBGHOSTTY_VT_SYS_OPTIMIZE must be one of Debug, ReleaseSafe, ReleaseFast, ReleaseSmall (got '{other}')"
            ),
        };
    }

    if env::var("DEBUG").as_deref() == Ok("true") {
        return "Debug";
    }

    match env::var("OPT_LEVEL").as_deref() {
        Ok("s") | Ok("z") => "ReleaseSmall",
        _ => "ReleaseFast",
    }
}

/// vtmux: fail the build, with `message` as cargo shows it (a build
/// script's stderr), not a panic's backtrace hint.
fn fail(message: &str) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
}

/// vtmux: the `zig` on PATH is the one Ghostty's build needs, or the build
/// stops here and says what to install (a `cargo install vtmux` user may
/// have none, or another version, which fails deep in ghostty's build.zig).
fn check_zig() {
    let install = format!(
        "install Zig {ZIG_VERSION} (`mise use -g zig@{ZIG_VERSION}`, or https://ziglang.org/download/) and put it first on PATH"
    );
    let version = match Command::new("zig").arg("version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_owned(),
        Ok(out) => fail(&format!(
            "`zig version` failed ({}): {}; {install}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )),
        Err(e) => fail(&format!(
            "vtmux-ghostty-vt-sys builds Ghostty from source with Zig {ZIG_VERSION}, and there is no `zig` to run ({e}): {install}"
        )),
    };
    if !zig_version_matches(&version) {
        fail(&format!(
            "libghostty-vt {GHOSTTY_VERSION} needs Zig {ZIG_VERSION}, and the `zig` on PATH is {version}: {install}"
        ));
    }
}

/// `zig version` output (`0.17.0`, `0.17.0-dev.12+abc`) of `ZIG_VERSION`'s
/// major.minor.
fn zig_version_matches(version: &str) -> bool {
    version
        .strip_prefix(ZIG_VERSION)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

/// vtmux: the release's source, with vtmux's patches applied, in
/// OUT_DIR/ghostty-src, and its Zig packages in OUT_DIR/ghostty-deps.
/// Reuses both if the stamp matches. The tarballs come from
/// `GHOSTTY_RELEASES` (`GHOSTTY_RELEASE_URL` for a mirror), or from
/// `GHOSTTY_SOURCE_TARBALL` and `GHOSTTY_DEPS_TARBALL` (an offline build, a
/// distribution), checked against their pinned SHA-256 before anything is
/// extracted.
fn fetch_ghostty(out_dir: &Path) -> (PathBuf, PathBuf) {
    let src_dir = out_dir.join("ghostty-src");
    let deps_dir = out_dir.join("ghostty-deps");
    let source = format!("libghostty-vt-{GHOSTTY_VERSION}.tar.gz");
    let deps = format!("libghostty-vt-{GHOSTTY_VERSION}-deps.tar.gz");
    // Both local, or both downloaded: one set alone is a half-offline
    // build that would go to the network for the other.
    match (
        env::var_os("GHOSTTY_SOURCE_TARBALL"),
        env::var_os("GHOSTTY_DEPS_TARBALL"),
    ) {
        (Some(_), None) => fail(&format!(
            "GHOSTTY_SOURCE_TARBALL is set but GHOSTTY_DEPS_TARBALL is not: set both (to {source} and {deps}) or neither"
        )),
        (None, Some(_)) => fail(&format!(
            "GHOSTTY_DEPS_TARBALL is set but GHOSTTY_SOURCE_TARBALL is not: set both (to {source} and {deps}) or neither"
        )),
        _ => {}
    }

    let stamp = src_dir.join(".ghostty-release");

    // vtmux: local patches (`patches/*.patch`, VENDOR.md) applied on top of
    // the release. The stamp records the release and the patches, so
    // changing either extracts afresh.
    let patches = local_patches();
    let mut want = format!("{GHOSTTY_VERSION} {GHOSTTY_SOURCE_SHA256} {GHOSTTY_DEPS_SHA256}");
    for (_, text) in &patches {
        want.push('\n');
        want.push_str(text);
    }

    if stamp.exists()
        && deps_dir.is_dir()
        && let Ok(existing) = std::fs::read_to_string(&stamp)
        && existing == want
    {
        return (src_dir, deps_dir);
    }

    // Clean and extract fresh.
    for dir in [&src_dir, &deps_dir] {
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .unwrap_or_else(|e| panic!("failed to remove {}: {e}", dir.display()));
        }
    }

    for (file, var, sha, dest) in [
        (
            &source,
            "GHOSTTY_SOURCE_TARBALL",
            GHOSTTY_SOURCE_SHA256,
            &src_dir,
        ),
        (
            &deps,
            "GHOSTTY_DEPS_TARBALL",
            GHOSTTY_DEPS_SHA256,
            &deps_dir,
        ),
    ] {
        let (tarball, downloaded) = match env::var_os(var) {
            Some(path) => (PathBuf::from(path), false),
            None => (download_release(out_dir, file, var), true),
        };
        verify_sha256(&tarball, sha, var);
        extract_ghostty(&tarball, out_dir, dest);
        if downloaded {
            let _ = std::fs::remove_file(&tarball);
        }
    }

    // Its own repository, so `git apply` takes the patches' paths from
    // here, not from a repository OUT_DIR is inside (a checkout's target/).
    let mut init = Command::new("git");
    init.arg("init").arg("-q").current_dir(&src_dir);
    run_tool(init, "git", "apply vtmux's patches to Ghostty");
    for (path, _) in &patches {
        let mut apply = Command::new("git");
        apply.arg("apply").arg(path).current_dir(&src_dir);
        run(apply, &format!("git apply {}", path.display()));
    }

    std::fs::write(&stamp, want).unwrap_or_else(|e| panic!("failed to write stamp: {e}"));

    (src_dir, deps_dir)
}

/// vtmux: download release file `file` into OUT_DIR with curl.
fn download_release(out_dir: &Path, file: &str, var: &str) -> PathBuf {
    let base = env::var("GHOSTTY_RELEASE_URL").unwrap_or_else(|_| GHOSTTY_RELEASES.to_owned());
    let url = format!("{}/{GHOSTTY_VERSION}/{file}", base.trim_end_matches('/'));
    let tarball = out_dir.join(file);
    eprintln!("Downloading {url} ...");
    let mut curl = Command::new("curl");
    curl.args(["--fail", "--location", "--silent", "--show-error"])
        .args(["--retry", "3", "--output"])
        .arg(&tarball)
        .arg(&url);
    let status = curl.status().unwrap_or_else(|e| {
        fail(&format!(
            "no `curl` to download libghostty-vt with ({e}): install curl, or download {url} and set {var} to it"
        ))
    });
    if !status.success() {
        fail(&format!(
            "downloading {url} failed ({status}); without network, download it elsewhere and set {var} to the file"
        ));
    }
    tarball
}

/// vtmux: extract `tarball` into `dest`. A release tarball has its tree
/// under one directory (`libghostty-vt-<version>/`, `...-deps/`), which
/// becomes `dest`.
fn extract_ghostty(tarball: &Path, out_dir: &Path, dest: &Path) {
    let tmp = out_dir.join("ghostty-extract");
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)
            .unwrap_or_else(|e| panic!("failed to remove {}: {e}", tmp.display()));
    }
    std::fs::create_dir_all(&tmp)
        .unwrap_or_else(|e| panic!("failed to create {}: {e}", tmp.display()));
    let mut extract = Command::new("tar");
    extract.arg("-xzf").arg(tarball).current_dir(&tmp);
    run_tool(extract, "tar", "extract the libghostty-vt release");
    let entries: Vec<PathBuf> = std::fs::read_dir(&tmp)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", tmp.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    let root = match entries.as_slice() {
        [only] if only.is_dir() => only.clone(),
        _ => tmp.clone(),
    };
    std::fs::rename(&root, dest).unwrap_or_else(|e| {
        panic!(
            "failed to move {} to {}: {e}",
            root.display(),
            dest.display()
        )
    });
    let _ = std::fs::remove_dir_all(&tmp);
}

/// vtmux: `tarball`'s SHA-256 is `want`, or the build stops saying which
/// file (named by `var`) is not the pinned release.
fn verify_sha256(tarball: &Path, want: &str, var: &str) {
    let bytes = std::fs::read(tarball)
        .unwrap_or_else(|e| fail(&format!("{var} {}: {e}", tarball.display())));
    let got = sha256_hex(&bytes);
    if got != want {
        fail(&format!(
            "{} is not libghostty-vt {GHOSTTY_VERSION}'s release file: SHA-256 {got}, expected {want} (set {var} to the right file)",
            tarball.display()
        ));
    }
}

/// vtmux: SHA-256 (FIPS 180-4) as lowercase hex, so the check needs no
/// tool or crate. Two 5-10 MB tarballs per fresh build.
fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (x, y) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *x = x.wrapping_add(y);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

/// vtmux: run a tool the build needs besides Zig, saying which one is
/// missing rather than panicking on the spawn.
fn run_tool(mut command: Command, tool: &str, purpose: &str) {
    let status = command.status().unwrap_or_else(|e| {
        fail(&format!(
            "no `{tool}` to {purpose} with ({e}): install {tool}"
        ))
    });
    if !status.success() {
        fail(&format!("`{tool}` failed to {purpose} ({status})"));
    }
}

/// The vtmux patches to apply to the fetched ghostty, in name order, with
/// their contents.
fn local_patches() -> Vec<(PathBuf, String)> {
    let dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR")).join("patches");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map(|entries| {
            entries
                .filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "patch"))
                .collect()
        })
        .unwrap_or_default();
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()));
            (p, text)
        })
        .collect()
}

fn run(mut command: Command, context: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to execute {context}: {error}"));
    assert!(status.success(), "{context} failed with status {status}");
}

/// Returns directories to search for the built library artifact.
/// On Windows, Zig may place the DLL in `bin/` and the import lib in `lib/`,
/// so both are included.
fn library_search_dirs(target: &str, install_prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![install_prefix.join("lib")];
    if target.contains("windows") {
        dirs.push(install_prefix.join("bin"));
    }
    dirs
}

fn zig_target(target: &str) -> String {
    let value = match target {
        "x86_64-unknown-linux-gnu" => "x86_64-linux-gnu",
        "x86_64-unknown-linux-musl" => "x86_64-linux-musl",
        "aarch64-unknown-linux-gnu" => "aarch64-linux-gnu",
        "aarch64-unknown-linux-musl" => "aarch64-linux-musl",
        "aarch64-apple-darwin" => "aarch64-macos-none",
        "x86_64-apple-darwin" => "x86_64-macos-none",
        "x86_64-pc-windows-gnu" => "x86_64-windows-gnu",
        "aarch64-pc-windows-gnullvm" => "aarch64-windows-gnu",
        "x86_64-pc-windows-msvc" => "x86_64-windows-msvc",
        "aarch64-pc-windows-msvc" => "aarch64-windows-msvc",
        "aarch64-linux-android" => "aarch64-linux-android",
        "x86_64-linux-android" => "x86_64-linux-android",
        other => panic!("unsupported Rust target for vendored build: {other}"),
    };
    value.to_owned()
}
