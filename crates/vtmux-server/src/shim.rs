//! The `tmux` shim (spec D5): a private directory next to the socket
//! holding `tmux` -> this server's own executable, put first on `PATH` for
//! every process the server starts (panes, popups, `run-shell`, `if-shell`,
//! `#()` and hooks). Scripts call `tmux` and must reach this server, not
//! whatever `tmux` the user's `PATH` has: invoked as `tmux` the binary
//! reads `$TMUX` for the socket (tmux.c:562-566), which every child gets.
//!
//! tmux has nothing like it: its children see the `PATH` they were given
//! (compat/DIVERGENCES.md D-022).

use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// `PATH` when a child would have none: tmux's fallback for panes,
/// `_PATH_DEFPATH` (spawn.c:423-424), which glibc's, macOS's and
/// FreeBSD's paths.h and tmux's own compat.h make `/usr/bin:/bin`, and
/// musl's paths.h `/usr/local/bin:/bin:/usr/bin`. Jobs and popups take it
/// too, where tmux gives them none: the shim needs a `PATH` to go first on
/// (compat/DIVERGENCES.md D-022).
#[cfg(not(target_env = "musl"))]
pub const DEFAULT_PATH: &str = "/usr/bin:/bin";
/// As above, musl's.
#[cfg(target_env = "musl")]
pub const DEFAULT_PATH: &str = "/usr/local/bin:/bin:/usr/bin";

/// The shim directory; removed when dropped (the server exits).
#[derive(Debug)]
pub struct Shim {
    dir: PathBuf,
    /// `dir` as `PATH` text.
    text: String,
}

impl Shim {
    /// Create `<socket dir>/.<socket name>.bin/tmux` -> `exe`. A stale
    /// directory left by a server that died is reused; one this user does
    /// not own, or that is not a real directory, is left alone and there is
    /// no shim (an `-S` socket may sit in a shared directory such as /tmp).
    pub fn create(socket: &Path, exe: &Path) -> io::Result<Self> {
        // A relative `-S` path: children change directory, PATH must not.
        let socket = std::path::absolute(socket)?;
        let dir = dir_for(&socket).ok_or_else(|| io::Error::other("no socket name"))?;
        let text = dir
            .to_str()
            .filter(|t| !t.contains(':'))
            .ok_or_else(|| io::Error::other("shim path unusable in PATH"))?
            .to_owned();
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e),
        }
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() || meta.uid() != rustix::process::getuid().as_raw() {
            return Err(io::Error::other("shim directory not ours"));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let link = dir.join("tmux");
        match std::fs::remove_file(&link) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        std::os::unix::fs::symlink(exe, &link)?;
        Ok(Self { dir, text })
    }

    /// `path` with the shim directory first, unless it is already first (a
    /// respawned pane, or a `PATH` copied from a child of this server).
    #[must_use]
    pub fn prepend(&self, path: Option<&str>) -> String {
        let path = path.unwrap_or(DEFAULT_PATH);
        if path.split(':').next() == Some(self.text.as_str()) {
            return path.to_owned();
        }
        if path.is_empty() {
            return self.text.clone();
        }
        format!("{}:{path}", self.text)
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Shim {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.dir.join("tmux"));
        let _ = std::fs::remove_dir(&self.dir);
    }
}

/// `.<name>.bin` beside the socket: hidden, so scripts that list a socket
/// directory's servers (`tmux-UID/*`) see only sockets.
fn dir_for(socket: &Path) -> Option<PathBuf> {
    let name = socket.file_name()?;
    let mut hidden = std::ffi::OsString::from(".");
    hidden.push(name);
    hidden.push(".bin");
    Some(socket.with_file_name(hidden))
}

/// The running executable, symlinks resolved, for the shim to point at.
#[must_use]
pub fn own_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    std::fs::canonicalize(&exe).ok().or(Some(exe))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_reuses_and_removes() {
        let tmp = tempfile_dir();
        let socket = tmp.join("foo");
        let exe = Path::new("/bin/echo");
        let shim = Shim::create(&socket, exe).unwrap();
        assert_eq!(shim.dir(), tmp.join(".foo.bin"));
        assert_eq!(std::fs::read_link(shim.dir().join("tmux")).unwrap(), exe);
        let mode = std::fs::metadata(shim.dir()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        // A stale one (the server died) is taken over.
        std::mem::forget(shim);
        std::fs::set_permissions(tmp.join(".foo.bin"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let shim = Shim::create(&socket, Path::new("/bin/true")).unwrap();
        assert_eq!(
            std::fs::read_link(shim.dir().join("tmux")).unwrap(),
            Path::new("/bin/true")
        );
        let mode = std::fs::metadata(shim.dir()).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o700);
        drop(shim);
        assert!(!tmp.join(".foo.bin").exists());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn a_non_directory_is_not_used() {
        let tmp = tempfile_dir();
        std::os::unix::fs::symlink("/", tmp.join(".foo.bin")).unwrap();
        assert!(Shim::create(&tmp.join("foo"), Path::new("/bin/echo")).is_err());
        std::fs::remove_dir_all(tmp).unwrap();
    }

    #[test]
    fn prepend_once() {
        let shim = Shim {
            dir: PathBuf::from("/s/.x.bin"),
            text: "/s/.x.bin".into(),
        };
        assert_eq!(shim.prepend(Some("/usr/bin")), "/s/.x.bin:/usr/bin");
        assert_eq!(
            shim.prepend(Some("/s/.x.bin:/usr/bin")),
            "/s/.x.bin:/usr/bin"
        );
        assert_eq!(shim.prepend(None), format!("/s/.x.bin:{DEFAULT_PATH}"));
        assert_eq!(shim.prepend(Some("")), "/s/.x.bin");
        std::mem::forget(shim);
    }

    fn tempfile_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "vtmux-shim-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
