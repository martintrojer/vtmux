//! The client side of the server's file requests (file.c
//! `file_write_open`, `file_read_open`): tmux has the command client
//! open, read and write files, so a slow or blocked file (a FIFO) holds
//! up only this client, never the server.
//!
//! Files are opened as tmux opens them (`O_NONBLOCK`, so a FIFO with no
//! reader fails with `ENXIO` instead of waiting). A write then switches to
//! blocking for the transfer: this process has nothing else to do. A read
//! waits for the file to poll readable before each `read(2)`, as
//! libevent's bufferevent does: a FIFO that has never had a writer would
//! read as an empty file, but polls readable only once a writer connects.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write as _};
use std::os::fd::AsFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;

use rustix::event::{PollFd, PollFlags, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use vtmux_core::protocol::{self, ClientMsg};

const EIO: i32 = rustix::io::Errno::IO.raw_os_error();

fn errno(e: &io::Error) -> i32 {
    e.raw_os_error().unwrap_or(EIO)
}

fn open(opts: &mut OpenOptions, path: &Path) -> io::Result<File> {
    opts.custom_flags(OFlags::NONBLOCK.bits().cast_signed())
        .open(path)
}

/// Block until `f` polls readable (or hung up, or in error: the next
/// `read(2)` says which).
fn wait_readable(f: &File) -> io::Result<()> {
    loop {
        let mut fds = [PollFd::new(f, PollFlags::IN)];
        match poll(&mut fds, None) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return Err(e.into()),
        }
    }
}

fn blocking(f: File) -> io::Result<File> {
    let flags = fcntl_getfl(&f)?;
    fcntl_setfl(&f, flags - OFlags::NONBLOCK)?;
    Ok(f)
}

/// An open `MSG_WRITE_OPEN` file and its first error.
pub struct Write {
    file: Option<File>,
    error: i32,
}

impl Write {
    /// `O_NONBLOCK|O_WRONLY|O_CREAT` plus `O_TRUNC` or `O_APPEND`, mode
    /// 0644 (file.c:611, 635).
    pub fn open(path: &Path, append: bool) -> Self {
        let mut opts = OpenOptions::new();
        opts.write(true)
            .create(true)
            .append(append)
            .truncate(!append)
            .mode(0o644);
        match open(&mut opts, path).and_then(blocking) {
            Ok(f) => Self {
                file: Some(f),
                error: 0,
            },
            Err(e) => Self {
                file: None,
                error: errno(&e),
            },
        }
    }

    /// A failed write closes the file; its errno is the result
    /// (`file_write_error_callback`).
    pub fn write(&mut self, data: &[u8]) {
        if let Some(f) = self.file.as_mut()
            && let Err(e) = f.write_all(data)
        {
            self.error = errno(&e);
            self.file = None;
        }
    }

    /// `MSG_WRITE_CLOSE`: the errno for `MSG_WRITE_DONE`.
    pub fn close(self) -> i32 {
        self.error
    }
}

/// `MSG_READ_OPEN`: read `path`, or stdin for `None`, to the end and send
/// it as `ReadData` then `FileDone`. A read error is `EIO`
/// (`file_read_error_callback`); an open error its errno.
pub fn read(stream: &mut UnixStream, path: Option<&Path>) {
    let file = match path {
        // dup(STDIN_FILENO), as file_read_open does.
        None => io::stdin().as_fd().try_clone_to_owned().map(File::from),
        Some(p) => open(OpenOptions::new().read(true), p),
    };
    let error = match file {
        Err(e) => errno(&e),
        Ok(mut f) => {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                if wait_readable(&f).is_err() {
                    break EIO;
                }
                match f.read(&mut buf) {
                    Ok(0) => break 0,
                    Ok(n) => {
                        let msg = ClientMsg::ReadData(buf[..n].to_vec());
                        if protocol::write_msg(stream, &msg).is_err() {
                            return;
                        }
                    }
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
                        ) => {}
                    Err(_) => break EIO,
                }
            }
        }
    };
    let _ = protocol::write_msg(stream, &ClientMsg::FileDone { error });
}
