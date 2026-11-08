//! Attached client: raw input, signals and encoded server frames, never a grid.
use std::collections::VecDeque;
use std::ffi::OsString;
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use signal_hook::consts::{SIGCONT, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};
use signal_hook_mio::v1_0::Signals;
use vtmux_core::protocol::{
    self, ClientMsg, ProtocolError, REATTACH_FLAG, REATTACH_TOKEN_ENV, ServerMsg, UPGRADED_DETACH,
};

use crate::tty::RawTty;

const STDIN: Token = Token(0);
const SOCKET: Token = Token(1);
const SIGNALS: Token = Token(2);
const STDOUT: Token = Token(3);

/// How an attached client comes back after a live upgrade (live-upgrade
/// spec, *Clients*): what it re-executes as, and where it connects.
#[derive(Clone, Debug)]
pub struct Reattach {
    /// Its `argv[0]`, kept across the exec: `tmux` or `vtmux` (D21).
    pub argv0: OsString,
    /// The socket it is connected through.
    pub socket: PathBuf,
    /// The terminal size its `Hello` gave when it is itself a re-attach:
    /// sent again only if the terminal has changed size since, so the
    /// server sees no resize that did not happen.
    pub sent_size: Option<(u32, u32)>,
}

/// Called after Ready; the server owns startup and teardown escapes.
#[must_use]
pub fn attach(stream: &mut UnixStream, how: &Reattach) -> i32 {
    match run(stream, how.sent_size) {
        Ok(Leave::Exit { code, message, hup }) => {
            if let Some(message) = message {
                println!("[{message}]");
            }
            // client.c:414-416: MSG_DETACHKILL hangs up the parent (the
            // login shell, normally) once the reason is printed.
            if hup && let Some(ppid) = rustix::process::getppid().filter(|p| !p.is_init()) {
                let _ = rustix::process::kill_process(ppid, rustix::process::Signal::HUP);
            }
            code
        }
        // The terminal is restored (run dropped its guard): the process
        // becomes the command.
        Ok(Leave::Exec { command, shell }) => exec(&shell, &command),
        // The terminal has its modes back and has taken every frame: the
        // process becomes the new binary, on the same terminal.
        Ok(Leave::Upgrade { binary, token }) => reexec(&binary, &token, how),
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// How the attached client ends.
enum Leave {
    Exit {
        code: i32,
        message: Option<String>,
        hup: bool,
    },
    /// `MSG_EXEC`.
    Exec { command: String, shell: PathBuf },
    /// The frozen `UpgradeNotice`: re-execute `binary` and re-attach.
    Upgrade { binary: PathBuf, token: String },
}

impl Leave {
    fn exit(code: i32, message: &str) -> Self {
        Self::Exit {
            code,
            message: Some(message.to_owned()),
            hup: false,
        }
    }
}

/// `client_exec` (client.c:492-509): `shell -c command` in place of this
/// process, with `SHELL` set to `shell` and a login-shell `argv[0]` when
/// the client is a login shell (`shell_argv0`, which vtmux does not
/// track: `argv[0]` is the shell's basename). Only returns, with the exit
/// status, if the exec fails.
#[must_use]
pub fn exec(shell: &Path, command: &str) -> i32 {
    use std::os::unix::process::CommandExt;
    let name = shell.file_name().unwrap_or(shell.as_os_str());
    let err = std::process::Command::new(shell)
        .arg0(name)
        .args(["-c", command])
        .env("SHELL", shell)
        .exec();
    eprintln!("{}: {err}", shell.display());
    1
}

/// The server is replacing itself with `binary` (live-upgrade spec,
/// decision 4): become `binary`, re-attaching with `token` on the same
/// terminal through the same socket (the frozen re-attach command line,
/// protocol module docs: the token in the environment, where other users
/// can't read it). The server restored the terminal before the notice, as
/// for a detach, so it is the shell's whatever happens next. Only returns,
/// with the exit status, if the exec fails: the client can't follow, so it
/// says why and what the server would have.
#[must_use]
pub fn reexec(binary: &Path, token: &str, how: &Reattach) -> i32 {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(binary)
        .arg0(&how.argv0)
        .arg(REATTACH_FLAG)
        .arg(&how.socket)
        .env(REATTACH_TOKEN_ENV, token)
        .exec();
    eprintln!(
        "can't follow the server's upgrade: {}: {err}",
        binary.display()
    );
    println!("[{UPGRADED_DETACH}]");
    1
}

fn run(stream: &mut UnixStream, sent_size: Option<(u32, u32)>) -> io::Result<Leave> {
    // Install signal handlers before entering raw mode: all returns drop the
    // tty guard, including SIGTERM/SIGHUP (client.c client_signal). SIGINT
    // goes to proc_signal_cb and client_signal ignores it while attached;
    // SIGQUIT is SIG_IGN (proc.c proc_set_signals:254-261). Catching it and
    // doing nothing is the same for a process that never execs.
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGWINCH, SIGINT, SIGQUIT, SIGCONT])?;
    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut signals, SIGNALS, Interest::READABLE)?;
    let raw = RawTty::enter()?;
    let stdin = io::stdin();
    poll.registry()
        .register(&mut SourceFd(&stdin.as_raw_fd()), STDIN, Interest::READABLE)?;
    poll.registry().register(
        &mut SourceFd(&stream.as_raw_fd()),
        SOCKET,
        Interest::READABLE,
    )?;
    // O_NONBLOCK on the tty is shared by stdin and stdout (one open file
    // description), so frames are buffered and written on writability, as
    // tmux's bufferevents do. A regular-file stdout cannot be registered and
    // never blocks.
    let stdout = io::stdout();
    let _ = poll.registry().register(
        &mut SourceFd(&stdout.as_raw_fd()),
        STDOUT,
        Interest::WRITABLE,
    );
    stream.set_nonblocking(true)?;
    let mut out = Output::default();
    let result = relay(stream, &mut poll, &mut signals, &mut out, &raw, sent_size);
    // Teardown escapes are in the buffer: deliver them before the guard
    // restores termios.
    let flushed = out.finish();
    let _ = stream.set_nonblocking(false);
    let result = result?;
    flushed?;
    Ok(result)
}

fn send(stream: &mut UnixStream, msg: &ClientMsg) -> io::Result<()> {
    // Small input messages; blocking only for the duration of this write.
    stream.set_nonblocking(false)?;
    let result = protocol::write_msg(stream, msg).map_err(io::Error::other);
    stream.set_nonblocking(true)?;
    result
}

/// Pending terminal output, written without blocking, and the frames the
/// terminal has taken in full (acknowledged to the server, D14).
#[derive(Default)]
struct Output {
    stdout: Vec<u8>,
    /// Bytes ever written to the terminal.
    written: u64,
    /// Stream offset at which each pending frame has been fully written.
    frame_ends: VecDeque<u64>,
    /// Frames fully written, and how many of those the server knows about.
    done: u64,
    acked: u64,
    /// Stop reading the socket above this much unwritten output.
    high_water: usize,
    /// Stream offset at which the last frame received starts.
    last_frame: u64,
}

impl Output {
    fn push(&mut self, bytes: &[u8], frame: bool) -> io::Result<()> {
        if frame {
            self.last_frame = self.written + self.stdout.len() as u64;
        }
        self.stdout.extend_from_slice(bytes);
        if frame {
            self.frame_ends
                .push_back(self.written + self.stdout.len() as u64);
        }
        self.flush()
    }

    /// Write what the terminal takes now; the rest waits for `STDOUT`.
    fn flush(&mut self) -> io::Result<()> {
        let n = write_some(io::stdout().as_fd(), &self.stdout)?;
        self.wrote(n);
        Ok(())
    }

    /// Wait for the terminal to take everything (exit path).
    fn finish(&mut self) -> io::Result<()> {
        write_all_waiting(io::stdout().as_fd(), &self.stdout)?;
        self.wrote(self.stdout.len());
        Ok(())
    }

    fn wrote(&mut self, n: usize) {
        self.stdout.drain(..n);
        self.written += n as u64;
        while self
            .frame_ends
            .front()
            .is_some_and(|&end| end <= self.written)
        {
            self.frame_ends.pop_front();
            self.done += 1;
        }
    }

    /// Leaving: drop what the terminal has not taken from the frames
    /// before the last one, which is the teardown (`Exit` and `Exec`
    /// follow it). tmux's `tty_close` writes the teardown straight to the
    /// terminal (`tty_stop_tty`, `tty_raw`) and frees the queued output
    /// (tty.c:524-535), so a slow terminal does not hold up a detach.
    fn discard_to_last_frame(&mut self) {
        let unwritten = usize::try_from(self.last_frame.saturating_sub(self.written))
            .unwrap_or(usize::MAX)
            .min(self.stdout.len());
        self.stdout.drain(..unwritten);
        self.written += unwritten as u64;
        self.frame_ends.clear();
    }

    fn full(&self) -> bool {
        self.stdout.len() >= self.high_water
    }

    /// Tell the server about frames written since the last ack. Advisory:
    /// once the server has sent Exit and closed, the write fails (EPIPE or
    /// ECONNRESET, seen on macOS) while Exit is still unread, so a failure
    /// is not an error; the next read reports what the server said.
    fn ack(&mut self, stream: &mut UnixStream) {
        if self.done > self.acked {
            let _ = send(stream, &ClientMsg::FrameAck { seq: self.done });
            self.acked = self.done;
        }
    }
}

/// Write until done or `WouldBlock`; returns the bytes taken.
fn write_some(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<usize> {
    let mut done = 0;
    while done < bytes.len() {
        match rustix::io::write(fd, &bytes[done..]) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => done += n,
            Err(rustix::io::Errno::INTR) => {}
            Err(rustix::io::Errno::AGAIN) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(done)
}

/// Write everything, polling for writability on `WouldBlock`.
fn write_all_waiting(fd: BorrowedFd<'_>, mut bytes: &[u8]) -> io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    while !bytes.is_empty() {
        let n = write_some(fd, bytes)?;
        bytes = &bytes[n..];
        if !bytes.is_empty() {
            let mut fds = [PollFd::new(&fd, PollFlags::OUT)];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) | Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(e.into()),
            }
        }
    }
    Ok(())
}

/// Above `cols*rows*8` unwritten bytes (the server's block threshold,
/// tty.c `TTY_BLOCK_START`) the client stops reading the socket, so a slow
/// terminal backs up into the server, where blocking applies (D14).
fn high_water() -> usize {
    rustix::termios::tcgetwinsize(io::stdin()).map_or(1 << 16, |ws| {
        (usize::from(ws.ws_col) * usize::from(ws.ws_row) * 8).max(4096)
    })
}

/// Send what the terminal has typed until `WouldBlock`.
fn read_stdin(stream: &mut UnixStream) -> io::Result<Option<Leave>> {
    let mut bytes = [0; 4096];
    loop {
        match io::stdin().read(&mut bytes) {
            // The tty is gone: tmux's server drops the client itself
            // (tty_read_callback -> server_client_lost, tty.c:181-189), with
            // no MSG_EXITING and so no focus-out. Close the connection, as it
            // would.
            Ok(0) => return Ok(Some(Leave::exit(1, "lost tty"))),
            Ok(n) => send(stream, &ClientMsg::Input(bytes[..n].to_vec()))?,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// `MSG_SUSPEND` (client.c:784-795): the teardown the server sent is
/// written, the terminal gets its modes back and the client stops itself
/// with SIGTSTP (left at its default action by the binary). In an
/// orphaned process group the kernel discards it and the client stays
/// suspended, as tmux's does.
fn suspend(out: &mut Output, raw: &RawTty) -> io::Result<()> {
    out.finish()?;
    raw.suspend();
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::TSTP);
    Ok(())
}

/// `MSG_LOCK` (client.c:797-803): the teardown and the lock screen the
/// server sent are written, the terminal gets its modes back and
/// `command` runs in the foreground (`system`: `/bin/sh -c`, the client
/// waiting). Then the terminal is raw again, the server is told
/// (`MSG_UNLOCK`) and redraws, and what was typed meanwhile and not read
/// by the command goes to the server.
fn lock(
    stream: &mut UnixStream,
    out: &mut Output,
    raw: &RawTty,
    command: &str,
) -> io::Result<Option<Leave>> {
    out.finish()?;
    raw.suspend();
    // system(3) ignores its result here; so does tmux.
    let _ = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(command)
        .status();
    raw.resume()?;
    // A client that left meanwhile (detach-client, its session killed)
    // has its teardown and Exit waiting on a socket the server has
    // closed: the failed send is ignored and the socket read as usual,
    // where Exit, or EOF for a server that went (kill-server), ends it.
    let _ = send(stream, &ClientMsg::Unlock);
    read_stdin(stream)
}

fn relay(
    stream: &mut UnixStream,
    poll: &mut Poll,
    signals: &mut Signals,
    out: &mut Output,
    raw: &RawTty,
    sent_size: Option<(u32, u32)>,
) -> io::Result<Leave> {
    let mut events = Events::with_capacity(16);
    let mut socket = Socket::default();
    out.high_water = high_water();
    if sent_size.is_none() || sent_size != size().ok() {
        send_size(stream)?;
    }
    loop {
        match poll.poll(&mut events, None) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        }
        for event in &events {
            match event.token() {
                // A suspended client leaves the terminal's input to
                // whoever has it now (tty_stop_tty: event_del(event_in)).
                STDIN if socket.suspended => {}
                STDIN => {
                    if let Some(exit) = read_stdin(stream)? {
                        return Ok(exit);
                    }
                }
                STDOUT => out.flush()?,
                SIGNALS => {
                    for sig in signals.pending() {
                        match sig {
                            SIGWINCH => {
                                out.high_water = high_water();
                                send_size(stream)?;
                            }
                            // Closing the terminal raises SIGHUP as well as
                            // EOF on stdin; with the tty already hung up
                            // it is the same loss as EOF.
                            SIGHUP if tty_hung_up() => {
                                return Ok(Leave::exit(1, "lost tty"));
                            }
                            SIGTERM | SIGHUP => {
                                let _ = send(stream, &ClientMsg::Detach);
                                // client.c:543-544: a suspended client
                                // leaves with no reason on SIGTERM.
                                socket.signal_exit = match sig {
                                    SIGTERM if socket.suspended => None,
                                    SIGTERM => Some("terminated".to_owned()),
                                    _ => Some("lost tty".to_owned()),
                                };
                            }
                            // client.c:551-559: back from the stop, the
                            // terminal is raw again and the server told.
                            // A resize meanwhile comes as SIGWINCH.
                            SIGCONT if socket.suspended => {
                                socket.suspended = false;
                                raw.resume()?;
                                // As for Unlock: a client that left while
                                // stopped finds its Exit on the socket,
                                // read below.
                                let _ = send(stream, &ClientMsg::Wakeup);
                                socket.readable = true;
                                // Input typed before the stop was never
                                // read: edge-triggered, it raises no event.
                                if let Some(exit) = read_stdin(stream)? {
                                    return Ok(exit);
                                }
                            }
                            _ => {} // SIGINT, SIGQUIT, SIGCONT
                        }
                    }
                }
                _ => socket.readable = true,
            }
        }
        if socket.readable
            && !out.full()
            && let Some(exit) = socket.pump(stream, out)?
        {
            return Ok(exit);
        }
        if socket.suspend_now {
            socket.suspend_now = false;
            suspend(out, raw)?;
        }
        if let Some(command) = socket.lock.take() {
            if let Some(exit) = lock(stream, out, raw, &command)? {
                return Ok(exit);
            }
            // What came while the command ran (the redraw, or the Exit of
            // a client that left meanwhile) is read now, not on the next
            // event.
            if let Some(exit) = socket.pump(stream, out)? {
                return Ok(exit);
            }
        }
        out.ack(stream);
    }
}

/// Server socket state. Registration is edge-triggered, so `readable`
/// remembers unread data while reading is paused for backpressure.
#[derive(Default)]
struct Socket {
    input: Vec<u8>,
    readable: bool,
    signal_exit: Option<String>,
    /// `client_suspended`: between `Suspend` and SIGCONT.
    suspended: bool,
    /// `Suspend` arrived: stop once this read is dispatched.
    suspend_now: bool,
    /// `Lock` arrived: run its command once this read is dispatched.
    lock: Option<String>,
}

impl Socket {
    /// Read and dispatch until `WouldBlock`, the high-water mark or Exit.
    fn pump(&mut self, stream: &mut UnixStream, out: &mut Output) -> io::Result<Option<Leave>> {
        let mut bytes = [0; 16384];
        while !out.full() {
            match stream.read(&mut bytes) {
                Ok(0) => return Ok(Some(Leave::exit(1, "server exited unexpectedly"))),
                Ok(n) => self.input.extend_from_slice(&bytes[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    self.readable = false;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
            // Decode before another read so EOF cannot discard Exit.
            loop {
                match protocol::decode::<ServerMsg>(&self.input) {
                    Ok((msg, used)) => {
                        self.input.drain(..used);
                        match msg {
                            ServerMsg::Frame(b) => out.push(&b, true)?,
                            ServerMsg::Stdout(b) => out.push(&b, false)?,
                            ServerMsg::Stderr(b) => {
                                // Keep ordering with frames already queued.
                                out.finish()?;
                                write_all_waiting(io::stderr().as_fd(), &b)?;
                            }
                            ServerMsg::Exit { code, message, hup } => {
                                out.discard_to_last_frame();
                                return Ok(Some(if let Some(reason) = self.signal_exit.take() {
                                    Leave::exit(1, &reason)
                                } else {
                                    Leave::Exit { code, message, hup }
                                }));
                            }
                            ServerMsg::Exec { command, shell } => {
                                out.discard_to_last_frame();
                                return Ok(Some(Leave::Exec { command, shell }));
                            }
                            ServerMsg::Suspend => {
                                self.suspended = true;
                                self.suspend_now = true;
                            }
                            ServerMsg::Lock { command } => self.lock = Some(command),
                            // Nothing more is read: what comes after is
                            // the new image's, for the re-attach.
                            ServerMsg::UpgradeNotice { binary, token } => {
                                return Ok(Some(Leave::Upgrade { binary, token }));
                            }
                            // File requests go to command clients only: an
                            // attached client's I/O is the server's.
                            ServerMsg::Ready
                            | ServerMsg::ReadOpen { .. }
                            | ServerMsg::WriteOpen { .. }
                            | ServerMsg::WriteData(_)
                            | ServerMsg::WriteClose
                            | ServerMsg::Flags { .. }
                            // A result goes to a requester only.
                            | ServerMsg::UpgradeResult { .. } => {}
                        }
                    }
                    Err(ProtocolError::Truncated) => break,
                    Err(e) => return Err(io::Error::other(e)),
                }
            }
            out.ack(stream);
        }
        Ok(None)
    }
}

/// The terminal went away (the master closed): stdin polls `POLLHUP`.
fn tty_hung_up() -> bool {
    use rustix::event::{PollFd, PollFlags};
    let stdin = io::stdin();
    let mut fds = [PollFd::new(&stdin, PollFlags::empty())];
    rustix::event::poll(&mut fds, Some(&rustix::event::Timespec::default()))
        .is_ok_and(|n| n > 0 && fds[0].revents().contains(PollFlags::HUP))
}

/// The terminal's size, columns then rows.
fn size() -> io::Result<(u32, u32)> {
    let ws = rustix::termios::tcgetwinsize(io::stdin())?;
    Ok((u32::from(ws.ws_col), u32::from(ws.ws_row)))
}

/// `MSG_RESIZE`: the server reads the size itself (tty.c `tty_resize`),
/// pixels included; vtmux's server has no terminal, so the client sends it.
fn send_size(stream: &mut UnixStream) -> io::Result<()> {
    let ws = rustix::termios::tcgetwinsize(io::stdin())?;
    send(
        stream,
        &ClientMsg::Resize {
            cols: u32::from(ws.ws_col),
            rows: u32::from(ws.ws_row),
            xpixel: u32::from(ws.ws_xpixel),
            ypixel: u32::from(ws.ws_ypixel),
        },
    )
}
