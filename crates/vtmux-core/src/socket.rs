//! Socket path resolution, as `main()` and `make_label()` in tmux.c do it.
//!
//! Order (tmux.c:557-571): `-S path`, else `-L label`, else the socket path
//! in a valid `$TMUX` (text before the first comma; invalid when empty or
//! starting with a comma), else the label `default`. A label becomes
//! `<dir>/<label>` where `<dir>` is `<candidate>/tmux-UID` (or `vtmux-UID`
//! when invoked as `vtmux`, D20) and the candidates come from
//! `TMUX_SOCK = "$TMUX_TMPDIR:/tmp"` (tmux.h:95): each is expanded
//! (`$VAR` from the environment, unset -> skipped; `expand_path`,
//! tmux.c:122-150) and `realpath`ed (failure -> skipped; tmux.c:180), and
//! only the first survivor is used. So an empty or nonexistent `TMUX_TMPDIR`
//! falls back to `/tmp`, and a symlinked `TMUX_TMPDIR` is resolved.
//!
//! Reference runs (tmux 3.8, `new -d -s a` then `display -p '#{socket_path}'`):
//! `TMUX_TMPDIR=<real>` and `TMUX_TMPDIR=<symlink to real>` -> `<real>/tmux-1000/lab`;
//! `TMUX_TMPDIR=` (empty), `TMUX_TMPDIR=<missing>`, unset -> `/tmp/tmux-1000/lab`;
//! `TMUX=/x/y,1,2` with no -L/-S -> connects to `/x/y`; `TMUX=,1,2` -> ignored.
//! Directory checks (tmux.c:216-232): mode 0750 accepted, 0701 ->
//! `directory <dir> has unsafe permissions`, a symlink in place of the dir ->
//! `<dir> is not a directory`.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

use crate::error::strerror;

/// `TMUX_SOCK_PERM` (tmux.h:98): only the "other" bits are refused.
const SOCK_PERM: u32 = 0o007;

/// Which name we were started under; decides the socket directory (D20).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocation {
    Tmux,
    Vtmux,
}

impl Invocation {
    /// From argv[0]: a file name of exactly `tmux` is [`Invocation::Tmux`],
    /// anything else is [`Invocation::Vtmux`].
    #[must_use]
    pub fn from_argv0(argv0: &OsStr) -> Self {
        if Path::new(argv0).file_name() == Some(OsStr::new("tmux")) {
            Self::Tmux
        } else {
            Self::Vtmux
        }
    }

    fn dir_prefix(self) -> &'static str {
        match self {
            Self::Tmux => "tmux",
            Self::Vtmux => "vtmux",
        }
    }
}

/// Process environment and identity.
pub trait Env {
    fn var(&self, name: &str) -> Option<OsString>;
    fn uid(&self) -> u32;
}

/// What `lstat` tells us about the socket directory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirStat {
    /// A real directory; a symlink to one is not.
    pub is_dir: bool,
    pub uid: u32,
    /// Permission bits (`st_mode & 0o7777`).
    pub mode: u32,
}

/// The filesystem calls `make_label` makes.
pub trait SocketFs {
    fn realpath(&self, path: &Path) -> io::Result<PathBuf>;
    fn lstat(&self, path: &Path) -> io::Result<DirStat>;
    /// `mkdir(path, 0700)` (before umask); an existing path is
    /// [`io::ErrorKind::AlreadyExists`].
    fn mkdir(&self, path: &Path) -> io::Result<()>;
}

/// The real process environment.
pub struct RealEnv;

impl Env for RealEnv {
    fn var(&self, name: &str) -> Option<OsString> {
        std::env::var_os(name)
    }
    fn uid(&self) -> u32 {
        rustix::process::getuid().as_raw()
    }
}

/// The real filesystem.
pub struct RealFs;

impl SocketFs for RealFs {
    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
    fn lstat(&self, path: &Path) -> io::Result<DirStat> {
        let m = std::fs::symlink_metadata(path)?;
        Ok(DirStat {
            is_dir: m.is_dir(),
            uid: m.uid(),
            mode: m.mode() & 0o7777,
        })
    }
    fn mkdir(&self, path: &Path) -> io::Result<()> {
        std::fs::DirBuilder::new().mode(0o700).create(path)
    }
}

/// Why no socket path could be produced. `Display` is tmux's stderr line
/// (tmux.c:204-232); the caller prints it and exits 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SocketError {
    NoSuitablePath,
    CreateDir { dir: PathBuf, cause: String },
    ReadDir { dir: PathBuf, cause: String },
    NotADirectory(PathBuf),
    UnsafePermissions(PathBuf),
}

impl fmt::Display for SocketError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuitablePath => f.write_str("no suitable socket path"),
            Self::CreateDir { dir, cause } => {
                write!(f, "couldn't create directory {} ({cause})", dir.display())
            }
            Self::ReadDir { dir, cause } => {
                write!(f, "couldn't read directory {} ({cause})", dir.display())
            }
            Self::NotADirectory(dir) => write!(f, "{} is not a directory", dir.display()),
            Self::UnsafePermissions(dir) => {
                write!(f, "directory {} has unsafe permissions", dir.display())
            }
        }
    }
}

impl SocketError {
    /// The stderr line (no newline) with path bytes exactly as given:
    /// tmux prints its `char *` paths with `%s`, so a non-UTF-8 path must
    /// not turn into U+FFFD (D20). Use this for stderr, `Display` for logs.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            Self::NoSuitablePath => self.to_string().into_bytes(),
            Self::CreateDir { dir, cause } => {
                path_line("couldn't create directory ", dir, &format!(" ({cause})"))
            }
            Self::ReadDir { dir, cause } => {
                path_line("couldn't read directory ", dir, &format!(" ({cause})"))
            }
            Self::NotADirectory(dir) => path_line("", dir, " is not a directory"),
            Self::UnsafePermissions(dir) => path_line("directory ", dir, " has unsafe permissions"),
        }
    }
}

/// `before` + raw path bytes + `after`.
pub(crate) fn path_line(before: &str, path: &Path, after: &str) -> Vec<u8> {
    let mut line = before.as_bytes().to_vec();
    line.extend_from_slice(path.as_os_str().as_bytes());
    line.extend_from_slice(after.as_bytes());
    line
}

impl std::error::Error for SocketError {}

/// Resolve the server socket path. `label` is `-L`, `path` is `-S`.
///
/// `-S` and a `$TMUX` path are returned as given: nothing is created or
/// checked for them. Only the label route touches the filesystem.
pub fn socket_path(
    inv: Invocation,
    label: Option<&str>,
    path: Option<&Path>,
    env: &dyn Env,
    fs: &dyn SocketFs,
) -> Result<PathBuf, SocketError> {
    if let Some(path) = path {
        return Ok(path.to_owned());
    }
    if label.is_none()
        && let Some(path) = tmux_env_path(env)
    {
        return Ok(path);
    }
    make_label(inv, label.unwrap_or("default"), env, fs)
}

/// The socket part of `$TMUX` (`socket,pid,session`), if usable
/// (tmux.c:562-566).
fn tmux_env_path(env: &dyn Env) -> Option<PathBuf> {
    let value = env.var("TMUX")?;
    let bytes = value.as_bytes();
    if bytes.is_empty() || bytes[0] == b',' {
        return None;
    }
    let end = bytes.iter().position(|&b| b == b',').unwrap_or(bytes.len());
    Some(PathBuf::from(OsStr::from_bytes(&bytes[..end])))
}

/// The first usable directory of `$TMUX_TMPDIR:/tmp` (tmux.c:159-198 with
/// `TMUX_SOCK`).
fn first_candidate(env: &dyn Env, fs: &dyn SocketFs) -> Option<PathBuf> {
    let tmpdir = env.var("TMUX_TMPDIR");
    let mut candidates = tmpdir.into_iter().chain([OsString::from("/tmp")]);
    candidates.find_map(|c| fs.realpath(Path::new(&c)).ok())
}

fn make_label(
    inv: Invocation,
    label: &str,
    env: &dyn Env,
    fs: &dyn SocketFs,
) -> Result<PathBuf, SocketError> {
    let uid = env.uid();
    let base = first_candidate(env, fs).ok_or(SocketError::NoSuitablePath)?;
    // Plain concatenation like tmux's "%s/tmux-%ld" and "%s/%s": a label
    // containing '/' must not be re-rooted the way `Path::join` would.
    let mut dir = base.into_os_string();
    dir.push(format!("/{}-{uid}", inv.dir_prefix()));
    let dir = PathBuf::from(dir);

    match fs.mkdir(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => {
            return Err(SocketError::CreateDir {
                dir,
                cause: strerror(&e),
            });
        }
    }
    let st = fs.lstat(&dir).map_err(|e| SocketError::ReadDir {
        dir: dir.clone(),
        cause: strerror(&e),
    })?;
    if !st.is_dir {
        return Err(SocketError::NotADirectory(dir));
    }
    if st.uid != uid || st.mode & SOCK_PERM != 0 {
        return Err(SocketError::UnsafePermissions(dir));
    }
    let mut sock = dir.into_os_string();
    sock.push("/");
    sock.push(label);
    Ok(PathBuf::from(sock))
}

/// A fixed environment for tests.
#[derive(Clone, Debug, Default)]
pub struct FakeEnv {
    pub vars: HashMap<String, OsString>,
    pub uid: u32,
}

impl FakeEnv {
    #[must_use]
    pub fn new(uid: u32) -> Self {
        Self {
            vars: HashMap::new(),
            uid,
        }
    }

    #[must_use]
    pub fn with(mut self, name: &str, value: &str) -> Self {
        self.vars.insert(name.to_owned(), value.into());
        self
    }
}

impl Env for FakeEnv {
    fn var(&self, name: &str) -> Option<OsString> {
        self.vars.get(name).cloned()
    }
    fn uid(&self) -> u32 {
        self.uid
    }
}

#[derive(Clone, Debug)]
enum FakeNode {
    Dir(DirStat),
    Other(DirStat),
}

/// An in-memory filesystem for tests. `realpath` succeeds exactly for
/// directories that were added (symlinks and files fail, which is all the
/// resolution logic needs); `mkdir` needs an existing parent.
#[derive(Debug, Default)]
pub struct FakeFs {
    nodes: RefCell<BTreeMap<PathBuf, FakeNode>>,
    /// Paths created by `mkdir`, in order.
    pub made: RefCell<Vec<PathBuf>>,
    /// Owner of directories made by `mkdir` (the process uid).
    creator: u32,
}

impl FakeFs {
    /// An empty filesystem whose `mkdir` creates directories owned by `uid`.
    #[must_use]
    pub fn new(uid: u32) -> Self {
        Self {
            creator: uid,
            ..Self::default()
        }
    }

    #[must_use]
    pub fn dir(self, path: &str, uid: u32, mode: u32) -> Self {
        self.nodes.borrow_mut().insert(
            path.into(),
            FakeNode::Dir(DirStat {
                is_dir: true,
                uid,
                mode,
            }),
        );
        self
    }

    /// Something that is not a directory at `path` (a symlink, say).
    #[must_use]
    pub fn symlink(self, path: &str, uid: u32) -> Self {
        self.nodes.borrow_mut().insert(
            path.into(),
            FakeNode::Other(DirStat {
                is_dir: false,
                uid,
                mode: 0o777,
            }),
        );
        self
    }
}

impl SocketFs for FakeFs {
    fn realpath(&self, path: &Path) -> io::Result<PathBuf> {
        match self.nodes.borrow().get(path) {
            Some(FakeNode::Dir(_)) => Ok(path.to_owned()),
            _ => Err(io::Error::from_raw_os_error(2)),
        }
    }
    fn lstat(&self, path: &Path) -> io::Result<DirStat> {
        match self.nodes.borrow().get(path) {
            Some(FakeNode::Dir(s) | FakeNode::Other(s)) => Ok(*s),
            None => Err(io::Error::from_raw_os_error(2)),
        }
    }
    fn mkdir(&self, path: &Path) -> io::Result<()> {
        let mut nodes = self.nodes.borrow_mut();
        if nodes.contains_key(path) {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        let parent_ok = path
            .parent()
            .is_some_and(|p| matches!(nodes.get(p), Some(FakeNode::Dir(_))));
        if !parent_ok {
            return Err(io::Error::from_raw_os_error(2));
        }
        nodes.insert(
            path.to_owned(),
            FakeNode::Dir(DirStat {
                is_dir: true,
                uid: self.creator,
                mode: 0o700,
            }),
        );
        self.made.borrow_mut().push(path.to_owned());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use Invocation::{Tmux, Vtmux};

    const UID: u32 = 1000;

    fn fs() -> FakeFs {
        FakeFs::new(UID)
            .dir("/tmp", 0, 0o1777)
            .dir("/custom", UID, 0o755)
    }

    fn resolve(
        inv: Invocation,
        label: Option<&str>,
        path: Option<&str>,
        env: &FakeEnv,
        fs: &FakeFs,
    ) -> Result<PathBuf, SocketError> {
        socket_path(inv, label, path.map(Path::new), env, fs)
    }

    #[test]
    fn invocation_from_argv0() {
        assert_eq!(Invocation::from_argv0("tmux".as_ref()), Tmux);
        assert_eq!(Invocation::from_argv0("/usr/bin/tmux".as_ref()), Tmux);
        assert_eq!(Invocation::from_argv0("vtmux".as_ref()), Vtmux);
        assert_eq!(
            Invocation::from_argv0("./target/debug/vtmux".as_ref()),
            Vtmux
        );
        assert_eq!(Invocation::from_argv0("tmux-3.8".as_ref()), Vtmux);
        assert_eq!(Invocation::from_argv0("xtmux".as_ref()), Vtmux);
    }

    #[test]
    fn default_label_per_invocation() {
        let env = FakeEnv::new(UID);
        assert_eq!(
            resolve(Tmux, None, None, &env, &fs()).unwrap(),
            Path::new("/tmp/tmux-1000/default")
        );
        assert_eq!(
            resolve(Vtmux, None, None, &env, &fs()).unwrap(),
            Path::new("/tmp/vtmux-1000/default")
        );
    }

    #[test]
    fn the_base_dir_is_created_once() {
        let env = FakeEnv::new(UID);
        let f = fs();
        resolve(Tmux, None, None, &env, &f).unwrap();
        resolve(Tmux, None, None, &env, &f).unwrap();
        assert_eq!(*f.made.borrow(), [PathBuf::from("/tmp/tmux-1000")]);
    }

    #[test]
    fn label_wins_over_tmux_env() {
        let env = FakeEnv::new(UID).with("TMUX", "/x/y,1,2");
        for (inv, want) in [(Tmux, "/tmp/tmux-1000/lab"), (Vtmux, "/tmp/vtmux-1000/lab")] {
            assert_eq!(
                resolve(inv, Some("lab"), None, &env, &fs()).unwrap(),
                Path::new(want)
            );
        }
    }

    #[test]
    fn path_wins_over_everything_and_touches_nothing() {
        let env = FakeEnv::new(UID).with("TMUX", "/x/y,1,2");
        let f = fs();
        for inv in [Tmux, Vtmux] {
            assert_eq!(
                resolve(inv, Some("lab"), Some("/any/where/s"), &env, &f).unwrap(),
                Path::new("/any/where/s")
            );
        }
        assert!(f.made.borrow().is_empty());
    }

    #[test]
    fn valid_tmux_env_gives_its_socket_part() {
        let f = fs();
        for inv in [Tmux, Vtmux] {
            let env = FakeEnv::new(UID).with("TMUX", "/x/y,1,2");
            assert_eq!(
                resolve(inv, None, None, &env, &f).unwrap(),
                Path::new("/x/y")
            );
            let env = FakeEnv::new(UID).with("TMUX", "/x/y");
            assert_eq!(
                resolve(inv, None, None, &env, &f).unwrap(),
                Path::new("/x/y")
            );
        }
        assert!(f.made.borrow().is_empty());
    }

    #[test]
    fn invalid_tmux_env_is_ignored() {
        let f = fs();
        for bad in ["", ",1,2"] {
            let env = FakeEnv::new(UID).with("TMUX", bad);
            assert_eq!(
                resolve(Tmux, None, None, &env, &f).unwrap(),
                Path::new("/tmp/tmux-1000/default"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn tmux_tmpdir_is_used_when_it_resolves() {
        let env = FakeEnv::new(UID).with("TMUX_TMPDIR", "/custom");
        assert_eq!(
            resolve(Tmux, None, None, &env, &fs()).unwrap(),
            Path::new("/custom/tmux-1000/default")
        );
        assert_eq!(
            resolve(Vtmux, Some("l"), None, &env, &fs()).unwrap(),
            Path::new("/custom/vtmux-1000/l")
        );
    }

    #[test]
    fn empty_or_missing_tmux_tmpdir_falls_back_to_tmp() {
        for value in ["", "/does/not/exist"] {
            let env = FakeEnv::new(UID).with("TMUX_TMPDIR", value);
            assert_eq!(
                resolve(Tmux, None, None, &env, &fs()).unwrap(),
                Path::new("/tmp/tmux-1000/default"),
                "{value:?}"
            );
            assert_eq!(
                resolve(Vtmux, None, None, &env, &fs()).unwrap(),
                Path::new("/tmp/vtmux-1000/default"),
                "{value:?}"
            );
        }
    }

    #[test]
    fn no_candidate_at_all() {
        let f = FakeFs::new(UID);
        assert_eq!(
            resolve(Tmux, None, None, &FakeEnv::new(UID), &f),
            Err(SocketError::NoSuitablePath)
        );
        assert_eq!(
            SocketError::NoSuitablePath.to_string(),
            "no suitable socket path"
        );
    }

    #[test]
    fn label_with_a_slash_is_not_re_rooted() {
        let got = resolve(Tmux, Some("/abs/l"), None, &FakeEnv::new(UID), &fs()).unwrap();
        assert_eq!(got, Path::new("/tmp/tmux-1000//abs/l"));
    }

    #[test]
    fn group_bits_are_allowed_other_bits_are_not() {
        let env = FakeEnv::new(UID);
        let ok = fs().dir("/tmp/tmux-1000", UID, 0o750);
        assert!(resolve(Tmux, None, None, &env, &ok).is_ok());
        let bad = fs().dir("/tmp/tmux-1000", UID, 0o701);
        let err = resolve(Tmux, None, None, &env, &bad).unwrap_err();
        assert_eq!(
            err.to_string(),
            "directory /tmp/tmux-1000 has unsafe permissions"
        );
        let bad = fs().dir("/tmp/vtmux-1000", UID, 0o704);
        let err = resolve(Vtmux, None, None, &env, &bad).unwrap_err();
        assert_eq!(
            err.to_string(),
            "directory /tmp/vtmux-1000 has unsafe permissions"
        );
    }

    #[test]
    fn wrong_owner_is_unsafe() {
        let f = fs().dir("/tmp/tmux-1000", 0, 0o700);
        let err = resolve(Tmux, None, None, &FakeEnv::new(UID), &f).unwrap_err();
        assert_eq!(
            err.to_string(),
            "directory /tmp/tmux-1000 has unsafe permissions"
        );
    }

    #[test]
    fn symlink_in_place_of_the_dir_is_not_a_directory() {
        let f = fs().symlink("/tmp/tmux-1000", UID);
        let err = resolve(Tmux, None, None, &FakeEnv::new(UID), &f).unwrap_err();
        assert_eq!(err.to_string(), "/tmp/tmux-1000 is not a directory");
    }

    #[test]
    fn mkdir_failure_text() {
        // /custom exists but its parent chain is irrelevant: make /tmp
        // unwritable by removing it after realpath would succeed.
        struct DenyMkdir(FakeFs);
        impl SocketFs for DenyMkdir {
            fn realpath(&self, p: &Path) -> io::Result<PathBuf> {
                self.0.realpath(p)
            }
            fn lstat(&self, p: &Path) -> io::Result<DirStat> {
                self.0.lstat(p)
            }
            fn mkdir(&self, _: &Path) -> io::Result<()> {
                Err(io::Error::from_raw_os_error(13))
            }
        }
        let err = socket_path(Tmux, None, None, &FakeEnv::new(UID), &DenyMkdir(fs())).unwrap_err();
        assert_eq!(
            err.to_string(),
            "couldn't create directory /tmp/tmux-1000 (Permission denied)"
        );
    }

    // Real filesystem: realpath, lstat and mkdir against a temp dir, with the
    // real uid. Mirrors the reference runs in the module docs.
    mod real {
        use super::*;
        use std::os::unix::fs::{PermissionsExt, symlink};

        fn env_with(tmpdir: &Path) -> RealEnvWith {
            RealEnvWith(tmpdir.as_os_str().to_owned())
        }

        struct RealEnvWith(OsString);
        impl Env for RealEnvWith {
            fn var(&self, name: &str) -> Option<OsString> {
                (name == "TMUX_TMPDIR").then(|| self.0.clone())
            }
            fn uid(&self) -> u32 {
                RealEnv.uid()
            }
        }

        #[test]
        fn creates_0700_and_resolves_symlinked_tmpdir() {
            let t = tempfile::tempdir().unwrap();
            let real = t.path().canonicalize().unwrap().join("real");
            std::fs::create_dir(&real).unwrap();
            let link = t.path().join("link");
            symlink(&real, &link).unwrap();
            let uid = RealEnv.uid();
            for (inv, name) in [(Tmux, "tmux"), (Vtmux, "vtmux")] {
                let got = socket_path(inv, Some("lab"), None, &env_with(&link), &RealFs).unwrap();
                assert_eq!(got, real.join(format!("{name}-{uid}")).join("lab"));
                let mode = std::fs::metadata(got.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode();
                assert_eq!(
                    mode & 0o777 & !0o022,
                    0o700 & !0o022,
                    "created 0700 (before umask)"
                );
            }
        }

        #[test]
        fn real_modes_and_symlinked_dir() {
            let t = tempfile::tempdir().unwrap();
            let base = t.path().canonicalize().unwrap();
            let uid = RealEnv.uid();
            let dir = base.join(format!("tmux-{uid}"));
            std::fs::create_dir(&dir).unwrap();
            let env = env_with(&base);
            let set = |m| std::fs::set_permissions(&dir, PermissionsExt::from_mode(m)).unwrap();

            set(0o750);
            assert!(socket_path(Tmux, Some("q"), None, &env, &RealFs).is_ok());
            set(0o701);
            let err = socket_path(Tmux, Some("q"), None, &env, &RealFs).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("directory {} has unsafe permissions", dir.display())
            );

            set(0o700);
            let moved = base.join("x");
            std::fs::rename(&dir, &moved).unwrap();
            symlink(&moved, &dir).unwrap();
            let err = socket_path(Tmux, Some("q"), None, &env, &RealFs).unwrap_err();
            assert_eq!(
                err.to_string(),
                format!("{} is not a directory", dir.display())
            );
        }
    }
}
