//! The passwd database (through NSS, so `/usr/lib/passwd`, sssd and LDAP
//! users resolve as tmux sees them) and `localtime_r`.

use std::ffi::{CStr, CString, OsStr};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use vtmux_core::format::Tm;

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

/// `localtime_r(epoch)` in the process's zone (`TZ` or /etc/localtime).
#[must_use]
pub fn local_time(epoch: i64) -> Option<Tm> {
    let t: libc::time_t = epoch;
    // SAFETY: tm is a plain C struct; all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r reads `t` and writes into `tm`, both valid. glibc
    // initialises the zone on first use.
    let ok = unsafe { !libc::localtime_r(&raw const t, &raw mut tm).is_null() };
    if !ok {
        return None;
    }
    let zone = if tm.tm_zone.is_null() {
        String::new()
    } else {
        // SAFETY: glibc points tm_zone at a static NUL-terminated string.
        unsafe { CStr::from_ptr(tm.tm_zone) }
            .to_string_lossy()
            .into_owned()
    };
    let small = |v: libc::c_int| u8::try_from(v).unwrap_or(0);
    Some(Tm {
        year: i64::from(tm.tm_year) + 1900,
        month: small(tm.tm_mon + 1),
        day: small(tm.tm_mday),
        hour: small(tm.tm_hour),
        minute: small(tm.tm_min),
        second: small(tm.tm_sec),
        weekday: small(tm.tm_wday),
        yearday: u16::try_from(tm.tm_yday).unwrap_or(0),
        utc_offset: i32::try_from(tm.tm_gmtoff).unwrap_or(0),
        zone,
        epoch,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_user_has_a_home() {
        assert!(current_user().is_some_and(|p| !p.home.as_os_str().is_empty()));
        assert!(user_named("root").is_some_and(|p| p.home == *"/root"));
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

    #[test]
    fn local_time_round_trips_the_epoch() {
        let tm = local_time(1_000_000_000).expect("localtime");
        assert_eq!(tm.epoch, 1_000_000_000);
        assert_eq!(tm.year, 2001);
    }
}
