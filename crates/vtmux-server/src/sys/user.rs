//! The passwd database (through NSS, so `/usr/lib/passwd`, sssd and LDAP
//! users resolve as tmux sees them).

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

/// The fields of a passwd entry the server uses.
#[derive(Clone, Debug)]
pub struct Passwd {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_has_a_home() {
        assert!(current_user().is_some_and(|p| !p.home.as_os_str().is_empty()));
        // root's home is /root on Linux and /var/root on macOS.
        assert!(user_named("root").is_some_and(|p| p.home.ends_with("root")));
        assert!(user_named("no-such-user-vtmux").is_none());
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
