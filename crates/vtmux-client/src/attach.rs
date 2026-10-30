//! Attached client: raw input, signals and encoded server frames, never a grid.
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

use mio::unix::SourceFd;
use mio::{Events, Interest, Poll, Token};
use signal_hook::consts::{SIGHUP, SIGTERM, SIGWINCH};
use signal_hook_mio::v1_0::Signals;
use vtmux_core::protocol::{self, ClientMsg, ProtocolError, ServerMsg};

use crate::tty::RawTty;

/// Called after Ready; the server owns startup and teardown escapes.
#[must_use]
pub fn attach(stream: &mut UnixStream) -> i32 {
    match run(stream) {
        Ok((code, message)) => {
            if let Some(message) = message {
                println!("[{message}]");
            }
            code
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn run(stream: &mut UnixStream) -> io::Result<(i32, Option<String>)> {
    // Install signal handlers before entering raw mode: all returns drop the
    // tty guard, including SIGTERM/SIGHUP (client.c client_signal).
    let mut signals = Signals::new([SIGTERM, SIGHUP, SIGWINCH])?;
    let mut poll = Poll::new()?;
    poll.registry()
        .register(&mut signals, Token(2), Interest::READABLE)?;
    let _raw = RawTty::enter()?;
    let stdin = io::stdin();
    poll.registry().register(
        &mut SourceFd(&stdin.as_raw_fd()),
        Token(0),
        Interest::READABLE,
    )?;
    poll.registry().register(
        &mut SourceFd(&stream.as_raw_fd()),
        Token(1),
        Interest::READABLE,
    )?;
    stream.set_nonblocking(true)?;
    let result = relay(stream, &mut poll, &mut signals);
    let _ = stream.set_nonblocking(false);
    result
}

fn send(stream: &mut UnixStream, msg: &ClientMsg) -> io::Result<()> {
    // Small input messages; blocking only for the duration of this write.
    stream.set_nonblocking(false)?;
    let result = protocol::write_msg(stream, msg).map_err(io::Error::other);
    stream.set_nonblocking(true)?;
    result
}

fn relay(
    stream: &mut UnixStream,
    poll: &mut Poll,
    signals: &mut Signals,
) -> io::Result<(i32, Option<String>)> {
    let mut events = Events::with_capacity(16);
    let mut input = Vec::new();
    send_size(stream)?;
    loop {
        match poll.poll(&mut events, None) {
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        }
        for event in &events {
            match event.token() {
                Token(0) => {
                    let mut bytes = [0; 4096];
                    let n = io::stdin().read(&mut bytes)?;
                    if n == 0 {
                        let _ = send(stream, &ClientMsg::Detach);
                        return Ok((1, Some("lost tty".into())));
                    }
                    send(stream, &ClientMsg::Input(bytes[..n].to_vec()))?;
                }
                Token(2) => {
                    for sig in signals.pending() {
                        if sig == SIGWINCH {
                            send_size(stream)?;
                        } else {
                            let _ = send(stream, &ClientMsg::Detach);
                            return Ok((
                                1,
                                Some(
                                    if sig == SIGTERM {
                                        "terminated"
                                    } else {
                                        "lost tty"
                                    }
                                    .into(),
                                ),
                            ));
                        }
                    }
                }
                _ => {
                    let mut bytes = [0; 16384];
                    loop {
                        match stream.read(&mut bytes) {
                            Ok(0) => return Ok((1, Some("server exited unexpectedly".into()))),
                            Ok(n) => input.extend_from_slice(&bytes[..n]),
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                            Err(e) => return Err(e),
                        }
                        // Decode before another read so EOF cannot discard Exit.
                        loop {
                            match protocol::decode::<ServerMsg>(&input) {
                                Ok((msg, used)) => {
                                    input.drain(..used);
                                    match msg {
                                        ServerMsg::Frame(b) | ServerMsg::Stdout(b) => {
                                            let mut out = io::stdout().lock();
                                            out.write_all(&b)?;
                                            out.flush()?;
                                        }
                                        ServerMsg::Stderr(b) => io::stderr().write_all(&b)?,
                                        ServerMsg::Exit { code, message } => {
                                            return Ok((code, message));
                                        }
                                        ServerMsg::Ready => {}
                                    }
                                }
                                Err(ProtocolError::Truncated) => break,
                                Err(e) => return Err(io::Error::other(e)),
                            }
                        }
                    }
                }
            }
        }
    }
}

fn send_size(stream: &mut UnixStream) -> io::Result<()> {
    let ws = rustix::termios::tcgetwinsize(io::stdin())?;
    send(
        stream,
        &ClientMsg::Resize {
            cols: u32::from(ws.ws_col),
            rows: u32::from(ws.ws_row),
        },
    )
}
