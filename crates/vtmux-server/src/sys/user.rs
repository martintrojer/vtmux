//! The passwd database (through NSS, so `/usr/lib/passwd`, sssd and LDAP
//! users resolve as tmux sees them).

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// The fields of a passwd entry the server uses.
#[derive(Clone, Debug)]
pub struct Passwd {
    /// `pw_name` (`#{user}`, `#{client_user}`).
    pub name: String,
    pub uid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}

/// Copy the fields out of a filled-in `passwd`.
///
/// # Safety
/// Every non-null string pointer in `pw` must be NUL-terminated and valid.
unsafe fn fields(pw: &libc::passwd) -> Passwd {
    let field = |p: *const libc::c_char| {
        if p.is_null() {
            PathBuf::new()
        } else {
            // SAFETY: the caller guarantees the pointer is a valid C string.
            let bytes = unsafe { CStr::from_ptr(p) }.to_bytes();
            // tmux keeps the bytes as they are.
            PathBuf::from(OsStr::from_bytes(bytes))
        }
    };
    Passwd {
        name: field(pw.pw_name).to_string_lossy().into_owned(),
        uid: pw.pw_uid,
        home: field(pw.pw_dir),
        shell: field(pw.pw_shell),
    }
}

/// Read a passwd entry with one of the reentrant lookups.
fn lookup(
    call: impl Fn(*mut libc::passwd, *mut libc::c_char, usize, *mut *mut libc::passwd) -> libc::c_int,
) -> Option<Passwd> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    loop {
        // SAFETY: passwd is a plain C struct; all-zero is a valid value.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        let rc = call(&raw mut pw, buf.as_mut_ptr(), buf.len(), &raw mut result);
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: on success the getpw*_r functions fill `pw` with
        // NUL-terminated strings pointing into `buf`, alive here.
        return Some(unsafe { fields(&pw) });
    }
}

/// `getpwuid(getuid())`.
#[must_use]
pub fn current_user() -> Option<Passwd> {
    let uid = rustix::process::getuid().as_raw();
    // SAFETY: getpwuid_r with valid out-pointers and a buffer of `len` bytes.
    lookup(|pw, buf, len, out| unsafe { libc::getpwuid_r(uid, pw, buf, len, out) })
}

/// `getpwnam(name)`.
#[must_use]
pub fn user_named(name: &str) -> Option<Passwd> {
    let name = CString::new(name).ok()?;
    // SAFETY: getpwnam_r with a NUL-terminated name, valid out-pointers and
    // a buffer of `len` bytes.
    lookup(|pw, buf, len, out| unsafe { libc::getpwnam_r(name.as_ptr(), pw, buf, len, out) })
}

/// `getpwuid(uid)`.
#[must_use]
pub fn user_by_uid(uid: u32) -> Option<Passwd> {
    // SAFETY: getpwuid_r with valid out-pointers and a buffer of `len` bytes.
    lookup(|pw, buf, len, out| unsafe { libc::getpwuid_r(uid, pw, buf, len, out) })
}

/// A group entry's name and id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub name: String,
    pub gid: u32,
}

/// Read a group entry with one of the reentrant lookups.
fn lookup_group(
    call: impl Fn(*mut libc::group, *mut libc::c_char, usize, *mut *mut libc::group) -> libc::c_int,
) -> Option<Group> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    loop {
        // SAFETY: group is a plain C struct; all-zero is a valid value.
        let mut gr: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        let rc = call(&raw mut gr, buf.as_mut_ptr(), buf.len(), &raw mut result);
        if rc == libc::ERANGE && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        if rc != 0 || result.is_null() || gr.gr_name.is_null() {
            return None;
        }
        // SAFETY: on success the getgr*_r functions fill `gr` with
        // NUL-terminated strings pointing into `buf`, alive here.
        let name = unsafe { CStr::from_ptr(gr.gr_name) };
        return Some(Group {
            name: name.to_string_lossy().into_owned(),
            gid: gr.gr_gid,
        });
    }
}

/// `getgrnam(name)`.
#[must_use]
pub fn group_named(name: &str) -> Option<Group> {
    let name = CString::new(name).ok()?;
    // SAFETY: getgrnam_r with a NUL-terminated name, valid out-pointers and
    // a buffer of `len` bytes.
    lookup_group(|gr, buf, len, out| unsafe { libc::getgrnam_r(name.as_ptr(), gr, buf, len, out) })
}

/// `getgrgid(gid)`.
#[must_use]
pub fn group_by_gid(gid: u32) -> Option<Group> {
    // SAFETY: getgrgid_r with valid out-pointers and a buffer of `len` bytes.
    lookup_group(|gr, buf, len, out| unsafe { libc::getgrgid_r(gid, gr, buf, len, out) })
}

/// The uid and gid of the process at the other end of a Unix socket
/// (`getpeereid`, proc.c:331-334); `None` when the system can't say.
#[must_use]
pub fn peer_ids(fd: std::os::fd::BorrowedFd<'_>) -> Option<(u32, u32)> {
    #[cfg(target_os = "linux")]
    {
        let cred = rustix::net::sockopt::socket_peercred(fd).ok()?;
        Some((cred.uid.as_raw(), cred.gid.as_raw()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        use std::os::fd::AsRawFd;
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: getpeereid writes two ids through valid pointers; the fd
        // is borrowed and open for the call.
        let rc = unsafe { libc::getpeereid(fd.as_raw_fd(), &raw mut uid, &raw mut gid) };
        (rc == 0).then_some((uid, gid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_has_a_home() {
        assert!(current_user().is_some_and(|p| !p.home.as_os_str().is_empty()));
        // root's home is /root on Linux and /var/root on macOS.
        assert!(user_named("root").is_some_and(|p| p.home.ends_with("root")));
        assert!(user_named("no-such-user-vtmux").is_none());
        assert!(user_named("root").is_some_and(|p| p.uid == 0));
        assert!(user_by_uid(0).is_some_and(|p| p.home.ends_with("root")));
    }

    #[test]
    fn groups_resolve_both_ways() {
        // gid 0 is root on Linux and wheel on macOS.
        let g = group_by_gid(0).expect("gid 0");
        assert_eq!(group_named(&g.name), Some(g));
        assert!(group_named("no-such-group-vtmux").is_none());
    }

    #[test]
    fn a_socket_pair_peer_is_this_process() {
        use std::os::fd::AsFd;
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let ids = peer_ids(a.as_fd()).expect("peer ids");
        assert_eq!(ids.0, rustix::process::getuid().as_raw());
        assert_eq!(ids.1, rustix::process::getgid().as_raw());
    }

    #[test]
    fn non_utf8_fields_are_kept_as_bytes() {
        let home = CString::new(&b"/home/caf\xe9"[..]).unwrap();
        let shell = CString::new(&b"/bin/s\xffh"[..]).unwrap();
        // SAFETY: passwd is a plain C struct; all-zero is a valid value.
        let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
        pw.pw_dir = home.as_ptr().cast_mut();
        pw.pw_shell = shell.as_ptr().cast_mut();
        // SAFETY: both pointers are NUL-terminated and outlive the call.
        let got = unsafe { fields(&pw) };
        assert_eq!(got.home.as_os_str().as_bytes(), b"/home/caf\xe9");
        assert_eq!(got.shell.as_os_str().as_bytes(), b"/bin/s\xffh");
    }
}
