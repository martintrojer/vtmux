//! The programs the harnesses run, by absolute path.
//!
//! `Command::new("ps")` is `posix_spawnp`, and macOS's tries the name in
//! every `PATH` entry with a `posix_spawn` of its own: a process made and
//! failed per entry before the one that holds it. Under `mise exec` `PATH`
//! has about 70 entries and `/usr/bin` comes after 53 of them, so one `ps`
//! cost 54 processes, each run past the host's endpoint-security agents. A
//! path found here once makes it one.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// `name` in the first `PATH` entry that holds it as an executable file,
/// looked up once per name; `name` itself when none does (the spawn then
/// fails as it would have).
pub fn path(name: &str) -> PathBuf {
    static FOUND: OnceLock<Mutex<HashMap<String, PathBuf>>> = OnceLock::new();
    let mut found = FOUND.get_or_init(Mutex::default).lock().expect("progs");
    found
        .entry(name.to_owned())
        .or_insert_with(|| {
            search(name, std::env::var_os("PATH").as_deref()).unwrap_or_else(|| name.into())
        })
        .clone()
}

/// `name` in the first entry of `path_var` that holds it as an executable
/// file. A name with a `/` is used as it is.
fn search(name: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt as _;
    if name.contains('/') {
        return Some(name.into());
    }
    std::env::split_paths(path_var?)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(name))
        .find(|p| {
            p.metadata()
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
}

/// The real `git`, found once. On macOS `xcrun -f git` names the developer
/// tools' own, past any wrapper earlier on `PATH` (a site's `git` that runs
/// telemetry processes of its own on every call); elsewhere, and when xcrun
/// names none, `git` on `PATH`.
pub fn git() -> PathBuf {
    static GIT: OnceLock<PathBuf> = OnceLock::new();
    GIT.get_or_init(|| {
        if cfg!(target_os = "macos")
            && let Some(git) = xcrun_git(&path("xcrun"))
        {
            return git;
        }
        path("git")
    })
    .clone()
}

fn xcrun_git(xcrun: &Path) -> Option<PathBuf> {
    let out = std::process::Command::new(xcrun)
        .args(["-f", "git"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let git = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    (out.status.success() && git.is_absolute() && git.is_file()).then_some(git)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_found_in_the_first_entry_that_runs_it() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("xt-progs-{}", std::process::id()));
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        for d in [&a, &b, &c] {
            std::fs::create_dir_all(d).unwrap();
        }
        // In a: not executable. In b: a directory. In c: the program.
        std::fs::write(a.join("prog"), "").unwrap();
        std::fs::create_dir_all(b.join("prog")).unwrap();
        std::fs::write(c.join("prog"), "").unwrap();
        std::fs::set_permissions(c.join("prog"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let var = std::env::join_paths([
            "",
            a.to_str().unwrap(),
            b.to_str().unwrap(),
            c.to_str().unwrap(),
        ])
        .unwrap();
        let got = search("prog", Some(&var));
        assert_eq!(search("missing", Some(&var)), None);
        assert_eq!(search("./rel/prog", Some(&var)), Some("./rel/prog".into()));
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got, Some(c.join("prog")));
    }

    #[test]
    fn the_real_git_runs() {
        let out = std::process::Command::new(git())
            .arg("--version")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", git().display());
    }
}
