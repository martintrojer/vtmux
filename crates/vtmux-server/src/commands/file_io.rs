//! The file side of `save-buffer`, `show-buffer` and `load-buffer`
//! (file.c `file_write`, `file_read`, `file_get_path`).
//!
//! tmux has the client do this I/O (`MSG_WRITE_OPEN` / `MSG_READ_OPEN`) and
//! opens with `O_NONBLOCK`; here the server does it, with the same flags,
//! so a FIFO with no reader is an error and never stalls the server. A
//! FIFO that would block mid-transfer is reported as `EAGAIN`: streaming
//! it needs the client plumbing.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use vtmux_core::error::strerror;

use super::Item;
use crate::server::Server;

/// `file_get_path` with `server_client_get_cwd` (server-client.c:3082):
/// `~/` is the home directory, a relative path is against the attached
/// client's session directory, else the command client's directory.
pub(super) fn path(server: &Server, item: &Item, file: &str) -> PathBuf {
    let joined = if let Some(rest) = file.strip_prefix("~/") {
        let home = crate::server::find_home(&server.global_env).unwrap_or_default();
        PathBuf::from(format!("{}/{rest}", home.display()))
    } else {
        PathBuf::from(file)
    };
    if joined.is_absolute() {
        return joined;
    }
    let cwd = item
        .client
        .and_then(|c| server.client_session(c))
        .and_then(|s| server.model.sessions.get(&s))
        .map_or_else(|| item.cwd.clone(), |s| s.cwd.clone());
    cwd.join(joined)
}

/// `file_write` to a path: create or truncate (or append) and write.
pub(super) fn write(path: &std::path::Path, data: &[u8], append: bool) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .custom_flags(libc::O_NONBLOCK)
        .mode(0o644)
        .open(path)?;
    f.write_all(data)
}

/// `file_read` from a path. A read error of an opened file (a directory)
/// is `EIO`, as the client's bufferevent reports it
/// (`file_read_error_callback`); an attached client's server-side `fopen`
/// reports the real errno.
pub(super) fn read(path: &std::path::Path, attached: bool) -> io::Result<Vec<u8>> {
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let mut data = Vec::new();
    match f.read_to_end(&mut data) {
        Ok(_) => Ok(data),
        Err(e) if attached => Err(e),
        Err(_) => Err(io::Error::from_raw_os_error(libc::EIO)),
    }
}

/// `cmdq_error("%s: %s", strerror(error), path)`.
pub(super) fn describe(err: &io::Error, path: &std::path::Path) -> String {
    format!("{}: {}", strerror(err), path.display())
}

/// Whether the item's client is attached or a control client, for which
/// tmux's `file_write` / `file_read` of `-` fail with `EBADF`.
pub(super) fn is_attached(server: &Server, item: &Item) -> bool {
    item.control.is_some()
        || item
            .client
            .is_some_and(|c| server.client_session(c).is_some())
}
