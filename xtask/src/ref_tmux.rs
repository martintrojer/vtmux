//! Resolve (or build) the pinned reference tmux 3.8.
//!
//! Resolution order: `$VTMUX_REFERENCE_TMUX`, then the shared cache
//! `${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux`, then build it
//! from the release tarball. Layout under `<cache>/vtmux/`:
//!
//! - `dl/`        the verified tarball
//! - `build/work/` scratch extraction + build + staging tree (tarball top dir
//!   is `tmux-3.8`); removed on every exit path
//! - `tmux-3.8/`  the install prefix (kept apart from the build trees)
//! - `ref-tmux.lock` exclusive lock serialising resolve-or-build
//!
//! The cache is shared by every worktree, so concurrent cold-cache callers
//! are serialised: whoever holds an exclusive `File::lock` on `ref-tmux.lock`
//! may download, build and publish; everyone else waits, then re-checks the
//! install and finds it done. A broken existing install is replaced only
//! while holding the lock. Because the lock is held, `build/` has no other
//! live user, so leftovers of a killed run are swept before a new build.
//!
//! `$VTMUX_REFERENCE_TMUX_URL` overrides the download URL (mirrors, tests);
//! the sha256 check still applies.

use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::io;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const VERSION_LINE: &str = "tmux 3.8";
const URL: &str = "https://github.com/tmux/tmux/releases/download/3.8/tmux-3.8.tar.gz";
const SHA256: &str = "e79c699c7e949dccd0a4a125e17b8d1261e311b16979dbc8eb34542f3966d82e";
const TARBALL: &str = "tmux-3.8.tar.gz";

const MISSING_DEPS_HINT: &str = "\
configure failed. tmux needs libevent, ncurses, pkg-config and yacc (bison); install them:
  Fedora Atomic: toolbox run -c vtmux-dev mise exec -- cargo xtask ref-tmux
  Fedora:        dnf install libevent-devel ncurses-devel bison
  Debian/Ubuntu: apt install libevent-dev libncurses-dev bison
  macOS:         brew install libevent ncurses pkg-config bison utf8proc (tmux requires utf8proc on macOS)";

#[derive(Debug)]
pub struct RefTmuxError(String);

impl fmt::Display for RefTmuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for RefTmuxError {}

type Result<T> = std::result::Result<T, RefTmuxError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(RefTmuxError(msg.into()))
}

/// CLI entry: print the reference tmux path as the only stdout line.
pub fn run() -> ExitCode {
    match reference_tmux() {
        Ok(path) => {
            println!("{}", path.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("ref-tmux: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Absolute path to a tmux whose `-V` prints exactly `tmux 3.8`, building it
/// into the shared cache if needed.
pub fn reference_tmux() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("VTMUX_REFERENCE_TMUX") {
        // An explicit override that is wrong (even empty) is an error, never a
        // silent fallback.
        if p.is_empty() {
            return err(
                "VTMUX_REFERENCE_TMUX is set but empty; unset it or point it at a tmux 3.8",
            );
        }
        let p = absolute(Path::new(&p))?;
        check_version(&p)?;
        return Ok(p);
    }
    let root = cache_root()?;
    let bin = root.join("tmux-3.8/bin/tmux");
    // Lock-free fast path: a published prefix is only ever replaced under the
    // lock, so a pass here is trustworthy and a failure just falls through.
    if bin.exists() && check_version(&bin).is_ok() {
        return Ok(bin);
    }
    fs::create_dir_all(&root).or_else(|e| err(format!("create {}: {e}", root.display())))?;
    let lock_path = root.join("ref-tmux.lock");
    let lock =
        File::create(&lock_path).or_else(|e| err(format!("open {}: {e}", lock_path.display())))?;
    eprintln!("ref-tmux: waiting for {}", lock_path.display());
    lock.lock()
        .or_else(|e| err(format!("lock {}: {e}", lock_path.display())))?;
    // Another caller may have published while we waited.
    if bin.exists() && check_version(&bin).is_ok() {
        return Ok(bin);
    }
    let url = std::env::var("VTMUX_REFERENCE_TMUX_URL").unwrap_or_else(|_| URL.to_string());
    build(&root, &url, &lock)?;
    check_version(&bin)?;
    Ok(bin)
}

fn absolute(p: &Path) -> Result<PathBuf> {
    std::path::absolute(p).or_else(|e| err(format!("{}: {e}", p.display())))
}

fn cache_root() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => match std::env::var_os("HOME").filter(|v| !v.is_empty()) {
            Some(h) => PathBuf::from(h).join(".cache"),
            None => return err("neither XDG_CACHE_HOME nor HOME is set"),
        },
    };
    Ok(absolute(&base)?.join("vtmux"))
}

fn check_version(bin: &Path) -> Result<()> {
    let out = Command::new(bin)
        .arg("-V")
        .output()
        .or_else(|e| err(format!("{}: {e}", bin.display())))?;
    let got = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && got.trim_end_matches('\n') == VERSION_LINE {
        Ok(())
    } else {
        err(format!(
            "{}: `-V` printed {:?}, expected {VERSION_LINE:?}",
            bin.display(),
            got.trim_end()
        ))
    }
}

/// Removes a file or directory tree when dropped, on every exit path.
struct RemoveOnDrop(PathBuf);

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
        let _ = fs::remove_file(&self.0);
    }
}

/// Download, build and publish. The caller must hold the cache lock (`_lock`).
fn build(root: &Path, url: &str, _lock: &File) -> Result<()> {
    let dl = root.join("dl");
    let build_root = root.join("build");
    let prefix = root.join("tmux-3.8");
    let io = |what: &str, e: io::Error| RefTmuxError(format!("{what}: {e}"));
    fs::create_dir_all(&dl).map_err(|e| io("create dl dir", e))?;
    // We hold the lock, so anything left in `build/` belongs to a dead run.
    let _ = fs::remove_dir_all(&build_root);
    let build = build_root.join("work");
    let _scratch = RemoveOnDrop(build.clone());
    fs::create_dir_all(&build).map_err(|e| io("create build dir", e))?;

    let tarball = fetch(&dl, url)?;
    eprintln!("ref-tmux: extracting into {}", build.display());
    run_cmd(
        Command::new("tar")
            .arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&build),
        "tar",
    )?;
    let src = build.join("tmux-3.8");

    eprintln!("ref-tmux: configure (prefix {})", prefix.display());
    if run_cmd(
        Command::new("./configure")
            .arg(format!("--prefix={}", prefix.display()))
            .current_dir(&src),
        "configure",
    )
    .is_err()
    {
        eprintln!("{MISSING_DEPS_HINT}");
        return err("configure failed");
    }
    let jobs = std::thread::available_parallelism().map_or(1, usize::from);
    run_cmd(
        Command::new("make")
            .arg(format!("-j{jobs}"))
            .current_dir(&src),
        "make",
    )?;
    let stage = build.join("stage");
    run_cmd(
        Command::new("make")
            .arg("install")
            .arg(format!("DESTDIR={}", stage.display()))
            .current_dir(&src),
        "make install",
    )?;
    let staged = stage.join(prefix.strip_prefix("/").unwrap_or(&prefix));
    publish(&staged, &prefix)
}

/// Move a finished install tree to `prefix`, replacing whatever is there (the
/// caller holds the lock and has already found it missing or broken).
fn publish(staged: &Path, prefix: &Path) -> Result<()> {
    match fs::remove_dir_all(prefix) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return err(format!("remove {}: {e}", prefix.display())),
    }
    fs::rename(staged, prefix).or_else(|e| err(format!("install {}: {e}", prefix.display())))
}

/// Return the path of a tarball in `dl` whose sha256 matches the pin.
fn fetch(dl: &Path, url: &str) -> Result<PathBuf> {
    let dest = dl.join(TARBALL);
    if dest.exists() && sha256(&dest)? == SHA256 {
        return Ok(dest);
    }
    // Not there, or fails its checksum: download afresh. The final rename
    // atomically replaces any bad file, so `dest` is never half-written.
    let part = RemoveOnDrop(dl.join(format!("{TARBALL}.part")));
    eprintln!("ref-tmux: downloading {url}");
    download(url, &part.0, &dest).map(|()| dest)
}

fn download(url: &str, part: &Path, dest: &Path) -> Result<()> {
    run_cmd(
        Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--output",
            ])
            .arg(part)
            .arg(url),
        "curl",
    )?;
    let got = sha256(part)?;
    if got != SHA256 {
        return err(format!(
            "refusing {url}: sha256 {got} does not match pinned {SHA256}"
        ));
    }
    fs::rename(part, dest).or_else(|e| err(format!("rename {}: {e}", part.display())))
}

/// Lower-case hex sha256 of a file, via `sha256sum` or (macOS) `shasum -a 256`.
fn sha256(path: &Path) -> Result<String> {
    let attempts: [(&str, &[&str]); 2] = [("sha256sum", &[]), ("shasum", &["-a", "256"])];
    for (prog, args) in attempts {
        match Command::new(prog).args(args).arg(path).output() {
            Ok(out) if out.status.success() => {
                let text = String::from_utf8_lossy(&out.stdout);
                if let Some(h) = text.split_whitespace().next() {
                    return Ok(h.to_ascii_lowercase());
                }
            }
            Ok(_) | Err(_) => {}
        }
    }
    err("need `sha256sum` or `shasum` on PATH to verify the download")
}

/// Run a child with its stdout redirected to our stderr (stdout is reserved
/// for the final path).
fn run_cmd(cmd: &mut Command, what: &str) -> Result<()> {
    let out = io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .or_else(|e| err(format!("{what}: {e}")))?;
    let status = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .status()
        .or_else(|e| err(format!("{what}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        err(format!("{what} failed ({status})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn temp_root(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("vtmux-ref-tmux-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn fake_tmux(prefix: &Path, version_line: &str) {
        let bin = prefix.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let tmux = bin.join("tmux");
        fs::write(&tmux, format!("#!/bin/sh\necho '{version_line}'\n")).unwrap();
        fs::set_permissions(&tmux, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn failed_builds_leave_no_scratch() {
        let root = temp_root("scratch");
        fs::create_dir_all(root.join("build")).unwrap();
        // Stale leftover of a killed run is swept too.
        fs::create_dir_all(root.join("build/stale")).unwrap();
        let lock = File::create(root.join("ref-tmux.lock")).unwrap();
        lock.lock().unwrap();
        for _ in 0..2 {
            let r = build(&root, "file:///nonexistent/tmux-3.8.tar.gz", &lock);
            assert!(r.is_err());
            let left: Vec<_> = fs::read_dir(root.join("build")).unwrap().collect();
            assert!(left.is_empty(), "build dir not empty: {left:?}");
            let dl: Vec<_> = fs::read_dir(root.join("dl")).unwrap().collect();
            assert!(dl.is_empty(), "dl dir not empty: {dl:?}");
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn publish_replaces_wrong_version_prefix() {
        let root = temp_root("publish");
        let prefix = root.join("tmux-3.8");
        let staged = root.join("stage");
        fake_tmux(&prefix, "tmux 3.7");
        fake_tmux(&staged, VERSION_LINE);
        assert!(check_version(&prefix.join("bin/tmux")).is_err());
        publish(&staged, &prefix).unwrap();
        check_version(&prefix.join("bin/tmux")).unwrap();
        fs::remove_dir_all(&root).unwrap();
    }
}
