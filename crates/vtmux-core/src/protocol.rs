//! Client/server wire protocol (D2: our own, not imsg; D14: the client is a
//! dumb pipe).
//!
//! A frame is `u32 LE payload length` + payload. The payload is one tag byte
//! followed by the variant's fields: integers little-endian, byte strings and
//! strings as `u32 LE length` + bytes, `Option` as a 0/1 byte, vectors as a
//! `u32 LE` count + elements. A frame whose declared length exceeds
//! [`MAX_FRAME`], a frame cut short, an unknown tag, bad UTF-8 or bytes left
//! over after the last field are errors, never panics.
//!
//! A version mismatch is reported the way tmux does (client.c:659-668:
//! `protocol version mismatch (client N, server M)`, exit 1), with vtmux's
//! own numbers; a client newer than the server is also told to run
//! `vtmux-upgrade`. See [`Hello::check_version`].
//!
//! # Frozen frames
//!
//! Most frames change freely: both ends run the same build, and
//! [`PROTOCOL_VERSION`] turns everyone else away. A few frames are frozen
//! (live-upgrade spec, decisions 12 and 26): their tag and encoding never
//! change, so binaries of different versions can still speak them. They are
//! the upgrade handshake and the frames control mode and command I/O use:
//!
//! | frame | tag | payload after the tag byte |
//! |---|---|---|
//! | [`ClientMsg::Hello`] | `0x00` | `version` u32, `invocation` u8 (0 `tmux`, 1 `vtmux`), `pid` u32, `argv` (u32 count, then bytes each), `cwd` bytes, `env` (u32 count, then key string and value string each), `tty` option of (`name` string, `term` string, `cols` u32, `rows` u32, `features` u32 count + strings), `control` u8 (0 none, 1 `-C`, 2 `-CC`), `utf8` bool, `no_start` bool, `ext` bytes |
//! | [`ClientMsg::Stdin`] | `0x03` | bytes |
//! | [`ClientMsg::StdinEof`] | `0x04` | nothing |
//! | [`ClientMsg::UpgradeRequest`] | `0x80` | `binary` bytes (an absolute path) |
//! | [`ServerMsg::Stdout`] | `0x00` | bytes |
//! | [`ServerMsg::Stderr`] | `0x01` | bytes |
//! | [`ServerMsg::Exit`] | `0x03` | `code` i32, `message` option of string, `hup` bool |
//! | [`ServerMsg::Exec`] | `0x09` | `command` string, `shell` bytes |
//! | [`ServerMsg::Flags`] | `0x0c` | `wait_exit` bool |
//! | [`ServerMsg::UpgradeResult`] | `0x80` | `ok` bool, `message` string |
//! | [`ServerMsg::UpgradeNotice`] | `0x81` | `binary` bytes (an absolute path), `token` string (`[A-Za-z0-9]{16,64}`) |
//!
//! With the frame header, that is every byte: u32/i32 little-endian, bytes
//! and strings a u32 length then the bytes (strings UTF-8), a bool or an
//! option's presence one byte 0 or 1. Paths travel as their raw bytes.
//! `tests::frozen_frames_never_change` pins each frame byte for byte: never
//! edit its goldens. To carry something new, add a new frame (new frozen
//! frames take tags from `0x80` up); never change a frozen one.
//!
//! Also frozen:
//! - [`MAX_FRAME`], 16 MiB: no frame's payload is longer, in either
//!   direction, and a longer one is refused ([`ProtocolError::Oversized`]).
//! - `Hello`'s `version` sits at payload bytes 1-4, right after the tag. A
//!   server reads it before the rest ([`hello_version`]), so a `Hello` of
//!   another version gets the version mismatch even when the rest of it
//!   does not decode.
//! - `Hello`'s `ext` is its extension area, empty from protocol 9 on. A
//!   later version appends its new `Hello` fields there in version order;
//!   a reader takes the fields it knows and ignores the rest (protocol 9
//!   ignores it all). Bytes after `ext` are still an error.
//! - The `UpgradeRequest` and `UpgradeNotice` paths are absolute, since
//!   the server's working directory is not the client's: the server refuses
//!   a request with a relative one, and a client refuses such a notice
//!   ([`ProtocolError::Malformed`]), as it does a notice whose token is not
//!   16 to 64 ASCII letters and digits ([`is_upgrade_token`]): the token
//!   goes on the re-executed client's command line.
//! - A control client, the one client served across versions, skips a
//!   server frame whose tag it doesn't know (it has read the whole frame),
//!   so a later server can add frozen frames without ending older control
//!   clients' sessions.
//!
//! What the frozen set buys:
//! - [`ClientMsg::UpgradeRequest`] is answered ahead of any version check, so
//!   a client of any newer binary can ask a server of any older one (from
//!   [`FROZEN_SINCE`]) to move onto it. A peer the server access list
//!   denies or makes read-only is answered `access denied`.
//! - A control client (`-C`) of any version from [`FROZEN_SINCE`] on is
//!   accepted ([`Speaks::Frozen`]): it sends only `Hello`, `Stdin` and
//!   `StdinEof`, and the server sends it frozen frames only, dropping the
//!   rest ([`ServerMsg::is_frozen`]). `Exec` (`detach-client -E`) and
//!   `Flags` (`wait-exit`) are frozen so they reach it too.
//!
//! Known limit: no frozen frame tells a client the server's version. A
//! control client learns it only through a command,
//! `display -p '#{version}'` on its stdin, so a newer control client must
//! not send anything but the three frozen client frames: an older server
//! drops a connection that sends a tag it doesn't know, and reads the other
//! non-frozen tags as its own frames.

use std::fmt;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::error::EdgeError;
use crate::socket::Invocation;

/// Bumped on any incompatible wire change. The frozen frames (module docs)
/// never change, whatever this says.
pub const PROTOCOL_VERSION: u32 = 9;

/// The first protocol version with the frozen frames: a control client of
/// this version or later talks to any server of this version or later.
/// Never changes (live-upgrade spec, decision 14: servers from before it
/// need one last manual restart).
pub const FROZEN_SINCE: u32 = 9;

/// Largest payload accepted (16 MiB). Frames of terminal output are far
/// smaller; this only bounds what a corrupt length can make us allocate.
/// Frozen (module docs): binaries of other versions refuse a longer frame.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The shortest and longest [`ServerMsg::UpgradeNotice`] token (module
/// docs, *Frozen frames*).
pub const UPGRADE_TOKEN_LEN: std::ops::RangeInclusive<usize> = 16..=64;

/// Whether `token` is a valid [`ServerMsg::UpgradeNotice`] token: 16 to
/// 64 ASCII letters and digits, so it is safe on a command line (no `-`
/// in front, no NUL, no shell or option syntax).
#[must_use]
pub fn is_upgrade_token(token: &str) -> bool {
    UPGRADE_TOKEN_LEN.contains(&token.len()) && token.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The protocol version a [`ClientMsg::Hello`] frame at the front of `buf`
/// declares, read without decoding the rest of it: `None` if the frame is
/// not a `Hello` or its first 9 bytes have not come. Frozen (module docs):
/// the version is the four bytes after the tag, so a server can answer a
/// `Hello` of another version that it cannot decode with the mismatch.
#[must_use]
pub fn hello_version(buf: &[u8]) -> Option<u32> {
    let len = u32::from_le_bytes(buf.get(..4)?.try_into().expect("4 bytes"));
    if len < 5 || buf.get(4) != Some(&0) {
        return None;
    }
    Some(u32::from_le_bytes(
        buf.get(5..9)?.try_into().expect("4 bytes"),
    ))
}

/// The client's terminal, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TtyInfo {
    /// `ttyname(3)` of stdin: the client's name (server-client.c:3016-3019).
    pub name: String,
    pub term: String,
    pub cols: u32,
    pub rows: u32,
    pub features: Vec<String>,
}

/// First message on a connection: everything the server needs to run the
/// client's command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub version: u32,
    pub invocation: Invocation,
    /// The client's process ID: a client without a tty is named
    /// `client-PID` (server-client.c:3016-3019).
    pub pid: u32,
    /// The command words as the shell passed them: bytes, not UTF-8
    /// (tmux's argv is `char *`; `set-buffer` data keeps them).
    pub argv: Vec<Vec<u8>>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub tty: Option<TtyInfo>,
    pub flags: ClientFlags,
}

/// How the client was invoked, for the options M0 needs. Each field maps to
/// a tmux flag at the edge only; the wire carries one byte per field.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClientFlags {
    /// `-C` / `-CC`.
    pub control: ControlMode,
    /// `-u`: assume the terminal supports UTF-8.
    pub utf8: bool,
    /// `-N`: do not start a server.
    pub no_start: bool,
}

/// Control mode as selected by `-C` (once) or `-CC` (twice).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ControlMode {
    #[default]
    None,
    Control,
    ControlControl,
}

/// Which frames a client can read, as its [`Hello`] tells.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Speaks {
    /// This build's protocol: every frame.
    #[default]
    Full,
    /// Another version's control client: the frozen frames only (module
    /// docs).
    Frozen,
}

impl Hello {
    /// Which frames the client reads; `Err` if it can't be served: a
    /// different protocol version, unless it is a control client of
    /// [`FROZEN_SINCE`] or later.
    pub fn check_version(&self) -> Result<Speaks, EdgeError> {
        if self.version == PROTOCOL_VERSION {
            Ok(Speaks::Full)
        } else if self.version >= FROZEN_SINCE && self.flags.control != ControlMode::None {
            Ok(Speaks::Frozen)
        } else {
            Err(EdgeError::VersionMismatch {
                client: self.version,
                server: PROTOCOL_VERSION,
            })
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMsg {
    Hello(Hello),
    /// Terminal input bytes from an attached client.
    Input(Vec<u8>),
    Resize {
        cols: u32,
        rows: u32,
    },
    /// Bytes from the client's stdin (command stdin, control mode).
    Stdin(Vec<u8>),
    StdinEof,
    Detach,
    /// The first `seq` [`ServerMsg::Frame`]s on this connection have been
    /// written to the terminal in full. The server counts the frame bytes
    /// not yet acknowledged as the client's queue (backpressure, D14), so
    /// buffers between the server and the terminal cannot hide a slow one.
    FrameAck {
        seq: u64,
    },
    /// Bytes read for the open [`ServerMsg::ReadOpen`] (`MSG_READ`).
    ReadData(Vec<u8>),
    /// The open file operation finished: `error` is an errno, 0 for
    /// success (`MSG_READ_DONE`, `MSG_WRITE_CLOSE` acknowledged).
    FileDone {
        error: i32,
    },
    /// `MSG_WAKEUP`: the client was continued (SIGCONT) after a
    /// [`ServerMsg::Suspend`] and has its raw terminal back.
    Wakeup,
    /// `MSG_UNLOCK`: the [`ServerMsg::Lock`] command has exited and the
    /// client has its raw terminal back.
    Unlock,
    /// Frozen (module docs): move the server onto `binary` (`vtmux-upgrade
    /// -b`), an absolute path. Sent instead of [`ClientMsg::Hello`] and
    /// answered whatever the client's version, with
    /// [`ServerMsg::UpgradeResult`].
    UpgradeRequest {
        binary: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerMsg {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// An encoded terminal frame for an attached client.
    Frame(Vec<u8>),
    Exit {
        code: i32,
        message: Option<String>,
        /// `MSG_DETACHKILL`: after printing `message`, the client sends
        /// SIGHUP to its parent process (client.c:414-416).
        hup: bool,
    },
    /// `MSG_EXEC` (`server_client_exec`): the client replaces itself with
    /// `shell -c command` once its terminal is restored (client.c
    /// `client_exec`).
    Exec {
        command: String,
        shell: PathBuf,
    },
    Ready,
    /// Read a file, or stdin when `path` is `None`, and send it back as
    /// [`ClientMsg::ReadData`] then [`ClientMsg::FileDone`] (file.c
    /// `MSG_READ_OPEN`: tmux has the command client do its file I/O).
    ReadOpen {
        path: Option<PathBuf>,
    },
    /// Open `path` for writing (create, truncate or append) for the
    /// [`ServerMsg::WriteData`] that follow (`MSG_WRITE_OPEN`).
    WriteOpen {
        path: PathBuf,
        append: bool,
    },
    WriteData(Vec<u8>),
    /// No more data: close the file and answer [`ClientMsg::FileDone`].
    WriteClose,
    /// `MSG_SUSPEND` (`suspend-client`): the terminal teardown has been
    /// sent; the client restores its terminal and stops itself with
    /// SIGTSTP (client.c:784-795).
    Suspend,
    /// `MSG_LOCK` (`lock-client` and friends): the terminal teardown has
    /// been sent; the client restores its terminal, runs `command` with
    /// `/bin/sh -c` in the foreground and answers [`ClientMsg::Unlock`]
    /// (client.c:797-803).
    Lock {
        command: String,
    },
    /// `MSG_FLAGS` (`server_client_set_flags`): the client flags the client
    /// itself acts on. `wait_exit`: a control client waits, after printing
    /// `%exit`, for an empty line or the end of its input (client.c:423,
    /// control.c `control_wait_exit`).
    Flags {
        wait_exit: bool,
    },
    /// Frozen (module docs): the answer to [`ClientMsg::UpgradeRequest`],
    /// the last frame on that connection. `message` is what the requester
    /// prints: the reason when `ok` is false, warnings when it is true.
    UpgradeResult {
        ok: bool,
        message: String,
    },
    /// Frozen (module docs): the server is about to replace itself with
    /// `binary`, an absolute path. An attached client re-executes `binary`
    /// and re-attaches with `token` ([`is_upgrade_token`]), which rebinds
    /// it to its client record (live-upgrade spec, decision 21). A notice
    /// breaking either rule does not decode.
    UpgradeNotice {
        binary: PathBuf,
        token: String,
    },
}

impl ServerMsg {
    /// Whether the frame is frozen (module docs): the only frames a client
    /// of another protocol version is sent.
    #[must_use]
    pub fn is_frozen(&self) -> bool {
        matches!(
            self,
            Self::Stdout(_)
                | Self::Stderr(_)
                | Self::Exit { .. }
                | Self::Exec { .. }
                | Self::Flags { .. }
                | Self::UpgradeResult { .. }
                | Self::UpgradeNotice { .. }
        )
    }
}

#[derive(Debug)]
pub enum ProtocolError {
    /// The buffer ends before the frame does (or inside a field).
    Truncated,
    /// The declared payload length is above [`MAX_FRAME`].
    Oversized(usize),
    UnknownTag(u8),
    /// A field is invalid (bad UTF-8, bad option or enum byte).
    Malformed(&'static str),
    /// The payload is longer than its fields.
    TrailingBytes,
    Io(io::Error),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("truncated frame"),
            Self::Oversized(n) => write!(f, "frame too large ({n} bytes)"),
            Self::UnknownTag(t) => write!(f, "unknown message tag {t}"),
            Self::Malformed(what) => write!(f, "malformed frame: {what}"),
            Self::TrailingBytes => f.write_str("trailing bytes in frame"),
            Self::Io(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for ProtocolError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for ProtocolError {
    fn from(e: io::Error) -> Self {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            Self::Truncated
        } else {
            Self::Io(e)
        }
    }
}

/// A message that travels in a frame.
pub trait Message: Sized {
    /// Exact byte length `write_payload` will produce, so [`encode`] can
    /// refuse an oversized message before building it.
    fn payload_len(&self) -> usize;
    fn write_payload(&self, out: &mut Vec<u8>);
    fn read_payload(r: &mut Reader<'_>) -> Result<Self, ProtocolError>;
}

/// Encode `msg` as one frame.
///
/// # Errors
/// [`ProtocolError::Oversized`] if the payload is over [`MAX_FRAME`]; the
/// check happens before any buffer is built, and callers chunk their data.
pub fn encode<M: Message>(msg: &M) -> Result<Vec<u8>, ProtocolError> {
    let len = msg.payload_len();
    if len > MAX_FRAME {
        return Err(ProtocolError::Oversized(len));
    }
    let mut out = Vec::with_capacity(4 + len);
    out.extend_from_slice(&u32::try_from(len).expect("<= MAX_FRAME").to_le_bytes());
    msg.write_payload(&mut out);
    debug_assert_eq!(
        out.len(),
        4 + len,
        "payload_len disagrees with write_payload"
    );
    Ok(out)
}

/// Decode one frame from the front of `buf`; returns the message and the
/// number of bytes it used. An incomplete frame is
/// [`ProtocolError::Truncated`], which a streaming caller reads as "need more
/// bytes".
pub fn decode<M: Message>(buf: &[u8]) -> Result<(M, usize), ProtocolError> {
    let header: [u8; 4] = buf
        .get(..4)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .expect("slice of 4");
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(ProtocolError::Oversized(len));
    }
    let payload = buf.get(4..4 + len).ok_or(ProtocolError::Truncated)?;
    let mut r = Reader { buf: payload };
    let msg = M::read_payload(&mut r)?;
    if !r.buf.is_empty() {
        return Err(ProtocolError::TrailingBytes);
    }
    Ok((msg, 4 + len))
}

/// Write one frame to a stream.
///
/// # Errors
/// [`ProtocolError::Oversized`] (nothing is written) or [`ProtocolError::Io`].
pub fn write_msg<M: Message>(w: &mut impl Write, msg: &M) -> Result<(), ProtocolError> {
    w.write_all(&encode(msg)?)?;
    Ok(())
}

/// Read one frame from a blocking stream. End of stream mid-frame is
/// [`ProtocolError::Truncated`]; a clean end before the first byte is
/// `Ok(None)`.
pub fn read_msg<M: Message>(r: &mut impl Read) -> Result<Option<M>, ProtocolError> {
    let mut header = [0u8; 4];
    let mut got = 0;
    while got < 4 {
        match r.read(&mut header[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(ProtocolError::Truncated),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let len = u32::from_le_bytes(header) as usize;
    if len > MAX_FRAME {
        return Err(ProtocolError::Oversized(len));
    }
    let mut frame = Vec::with_capacity(4 + len);
    frame.extend_from_slice(&header);
    frame.resize(4 + len, 0);
    r.read_exact(&mut frame[4..])?;
    decode(&frame).map(|(m, _)| Some(m))
}

/// Cursor over a payload.
pub struct Reader<'a> {
    buf: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ProtocolError> {
        if self.buf.len() < n {
            return Err(ProtocolError::Truncated);
        }
        let (head, rest) = self.buf.split_at(n);
        self.buf = rest;
        Ok(head)
    }

    fn u8(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, ProtocolError> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, ProtocolError> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }

    fn i32(&mut self) -> Result<i32, ProtocolError> {
        Ok(i32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn flag(&mut self) -> Result<bool, ProtocolError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProtocolError::Malformed("bad bool byte")),
        }
    }

    fn bytes(&mut self) -> Result<Vec<u8>, ProtocolError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn path(&mut self) -> Result<PathBuf, ProtocolError> {
        use std::os::unix::ffi::OsStringExt;
        Ok(PathBuf::from(std::ffi::OsString::from_vec(self.bytes()?)))
    }

    fn string(&mut self) -> Result<String, ProtocolError> {
        String::from_utf8(self.bytes()?).map_err(|_| ProtocolError::Malformed("invalid UTF-8"))
    }

    fn option<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, ProtocolError>,
    ) -> Result<Option<T>, ProtocolError> {
        match self.u8()? {
            0 => Ok(None),
            1 => f(self).map(Some),
            _ => Err(ProtocolError::Malformed("bad option byte")),
        }
    }

    /// A count, checked against the bytes left so a corrupt count cannot
    /// make us reserve gigabytes: every element is at least `min_size` bytes.
    fn count(&mut self, min_size: usize) -> Result<usize, ProtocolError> {
        let n = self.u32()? as usize;
        if n.saturating_mul(min_size) > self.buf.len() {
            return Err(ProtocolError::Truncated);
        }
        Ok(n)
    }

    fn strings(&mut self) -> Result<Vec<String>, ProtocolError> {
        let n = self.count(4)?;
        (0..n).map(|_| self.string()).collect()
    }
}

fn strings_len(v: &[String]) -> usize {
    4 + v.iter().map(|s| 4 + s.len()).sum::<usize>()
}

fn option_len<T>(v: Option<&T>, f: impl FnOnce(&T) -> usize) -> usize {
    1 + v.map_or(0, f)
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_len(out: &mut Vec<u8>, n: usize) {
    put_u32(
        out,
        u32::try_from(n).expect("length fits in u32 (frames are capped)"),
    );
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    put_len(out, b.len());
    out.extend_from_slice(b);
}

fn put_option<T>(out: &mut Vec<u8>, v: Option<&T>, f: impl FnOnce(&mut Vec<u8>, &T)) {
    match v {
        None => out.push(0),
        Some(v) => {
            out.push(1);
            f(out, v);
        }
    }
}

fn put_strings(out: &mut Vec<u8>, v: &[String]) {
    put_len(out, v.len());
    for s in v {
        put_bytes(out, s.as_bytes());
    }
}

impl TtyInfo {
    fn len(&self) -> usize {
        4 + self.name.len() + 4 + self.term.len() + 8 + strings_len(&self.features)
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_bytes(out, self.name.as_bytes());
        put_bytes(out, self.term.as_bytes());
        put_u32(out, self.cols);
        put_u32(out, self.rows);
        put_strings(out, &self.features);
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        Ok(Self {
            name: r.string()?,
            term: r.string()?,
            cols: r.u32()?,
            rows: r.u32()?,
            features: r.strings()?,
        })
    }
}

impl Hello {
    fn write(&self, out: &mut Vec<u8>) {
        put_u32(out, self.version);
        out.push(match self.invocation {
            Invocation::Tmux => 0,
            Invocation::Vtmux => 1,
        });
        put_u32(out, self.pid);
        put_len(out, self.argv.len());
        for word in &self.argv {
            put_bytes(out, word);
        }
        put_bytes(out, self.cwd.as_os_str().as_bytes());
        put_len(out, self.env.len());
        for (k, v) in &self.env {
            put_bytes(out, k.as_bytes());
            put_bytes(out, v.as_bytes());
        }
        put_option(out, self.tty.as_ref(), |o, t| t.write(o));
        out.push(match self.flags.control {
            ControlMode::None => 0,
            ControlMode::Control => 1,
            ControlMode::ControlControl => 2,
        });
        out.push(u8::from(self.flags.utf8));
        out.push(u8::from(self.flags.no_start));
        // ext: empty from protocol 9 on (module docs).
        put_len(out, 0);
    }

    fn len(&self) -> usize {
        4 + 1
            + 4
            + 4
            + self.argv.iter().map(|w| 4 + w.len()).sum::<usize>()
            + 4
            + self.cwd.as_os_str().len()
            + 4
            + self
                .env
                .iter()
                .map(|(k, v)| 8 + k.len() + v.len())
                .sum::<usize>()
            + option_len(self.tty.as_ref(), TtyInfo::len)
            + 3
            + 4
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        let version = r.u32()?;
        let invocation = match r.u8()? {
            0 => Invocation::Tmux,
            1 => Invocation::Vtmux,
            _ => return Err(ProtocolError::Malformed("bad invocation byte")),
        };
        let pid = r.u32()?;
        let n = r.count(4)?;
        let argv = (0..n).map(|_| r.bytes()).collect::<Result<_, _>>()?;
        let cwd = r.path()?;
        let n = r.count(8)?;
        let env = (0..n)
            .map(|_| Ok((r.string()?, r.string()?)))
            .collect::<Result<_, ProtocolError>>()?;
        let tty = r.option(TtyInfo::read)?;
        let control = match r.u8()? {
            0 => ControlMode::None,
            1 => ControlMode::Control,
            2 => ControlMode::ControlControl,
            _ => return Err(ProtocolError::Malformed("bad control mode byte")),
        };
        let flags = ClientFlags {
            control,
            utf8: r.flag()?,
            no_start: r.flag()?,
        };
        // ext: a later version's fields, none of which this one knows.
        let n = r.u32()? as usize;
        r.take(n)?;
        Ok(Self {
            version,
            invocation,
            pid,
            argv,
            cwd,
            env,
            tty,
            flags,
        })
    }
}

impl Message for ClientMsg {
    fn payload_len(&self) -> usize {
        1 + match self {
            Self::Hello(h) => h.len(),
            Self::Input(b) | Self::Stdin(b) | Self::ReadData(b) => 4 + b.len(),
            Self::Resize { .. } | Self::FrameAck { .. } => 8,
            Self::FileDone { .. } => 4,
            Self::StdinEof | Self::Detach | Self::Wakeup | Self::Unlock => 0,
            Self::UpgradeRequest { binary } => 4 + binary.as_os_str().len(),
        }
    }

    fn write_payload(&self, out: &mut Vec<u8>) {
        match self {
            Self::Hello(h) => {
                out.push(0);
                h.write(out);
            }
            Self::Input(b) => {
                out.push(1);
                put_bytes(out, b);
            }
            Self::Resize { cols, rows } => {
                out.push(2);
                put_u32(out, *cols);
                put_u32(out, *rows);
            }
            Self::Stdin(b) => {
                out.push(3);
                put_bytes(out, b);
            }
            Self::StdinEof => out.push(4),
            Self::Detach => out.push(5),
            Self::FrameAck { seq } => {
                out.push(6);
                out.extend_from_slice(&seq.to_le_bytes());
            }
            Self::ReadData(b) => {
                out.push(7);
                put_bytes(out, b);
            }
            Self::FileDone { error } => {
                out.push(8);
                out.extend_from_slice(&error.to_le_bytes());
            }
            Self::Wakeup => out.push(9),
            Self::Unlock => out.push(10),
            Self::UpgradeRequest { binary } => {
                out.push(0x80);
                put_bytes(out, binary.as_os_str().as_bytes());
            }
        }
    }

    fn read_payload(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        Ok(match r.u8()? {
            0 => Self::Hello(Hello::read(r)?),
            1 => Self::Input(r.bytes()?),
            2 => Self::Resize {
                cols: r.u32()?,
                rows: r.u32()?,
            },
            3 => Self::Stdin(r.bytes()?),
            4 => Self::StdinEof,
            5 => Self::Detach,
            6 => Self::FrameAck { seq: r.u64()? },
            7 => Self::ReadData(r.bytes()?),
            8 => Self::FileDone { error: r.i32()? },
            9 => Self::Wakeup,
            10 => Self::Unlock,
            0x80 => Self::UpgradeRequest { binary: r.path()? },
            t => return Err(ProtocolError::UnknownTag(t)),
        })
    }
}

impl Message for ServerMsg {
    fn payload_len(&self) -> usize {
        1 + match self {
            Self::Stdout(b) | Self::Stderr(b) | Self::Frame(b) | Self::WriteData(b) => 4 + b.len(),
            Self::Exit { message, .. } => 4 + option_len(message.as_ref(), |m| 4 + m.len()) + 1,
            Self::Exec { command, shell } => 4 + command.len() + 4 + shell.as_os_str().len(),
            Self::Lock { command } => 4 + command.len(),
            Self::Ready | Self::WriteClose | Self::Suspend => 0,
            Self::Flags { .. } => 1,
            Self::UpgradeResult { message, .. } => 1 + 4 + message.len(),
            Self::UpgradeNotice { binary, token } => 4 + binary.as_os_str().len() + 4 + token.len(),
            Self::ReadOpen { path } => option_len(path.as_ref(), |p| 4 + p.as_os_str().len()),
            Self::WriteOpen { path, .. } => 4 + path.as_os_str().len() + 1,
        }
    }

    fn write_payload(&self, out: &mut Vec<u8>) {
        match self {
            Self::Stdout(b) => {
                out.push(0);
                put_bytes(out, b);
            }
            Self::Stderr(b) => {
                out.push(1);
                put_bytes(out, b);
            }
            Self::Frame(b) => {
                out.push(2);
                put_bytes(out, b);
            }
            Self::Exit { code, message, hup } => {
                out.push(3);
                out.extend_from_slice(&code.to_le_bytes());
                put_option(out, message.as_ref(), |o, m| put_bytes(o, m.as_bytes()));
                out.push(u8::from(*hup));
            }
            Self::Exec { command, shell } => {
                out.push(9);
                put_bytes(out, command.as_bytes());
                put_bytes(out, shell.as_os_str().as_bytes());
            }
            Self::Ready => out.push(4),
            Self::ReadOpen { path } => {
                out.push(5);
                put_option(out, path.as_ref(), |o, p| {
                    put_bytes(o, p.as_os_str().as_bytes());
                });
            }
            Self::WriteOpen { path, append } => {
                out.push(6);
                put_bytes(out, path.as_os_str().as_bytes());
                out.push(u8::from(*append));
            }
            Self::WriteData(b) => {
                out.push(7);
                put_bytes(out, b);
            }
            Self::WriteClose => out.push(8),
            Self::Suspend => out.push(10),
            Self::Lock { command } => {
                out.push(11);
                put_bytes(out, command.as_bytes());
            }
            Self::Flags { wait_exit } => {
                out.push(12);
                out.push(u8::from(*wait_exit));
            }
            Self::UpgradeResult { ok, message } => {
                out.push(0x80);
                out.push(u8::from(*ok));
                put_bytes(out, message.as_bytes());
            }
            Self::UpgradeNotice { binary, token } => {
                out.push(0x81);
                put_bytes(out, binary.as_os_str().as_bytes());
                put_bytes(out, token.as_bytes());
            }
        }
    }

    fn read_payload(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        Ok(match r.u8()? {
            0 => Self::Stdout(r.bytes()?),
            1 => Self::Stderr(r.bytes()?),
            2 => Self::Frame(r.bytes()?),
            3 => Self::Exit {
                code: r.i32()?,
                message: r.option(Reader::string)?,
                hup: r.flag()?,
            },
            4 => Self::Ready,
            5 => Self::ReadOpen {
                path: r.option(Reader::path)?,
            },
            6 => Self::WriteOpen {
                path: r.path()?,
                append: r.flag()?,
            },
            7 => Self::WriteData(r.bytes()?),
            8 => Self::WriteClose,
            9 => Self::Exec {
                command: r.string()?,
                shell: r.path()?,
            },
            10 => Self::Suspend,
            11 => Self::Lock {
                command: r.string()?,
            },
            12 => Self::Flags {
                wait_exit: r.flag()?,
            },
            0x80 => Self::UpgradeResult {
                ok: r.flag()?,
                message: r.string()?,
            },
            0x81 => {
                let binary = r.path()?;
                if !binary.is_absolute() {
                    return Err(ProtocolError::Malformed("upgrade binary not absolute"));
                }
                let token = r.string()?;
                if !is_upgrade_token(&token) {
                    return Err(ProtocolError::Malformed("bad upgrade token"));
                }
                Self::UpgradeNotice { binary, token }
            }
            t => return Err(ProtocolError::UnknownTag(t)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(tty: Option<TtyInfo>) -> Hello {
        Hello {
            version: PROTOCOL_VERSION,
            invocation: Invocation::Vtmux,
            pid: 4242,
            argv: vec![
                b"new-session".to_vec(),
                b"-d".to_vec(),
                b"-s".to_vec(),
                "ä".into(),
                b"\xff\x80".to_vec(),
            ],
            cwd: PathBuf::from("/home/u"),
            env: vec![
                ("TERM".into(), "xterm-256color".into()),
                ("E".into(), String::new()),
            ],
            tty,
            flags: ClientFlags {
                control: ControlMode::ControlControl,
                utf8: true,
                no_start: false,
            },
        }
    }

    fn tty() -> TtyInfo {
        TtyInfo {
            name: "/dev/pts/3".into(),
            term: "xterm-256color".into(),
            cols: 80,
            rows: 24,
            features: vec!["RGB".into(), "mouse".into()],
        }
    }

    fn all_client() -> Vec<ClientMsg> {
        vec![
            ClientMsg::Hello(hello(None)),
            ClientMsg::Hello(hello(Some(tty()))),
            ClientMsg::Input(vec![0x1b, b'[', b'A', 0]),
            ClientMsg::Input(vec![]),
            ClientMsg::Resize {
                cols: 200,
                rows: 50,
            },
            ClientMsg::Stdin(b"display -p hi\n".to_vec()),
            ClientMsg::StdinEof,
            ClientMsg::Detach,
            ClientMsg::FrameAck { seq: 0 },
            ClientMsg::FrameAck { seq: u64::MAX - 1 },
            ClientMsg::ReadData(b"from stdin\xff".to_vec()),
            ClientMsg::FileDone { error: 0 },
            ClientMsg::FileDone { error: 32 },
            ClientMsg::Wakeup,
            ClientMsg::Unlock,
            ClientMsg::UpgradeRequest {
                binary: PathBuf::from("/opt/v\u{e9}/bin/vtmux"),
            },
        ]
    }

    fn all_server() -> Vec<ServerMsg> {
        vec![
            ServerMsg::Stdout(b"out".to_vec()),
            ServerMsg::Stderr(b"no server running on /x\n".to_vec()),
            ServerMsg::Frame(vec![0xff; 1000]),
            ServerMsg::Exit {
                code: 0,
                message: None,
                hup: false,
            },
            ServerMsg::Exit {
                code: -1,
                message: Some("exited".into()),
                hup: false,
            },
            ServerMsg::Exit {
                code: 0,
                message: Some("detached and SIGHUP (from session a)".into()),
                hup: true,
            },
            ServerMsg::Exec {
                command: "echo hi".into(),
                shell: PathBuf::from("/bin/sh"),
            },
            ServerMsg::Ready,
            ServerMsg::ReadOpen { path: None },
            ServerMsg::ReadOpen {
                path: Some(PathBuf::from("/tmp/f")),
            },
            ServerMsg::WriteOpen {
                path: PathBuf::from("/tmp/\u{e9}"),
                append: true,
            },
            ServerMsg::WriteData(vec![0, 1, 2]),
            ServerMsg::WriteClose,
            ServerMsg::Suspend,
            ServerMsg::Lock {
                command: "printf locked >/tmp/x".into(),
            },
            ServerMsg::Flags { wait_exit: true },
            ServerMsg::Flags { wait_exit: false },
            ServerMsg::UpgradeResult {
                ok: false,
                message: "not implemented".into(),
            },
            ServerMsg::UpgradeResult {
                ok: true,
                message: String::new(),
            },
            ServerMsg::UpgradeNotice {
                binary: PathBuf::from("/usr/bin/vtmux"),
                token: "3f9aB7c2d4E5f6a8".into(),
            },
        ]
    }

    fn round_trip<M: Message + PartialEq + fmt::Debug>(m: &M) {
        let bytes = encode(m).unwrap();
        let (back, used) = decode::<M>(&bytes).unwrap();
        assert_eq!(&back, m);
        assert_eq!(used, bytes.len());
        let read = read_msg::<M>(&mut bytes.as_slice()).unwrap().unwrap();
        assert_eq!(&read, m);
    }

    #[test]
    fn every_variant_round_trips() {
        all_client().iter().for_each(round_trip);
        all_server().iter().for_each(round_trip);
    }

    #[test]
    fn frames_decode_back_to_back() {
        let mut stream = encode(&ClientMsg::StdinEof).unwrap();
        stream.extend(encode(&ClientMsg::Detach).unwrap());
        let (a, n) = decode::<ClientMsg>(&stream).unwrap();
        let (b, m) = decode::<ClientMsg>(&stream[n..]).unwrap();
        assert_eq!(
            (a, b, n + m),
            (ClientMsg::StdinEof, ClientMsg::Detach, stream.len())
        );
    }

    #[test]
    fn every_truncation_is_an_error() {
        let frames: Vec<Vec<u8>> = all_client()
            .iter()
            .map(|m| encode(m).unwrap())
            .chain(all_server().iter().map(|m| encode(m).unwrap()))
            .collect();
        for (i, f) in frames.iter().enumerate() {
            for cut in 0..f.len() {
                let r = if i < all_client().len() {
                    decode::<ClientMsg>(&f[..cut]).map(|_| ())
                } else {
                    decode::<ServerMsg>(&f[..cut]).map(|_| ())
                };
                assert!(
                    matches!(r, Err(ProtocolError::Truncated)),
                    "frame {i} cut at {cut}"
                );
            }
        }
    }

    #[test]
    fn truncated_stream_is_an_error_and_clean_eof_is_none() {
        let f = encode(&ClientMsg::Input(vec![1, 2, 3])).unwrap();
        assert!(matches!(
            read_msg::<ClientMsg>(&mut &f[..f.len() - 1]),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            read_msg::<ClientMsg>(&mut &f[..2]),
            Err(ProtocolError::Truncated)
        ));
        assert!(read_msg::<ClientMsg>(&mut &[][..]).unwrap().is_none());
    }

    #[test]
    fn oversized_frame_is_an_error() {
        let len = u32::try_from(MAX_FRAME + 1).unwrap();
        let mut f = len.to_le_bytes().to_vec();
        f.push(1);
        assert!(
            matches!(decode::<ClientMsg>(&f), Err(ProtocolError::Oversized(n)) if n == MAX_FRAME + 1)
        );
        assert!(matches!(
            read_msg::<ClientMsg>(&mut f.as_slice()),
            Err(ProtocolError::Oversized(_))
        ));
        let huge = u32::MAX.to_le_bytes();
        assert!(matches!(
            decode::<ServerMsg>(&huge),
            Err(ProtocolError::Oversized(_))
        ));
    }

    #[test]
    fn oversized_encode_and_write_are_errors_not_panics() {
        let big = ServerMsg::Frame(vec![0; MAX_FRAME]);
        assert!(matches!(
            encode(&big),
            Err(ProtocolError::Oversized(n)) if n == MAX_FRAME + 5
        ));
        let mut sink = Vec::new();
        assert!(matches!(
            write_msg(&mut sink, &big),
            Err(ProtocolError::Oversized(_))
        ));
        assert!(sink.is_empty(), "nothing is written for a refused frame");
        let big = ClientMsg::Stdin(vec![0; MAX_FRAME]);
        assert!(matches!(encode(&big), Err(ProtocolError::Oversized(_))));
    }

    #[test]
    fn write_msg_writes_the_frame() {
        let mut sink = Vec::new();
        write_msg(&mut sink, &ClientMsg::Detach).unwrap();
        assert_eq!(sink, encode(&ClientMsg::Detach).unwrap());
    }

    #[test]
    fn bad_flag_bytes_are_errors() {
        let mut f = encode(&ClientMsg::Hello(hello(None))).unwrap();
        let n = f.len();
        // control, utf8, no_start come before the (empty) ext's length.
        for (at, bad) in [(n - 7, 3), (n - 6, 2), (n - 5, 2)] {
            let saved = f[at];
            f[at] = bad;
            assert!(matches!(
                decode::<ClientMsg>(&f),
                Err(ProtocolError::Malformed(_))
            ));
            f[at] = saved;
        }
    }

    #[test]
    fn a_frame_at_the_limit_is_accepted() {
        // tag + u32 length + data fills the payload exactly.
        let m = ServerMsg::Frame(vec![7; MAX_FRAME - 5]);
        round_trip(&m);
    }

    #[test]
    fn corrupt_payloads_are_errors() {
        let frame = |payload: &[u8]| {
            let mut f = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
            f.extend_from_slice(payload);
            f
        };
        assert!(matches!(
            decode::<ClientMsg>(&frame(&[99])),
            Err(ProtocolError::UnknownTag(99))
        ));
        assert!(matches!(
            decode::<ClientMsg>(&frame(&[])),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            decode::<ClientMsg>(&frame(&[4, 0])),
            Err(ProtocolError::TrailingBytes)
        ));
        // Input with a declared length longer than the payload.
        assert!(matches!(
            decode::<ClientMsg>(&frame(&[1, 9, 0, 0, 0, 1])),
            Err(ProtocolError::Truncated)
        ));
        // Stdout is bytes; Exit's message must be UTF-8.
        let mut bad_utf8 = vec![3, 0, 0, 0, 0, 1, 1, 0, 0, 0, 0xff];
        assert!(matches!(
            decode::<ServerMsg>(&frame(&bad_utf8)),
            Err(ProtocolError::Malformed(_))
        ));
        bad_utf8[5] = 2;
        assert!(matches!(
            decode::<ServerMsg>(&frame(&bad_utf8)),
            Err(ProtocolError::Malformed(_))
        ));
        // A corrupt vector count cannot reserve memory: Hello with argv count 2^32-1.
        let mut hello = vec![0, 1, 0, 0, 0, 1, 0, 0, 0, 0];
        hello.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode::<ClientMsg>(&frame(&hello)),
            Err(ProtocolError::Truncated)
        ));
    }

    /// client.c:659-668 shape with vtmux numbers; only a newer client is
    /// told to move the server onto its binary (an older one would be a
    /// downgrade).
    #[test]
    fn version_mismatch_has_tmux_shape_and_names_the_upgrade_to_a_newer_client() {
        let at = |version| {
            Hello {
                version,
                flags: ClientFlags::default(),
                ..hello(None)
            }
            .check_version()
            .unwrap_err()
            .to_string()
        };
        assert_eq!(
            at(0),
            format!("protocol version mismatch (client 0, server {PROTOCOL_VERSION})")
        );
        let newer = PROTOCOL_VERSION + 1;
        assert_eq!(
            at(newer),
            format!(
                "protocol version mismatch (client {newer}, server {PROTOCOL_VERSION})\n\
                 run 'tmux vtmux-upgrade' to move the server onto this binary"
            )
        );
    }

    /// The same version speaks everything; another version is served only
    /// as a control client from [`FROZEN_SINCE`] on, and then frozen
    /// frames only.
    #[test]
    fn other_versions_are_served_only_as_control_clients_with_frozen_frames() {
        let with = |version, control| Hello {
            version,
            flags: ClientFlags {
                control,
                ..ClientFlags::default()
            },
            ..hello(None)
        };
        for control in [
            ControlMode::None,
            ControlMode::Control,
            ControlMode::ControlControl,
        ] {
            assert_eq!(
                with(PROTOCOL_VERSION, control).check_version().unwrap(),
                Speaks::Full
            );
        }
        for version in (FROZEN_SINCE..PROTOCOL_VERSION).chain([PROTOCOL_VERSION + 1, u32::MAX]) {
            for control in [ControlMode::Control, ControlMode::ControlControl] {
                assert_eq!(
                    with(version, control).check_version().unwrap(),
                    Speaks::Frozen,
                    "{version}"
                );
            }
            assert!(matches!(
                with(version, ControlMode::None).check_version(),
                Err(EdgeError::VersionMismatch { client, .. }) if client == version
            ));
        }
        // Before the frozen frames: one last manual restart (decision 14).
        assert!(
            with(FROZEN_SINCE - 1, ControlMode::Control)
                .check_version()
                .is_err()
        );
    }

    #[test]
    fn exactly_the_frozen_server_frames_are_frozen() {
        for m in all_server() {
            let tag = encode(&m).unwrap()[4];
            assert_eq!(
                m.is_frozen(),
                matches!(tag, 0 | 1 | 3 | 9 | 12 | 0x80 | 0x81),
                "{m:?}"
            );
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut s, b| {
            use fmt::Write as _;
            let _ = write!(s, "{b:02x}");
            s
        })
    }

    /// A golden: hex digits, spaces between fields.
    fn unhex(golden: &str) -> Vec<u8> {
        let digits: Vec<u8> = golden.bytes().filter(|b| *b != b' ').collect();
        digits
            .chunks(2)
            .map(|d| u8::from_str_radix(std::str::from_utf8(d).unwrap(), 16).unwrap())
            .collect()
    }

    /// The frozen client frames and their goldens (module docs): hex, a
    /// space between fields, the frame header first.
    fn frozen_client_goldens() -> Vec<(ClientMsg, &'static str)> {
        vec![
            (
                ClientMsg::Hello(Hello {
                    version: 9,
                    invocation: Invocation::Tmux,
                    pid: 0x0102_0304,
                    argv: vec![b"ls".to_vec(), b"\xff".to_vec()],
                    cwd: PathBuf::from("/"),
                    env: vec![("A".into(), "b".into())],
                    tty: None,
                    flags: ClientFlags {
                        control: ControlMode::Control,
                        utf8: true,
                        no_start: false,
                    },
                }),
                "34000000 00 09000000 00 04030201 02000000 02000000 6c73 01000000 ff 01000000 2f 01000000 01000000 41 01000000 62 00 01 01 00 00000000",
            ),
            (
                ClientMsg::Hello(Hello {
                    version: 9,
                    invocation: Invocation::Vtmux,
                    pid: 7,
                    argv: Vec::new(),
                    cwd: PathBuf::from("/h"),
                    env: Vec::new(),
                    tty: Some(TtyInfo {
                        name: "/dev/pts/1".into(),
                        term: "xterm".into(),
                        cols: 80,
                        rows: 24,
                        features: vec!["RGB".into()],
                    }),
                    flags: ClientFlags {
                        control: ControlMode::ControlControl,
                        utf8: false,
                        no_start: true,
                    },
                }),
                "4a000000 00 09000000 01 07000000 00000000 02000000 2f68 00000000 01 0a000000 2f6465762f7074732f31 05000000 787465726d 50000000 18000000 01000000 03000000 524742 02 00 01 00000000",
            ),
            (
                ClientMsg::Stdin(b"ls\n".to_vec()),
                "08000000 03 03000000 6c730a",
            ),
            (ClientMsg::Stdin(Vec::new()), "05000000 03 00000000"),
            (ClientMsg::StdinEof, "01000000 04"),
            (
                ClientMsg::UpgradeRequest {
                    binary: PathBuf::from("/bin/vtmux"),
                },
                "0f000000 80 0a000000 2f62696e2f76746d7578",
            ),
        ]
    }

    /// The frozen server frames and their goldens.
    fn frozen_server_goldens() -> Vec<(ServerMsg, &'static str)> {
        vec![
            (
                ServerMsg::Stdout(b"hi\n".to_vec()),
                "08000000 00 03000000 68690a",
            ),
            (ServerMsg::Stderr(b"e".to_vec()), "06000000 01 01000000 65"),
            (
                ServerMsg::Exit {
                    code: 1,
                    message: None,
                    hup: false,
                },
                "07000000 03 01000000 00 00",
            ),
            (
                ServerMsg::Exit {
                    code: -1,
                    message: Some("x".into()),
                    hup: true,
                },
                "0c000000 03 ffffffff 01 01000000 78 01",
            ),
            (
                ServerMsg::Exec {
                    command: "ls".into(),
                    shell: PathBuf::from("/bin/sh"),
                },
                "12000000 09 02000000 6c73 07000000 2f62696e2f7368",
            ),
            (ServerMsg::Flags { wait_exit: true }, "02000000 0c 01"),
            (ServerMsg::Flags { wait_exit: false }, "02000000 0c 00"),
            (
                ServerMsg::UpgradeResult {
                    ok: false,
                    message: "not implemented".into(),
                },
                "15000000 80 00 0f000000 6e6f7420696d706c656d656e746564",
            ),
            (
                ServerMsg::UpgradeResult {
                    ok: true,
                    message: String::new(),
                },
                "06000000 80 01 00000000",
            ),
            (
                ServerMsg::UpgradeNotice {
                    binary: PathBuf::from("/b"),
                    token: "t0123456789abcdE".into(),
                },
                "1b000000 81 02000000 2f62 10000000 74303132333435363738396162636445",
            ),
        ]
    }

    /// The frozen frames (module docs), byte for byte. These goldens must
    /// never change: binaries of other versions decode exactly these bytes.
    /// A failure here means a frozen frame changed; put it back and add a
    /// new frame instead.
    #[test]
    fn frozen_frames_never_change() {
        // Every frozen frame has a golden.
        let tags =
            |goldens: Vec<u8>| -> std::collections::BTreeSet<u8> { goldens.into_iter().collect() };
        assert_eq!(
            tags(
                frozen_client_goldens()
                    .iter()
                    .map(|(_, g)| unhex(g)[4])
                    .collect()
            ),
            tags(vec![0, 3, 4, 0x80])
        );
        assert_eq!(
            tags(
                frozen_server_goldens()
                    .iter()
                    .map(|(_, g)| unhex(g)[4])
                    .collect()
            ),
            tags(vec![0, 1, 3, 9, 12, 0x80, 0x81])
        );
        for (m, golden) in &frozen_client_goldens() {
            let bytes = unhex(golden);
            assert_eq!(hex(&encode(m).unwrap()), hex(&bytes), "{m:?}");
            assert_eq!(
                decode::<ClientMsg>(&bytes).unwrap(),
                (m.clone(), bytes.len())
            );
        }
        for (m, golden) in &frozen_server_goldens() {
            let bytes = unhex(golden);
            assert_eq!(hex(&encode(m).unwrap()), hex(&bytes), "{m:?}");
            assert_eq!(
                decode::<ServerMsg>(&bytes).unwrap(),
                (m.clone(), bytes.len())
            );
        }
    }

    /// The first Hello golden with `ext` set to `ext` and `tail` after it.
    fn hello_golden_with(version: u32, invocation: u8, ext: &[u8], tail: &[u8]) -> Vec<u8> {
        let golden = unhex(frozen_client_goldens()[0].1);
        let mut payload = golden[4..golden.len() - 4].to_vec();
        payload[1..5].copy_from_slice(&version.to_le_bytes());
        payload[5] = invocation;
        put_bytes(&mut payload, ext);
        payload.extend_from_slice(tail);
        let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
        frame.extend_from_slice(&payload);
        frame
    }

    /// Hello's extension area (module docs): what a later version puts
    /// there is ignored; bytes after it are still an error.
    #[test]
    fn a_hello_reads_past_its_extension_area() {
        let ClientMsg::Hello(want) = frozen_client_goldens()[0].0.clone() else {
            unreachable!()
        };
        let frame = hello_golden_with(9, 0, b"later fields", b"");
        assert_eq!(
            decode::<ClientMsg>(&frame).unwrap(),
            (ClientMsg::Hello(want), frame.len())
        );
        assert!(matches!(
            decode::<ClientMsg>(&hello_golden_with(9, 0, b"", b"x")),
            Err(ProtocolError::TrailingBytes)
        ));
    }

    /// The version comes first (module docs): it is read from a Hello that
    /// does not decode, and from nothing else.
    #[test]
    fn a_hello_version_is_read_without_decoding_the_rest() {
        let trailing = hello_golden_with(10, 0, b"", b"x");
        assert!(decode::<ClientMsg>(&trailing).is_err());
        assert_eq!(hello_version(&trailing), Some(10));
        let invocation = hello_golden_with(10, 2, b"", b"");
        assert!(matches!(
            decode::<ClientMsg>(&invocation),
            Err(ProtocolError::Malformed(_))
        ));
        assert_eq!(hello_version(&invocation), Some(10));
        assert_eq!(hello_version(&trailing[..9]), Some(10));
        assert_eq!(hello_version(&trailing[..8]), None, "not all there");
        // Tag 0 with less than a version is no Hello version.
        assert_eq!(hello_version(&[1, 0, 0, 0, 0]), None);
        assert_eq!(hello_version(&[4, 0, 0, 0, 0, 10, 0, 0, 0]), None);
        for m in all_client()
            .iter()
            .filter(|m| !matches!(m, ClientMsg::Hello(_)))
        {
            assert_eq!(hello_version(&encode(m).unwrap()), None, "{m:?}");
        }
    }

    /// An `UpgradeNotice`'s token is `[A-Za-z0-9]{16,64}` and its binary an
    /// absolute path (module docs); a client refuses anything else.
    #[test]
    fn an_upgrade_notice_needs_a_safe_token_and_an_absolute_binary() {
        let notice = |binary: &str, token: &str| {
            let mut payload = vec![0x81];
            put_bytes(&mut payload, binary.as_bytes());
            put_bytes(&mut payload, token.as_bytes());
            let mut frame = u32::try_from(payload.len()).unwrap().to_le_bytes().to_vec();
            frame.extend_from_slice(&payload);
            decode::<ServerMsg>(&frame).map(|(m, _)| m)
        };
        let ok = "0123456789abcdefABCDEFxyzXYZ";
        for token in ["a".repeat(16), "Z9".repeat(32), ok.to_owned()] {
            assert!(is_upgrade_token(&token), "{token}");
            assert_eq!(
                notice("/usr/bin/vtmux", &token).unwrap(),
                ServerMsg::UpgradeNotice {
                    binary: PathBuf::from("/usr/bin/vtmux"),
                    token,
                }
            );
        }
        for token in [
            String::new(),
            "a".repeat(15),
            "a".repeat(65),
            format!("-{}", &ok[1..]),
            format!("{}\0", &ok[1..]),
            format!("{} ", &ok[1..]),
            format!("{}\u{e9}", &ok[2..]),
        ] {
            assert!(!is_upgrade_token(&token), "{token:?}");
            assert!(
                matches!(
                    notice("/usr/bin/vtmux", &token),
                    Err(ProtocolError::Malformed(_))
                ),
                "{token:?}"
            );
        }
        for binary in ["vtmux", "bin/vtmux", "-x", ""] {
            assert!(
                matches!(notice(binary, ok), Err(ProtocolError::Malformed(_))),
                "{binary:?}"
            );
        }
    }
}
