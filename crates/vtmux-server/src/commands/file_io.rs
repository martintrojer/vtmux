//! The file side of `save-buffer`, `show-buffer` and `load-buffer`
//! (file.c `file_write`, `file_read`, `file_get_path`).
//!
//! tmux has the command client do this I/O (`MSG_WRITE_OPEN`,
//! `MSG_READ_OPEN`; the client opens with `O_NONBLOCK`), and the command
//! waits (`CMD_RETURN_WAIT`) until the client reports back. vtmux does the
//! same through [`ServerMsg::WriteOpen`] and [`ServerMsg::ReadOpen`]: the
//! batch parks, the client answers [`ClientMsg::FileDone`], and the error,
//! if any, is reported after the command's guard has closed with `%end`,
//! as tmux's done callbacks run `cmdq_error` then.
//!
//! An attached or control client (or none) gets the I/O in the server
//! (`c == NULL || CLIENT_ATTACHED`, file.c:321, 395), opened with
//! `O_NONBLOCK` so a FIFO with no reader is an error and never stalls the
//! server; the result is still reported after the guard. Batches that
//! cannot park (key bindings, config files, `source-file`) finish it at
//! once.
//!
//! [`ServerMsg::WriteOpen`]: vtmux_core::protocol::ServerMsg::WriteOpen
//! [`ServerMsg::ReadOpen`]: vtmux_core::protocol::ServerMsg::ReadOpen
//! [`ClientMsg::FileDone`]: vtmux_core::protocol::ClientMsg::FileDone

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use vtmux_core::error::{EdgeError, strerror};
use vtmux_core::protocol::ServerMsg;

use super::Item;
use crate::server::{ClientId, Server};

/// One file operation a parked batch waits for (`struct client_file`).
#[derive(Debug)]
pub(crate) struct FileOp {
    /// `cf->path`: `-` or the resolved path, for the error message.
    path: String,
    then: Then,
    /// Bytes read so far.
    data: Vec<u8>,
    /// `Some(errno)` once done (0 for success).
    done: Option<i32>,
}

/// What the done callback does.
#[derive(Debug)]
pub(crate) enum Then {
    /// `cmd_save_buffer_done`: report the error.
    Write,
    /// `cmd_load_buffer_done`: set buffer `name` (automatic if `None`)
    /// and, with `-w`, send it to `clip`'s clipboard.
    Load {
        name: Option<String>,
        clip: Option<ClientId>,
    },
}

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

/// Whether the item's client is attached or a control client, for which
/// tmux's `file_write` / `file_read` of `-` fail with `EBADF`.
pub(super) fn is_attached(server: &Server, item: &Item) -> bool {
    item.control.is_some()
        || item
            .client
            .is_some_and(|c| server.client_session(c).is_some())
}

/// The command client that does the I/O, or `None` for the server.
fn client_side(server: &Server, item: &Item) -> Option<ClientId> {
    item.client
        .filter(|_| can_park(server, item) && !is_attached(server, item))
}

fn can_park(server: &Server, item: &Item) -> bool {
    // A blocked command inside source-file would need the rest of the
    // file queued behind it; vtmux runs files in one go (wait_for.rs).
    item.can_wait && item.client.is_some() && server.source_depth == 0
}

/// `file_write`: `data` to `path` (created, truncated or appended).
pub(super) fn write(
    server: &mut Server,
    item: &mut Item,
    path: &Path,
    data: &[u8],
    append: bool,
) -> Result<(), EdgeError> {
    let shown = path.display().to_string();
    if let Some(c) = client_side(server, item) {
        server.send(
            c,
            &ServerMsg::WriteOpen {
                path: path.to_owned(),
                append,
            },
        );
        for chunk in data.chunks(CHUNK) {
            server.send(c, &ServerMsg::WriteData(chunk.to_vec()));
        }
        server.send(c, &ServerMsg::WriteClose);
        park(server, item, c, shown, Then::Write, None);
        return Ok(());
    }
    let error = errno(write_here(path, data, append));
    complete(server, item, shown, Then::Write, Vec::new(), error)
}

/// `file_read`: `path`, or the client's stdin for `None`.
pub(super) fn read(
    server: &mut Server,
    item: &mut Item,
    path: Option<&Path>,
    then: Then,
) -> Result<(), EdgeError> {
    let shown = path.map_or_else(|| "-".to_owned(), |p| p.display().to_string());
    if let Some(c) = client_side(server, item) {
        server.send(
            c,
            &ServerMsg::ReadOpen {
                path: path.map(Path::to_owned),
            },
        );
        park(server, item, c, shown, then, None);
        return Ok(());
    }
    let (data, error) = match path {
        // file.c:383-387: no client, or an attached or control one.
        None => (Vec::new(), libc::EBADF),
        Some(p) => match read_here(p, is_attached(server, item)) {
            Ok(d) => (d, 0),
            Err(e) => (Vec::new(), e.raw_os_error().unwrap_or(libc::EIO)),
        },
    };
    complete(server, item, shown, then, data, error)
}

/// `file_write` of `-` for a client that cannot print: `EBADF`.
pub(super) fn bad_stdout(server: &mut Server, item: &mut Item) -> Result<(), EdgeError> {
    complete(
        server,
        item,
        "-".to_owned(),
        Then::Write,
        Vec::new(),
        libc::EBADF,
    )
}

/// Frames of file data stay well under `MAX_FRAME`.
const CHUNK: usize = 1 << 20;

/// The I/O is done (server side): report now, or after the guard when
/// the batch can park (`file_fire_done` is an event, so the callback runs
/// after the command returned `CMD_RETURN_WAIT`).
fn complete(
    server: &mut Server,
    item: &mut Item,
    path: String,
    then: Then,
    data: Vec<u8>,
    error: i32,
) -> Result<(), EdgeError> {
    if let Some(c) = item.client.filter(|_| can_park(server, item)) {
        park(server, item, c, path, then, Some(error));
        if let Some(op) = server.files.get_mut(&c) {
            op.data = data;
        }
        server.wake(c);
        return Ok(());
    }
    let op = FileOp {
        path,
        then,
        data,
        done: Some(error),
    };
    finish(server, op).map_err(EdgeError::Message)
}

fn park(
    server: &mut Server,
    item: &mut Item,
    c: ClientId,
    path: String,
    then: Then,
    done: Option<i32>,
) {
    item.blocked = true;
    server.files.insert(
        c,
        FileOp {
            path,
            then,
            data: Vec::new(),
            done,
        },
    );
}

/// The done callback: `Err` is the `cmdq_error` message.
fn finish(server: &mut Server, op: FileOp) -> Result<(), String> {
    let error = op.done.unwrap_or(0);
    if error != 0 {
        let e = io::Error::from_raw_os_error(error);
        return Err(format!("{}: {}", strerror(&e), op.path));
    }
    let Then::Load { name, clip } = op.then else {
        return Ok(());
    };
    if op.data.is_empty() {
        return Ok(());
    }
    let limit = super::buffer_limit(server);
    server
        .model
        .paste
        .set(op.data.clone(), name.as_deref(), limit)?;
    // cmd-load-buffer.c:80-83: tty_set_selection for a client with a
    // session.
    if let Some(c) = clip.filter(|&c| server.client_session(c).is_some()) {
        server.set_selection(c, &op.data);
    }
    Ok(())
}

impl Server {
    /// `MSG_READ`: more of the file the client is reading.
    pub(crate) fn file_read_data(&mut self, id: ClientId, bytes: &[u8]) {
        if let Some(op) = self.files.get_mut(&id)
            && op.done.is_none()
        {
            op.data.extend_from_slice(bytes);
        }
    }

    /// `MSG_READ_DONE` / `MSG_WRITE_DONE`: the batch resumes.
    pub(crate) fn file_done(&mut self, id: ClientId, error: i32) {
        if let Some(op) = self.files.get_mut(&id)
            && op.done.is_none()
        {
            op.done = Some(error);
            self.wake(id);
        }
    }

    /// Whether a parked batch may resume: not while its file is open.
    /// Runs the done callback for a finished one, its error after the
    /// guard (`cmdq_error` from the callback).
    pub(crate) fn file_resume(&mut self, id: ClientId, item: &mut Item) -> bool {
        match self.files.get(&id) {
            None => return true,
            Some(op) if op.done.is_none() => return false,
            Some(_) => {}
        }
        let op = self.files.remove(&id).expect("checked above");
        if let Err(msg) = finish(self, op) {
            item.error(self, msg.into_bytes());
        }
        // Control output goes into the stream before the next guard.
        self.control_take_output(id, item);
        true
    }
}

fn errno(r: io::Result<()>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(e) => e.raw_os_error().unwrap_or(libc::EIO),
    }
}

/// `file_write` in the server: create or truncate (or append) and write.
/// tmux uses `fopen` here; vtmux adds `O_NONBLOCK` so a FIFO cannot stall
/// the server (a short write is `EIO`, as `fwrite` reports it).
fn write_here(path: &Path, data: &[u8], append: bool) -> io::Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .create(true)
        .append(append)
        .truncate(!append)
        .custom_flags(libc::O_NONBLOCK)
        .mode(0o644)
        .open(path)?;
    f.write_all(data)
        .map_err(|_| io::Error::from_raw_os_error(libc::EIO))
}

/// `file_read` in the server. A read error of an opened file (a
/// directory) is `EIO` as the client's bufferevent reports it
/// (`file_read_error_callback`); an attached client's server-side `fopen`
/// reports the real errno.
fn read_here(path: &Path, attached: bool) -> io::Result<Vec<u8>> {
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
