//! Resolve (or build) the pinned reference tmux 3.8.
//!
//! Resolution order: `$VTMUX_REFERENCE_TMUX`, then the shared cache
//! `${XDG_CACHE_HOME:-$HOME/.cache}/vtmux/tmux-3.8/bin/tmux`, then build it
//! from the release tarball. Layout under `<cache>/vtmux/`:
//!
//! - `dl/`        the verified tarball
//! - `build/`     scratch extraction + build tree (tarball top dir is `tmux-3.8`)
//! - `tmux-3.8/`  the install prefix (kept apart from `build/tmux-3.8`)
//!
//! `$VTMUX_REFERENCE_TMUX_URL` overrides the download URL (mirrors, tests);
//! the sha256 check still applies.

use std::error::Error;
use std::fmt;
use std::fs;
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
  macOS:         brew install libevent ncurses pkg-config bison";

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
    if let Some(p) = std::env::var_os("VTMUX_REFERENCE_TMUX").filter(|p| !p.is_empty()) {
        // An explicit override that is wrong is an error, never a silent fallback.
        let p = absolute(Path::new(&p))?;
        check_version(&p)?;
        return Ok(p);
    }
    let root = cache_root()?;
    let bin = root.join("tmux-3.8/bin/tmux");
    if bin.exists() && check_version(&bin).is_ok() {
        return Ok(bin);
    }
    build(&root)?;
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

fn build(root: &Path) -> Result<()> {
    let dl = root.join("dl");
    let build = root.join("build");
    let prefix = root.join("tmux-3.8");
    let io = |what: &str, e: io::Error| RefTmuxError(format!("{what}: {e}"));
    fs::create_dir_all(&dl).map_err(|e| io("create dl dir", e))?;
    // Fresh build tree every time: a half-built one is worthless.
    let _ = fs::remove_dir_all(&build);
    fs::create_dir_all(&build).map_err(|e| io("create build dir", e))?;

    let tarball = fetch(&dl)?;
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
    run_cmd(
        Command::new("make").arg("install").current_dir(&src),
        "make install",
    )?;
    let _ = fs::remove_dir_all(&build);
    Ok(())
}

/// Return the path of a tarball in `dl` whose sha256 matches the pin.
fn fetch(dl: &Path) -> Result<PathBuf> {
    let dest = dl.join(TARBALL);
    if dest.exists() {
        if sha256(&dest)? == SHA256 {
            return Ok(dest);
        }
        eprintln!(
            "ref-tmux: cached {} fails its checksum; removing",
            dest.display()
        );
        let _ = fs::remove_file(&dest);
    }
    let url = std::env::var("VTMUX_REFERENCE_TMUX_URL").unwrap_or_else(|_| URL.to_string());
    let part = dl.join(format!("{TARBALL}.part"));
    eprintln!("ref-tmux: downloading {url}");
    run_cmd(
        Command::new("curl")
            .args([
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--output",
            ])
            .arg(&part)
            .arg(&url),
        "curl",
    )?;
    let got = sha256(&part)?;
    if got != SHA256 {
        let _ = fs::remove_file(&part);
        return err(format!(
            "refusing {url}: sha256 {got} does not match pinned {SHA256}"
        ));
    }
    fs::rename(&part, &dest).or_else(|e| err(format!("rename {}: {e}", part.display())))?;
    Ok(dest)
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
