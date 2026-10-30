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
//! own numbers; see [`Hello::check_version`].

use std::fmt;
use std::io::{self, Read, Write};
use std::path::PathBuf;

use crate::error::EdgeError;
use crate::socket::Invocation;

/// Bumped on any incompatible wire change.
pub const PROTOCOL_VERSION: u32 = 2;

/// Largest payload accepted (16 MiB). Frames of terminal output are far
/// smaller; this only bounds what a corrupt length can make us allocate.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

/// The client's terminal, when it has one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TtyInfo {
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
    pub argv: Vec<String>,
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

impl Hello {
    /// `Err` if the client speaks a different protocol version than this
    /// build.
    pub fn check_version(&self) -> Result<(), EdgeError> {
        if self.version == PROTOCOL_VERSION {
            Ok(())
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
    },
    Ready,
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
        4 + self.term.len() + 8 + strings_len(&self.features)
    }

    fn write(&self, out: &mut Vec<u8>) {
        put_bytes(out, self.term.as_bytes());
        put_u32(out, self.cols);
        put_u32(out, self.rows);
        put_strings(out, &self.features);
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        Ok(Self {
            term: r.string()?,
            cols: r.u32()?,
            rows: r.u32()?,
            features: r.strings()?,
        })
    }
}

impl Hello {
    fn write(&self, out: &mut Vec<u8>) {
        use std::os::unix::ffi::OsStrExt;
        put_u32(out, self.version);
        out.push(match self.invocation {
            Invocation::Tmux => 0,
            Invocation::Vtmux => 1,
        });
        put_strings(out, &self.argv);
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
    }

    fn len(&self) -> usize {
        4 + 1
            + strings_len(&self.argv)
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
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, ProtocolError> {
        use std::os::unix::ffi::OsStringExt;
        let version = r.u32()?;
        let invocation = match r.u8()? {
            0 => Invocation::Tmux,
            1 => Invocation::Vtmux,
            _ => return Err(ProtocolError::Malformed("bad invocation byte")),
        };
        let argv = r.strings()?;
        let cwd = PathBuf::from(std::ffi::OsString::from_vec(r.bytes()?));
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
        Ok(Self {
            version,
            invocation,
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
            Self::Input(b) | Self::Stdin(b) => 4 + b.len(),
            Self::Resize { .. } | Self::FrameAck { .. } => 8,
            Self::StdinEof | Self::Detach => 0,
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
            t => return Err(ProtocolError::UnknownTag(t)),
        })
    }
}

impl Message for ServerMsg {
    fn payload_len(&self) -> usize {
        1 + match self {
            Self::Stdout(b) | Self::Stderr(b) | Self::Frame(b) => 4 + b.len(),
            Self::Exit { message, .. } => 4 + option_len(message.as_ref(), |m| 4 + m.len()),
            Self::Ready => 0,
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
            Self::Exit { code, message } => {
                out.push(3);
                out.extend_from_slice(&code.to_le_bytes());
                put_option(out, message.as_ref(), |o, m| put_bytes(o, m.as_bytes()));
            }
            Self::Ready => out.push(4),
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
            },
            4 => Self::Ready,
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
            argv: vec!["new-session".into(), "-d".into(), "-s".into(), "ä".into()],
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
            },
            ServerMsg::Exit {
                code: -1,
                message: Some("exited".into()),
            },
            ServerMsg::Ready,
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
        // control, utf8, no_start are the last three payload bytes.
        for (at, bad) in [(n - 3, 3), (n - 2, 2), (n - 1, 2)] {
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
        let mut hello = vec![0, 1, 0, 0, 0, 1];
        hello.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(matches!(
            decode::<ClientMsg>(&frame(&hello)),
            Err(ProtocolError::Truncated)
        ));
    }

    #[test]
    fn version_mismatch_has_tmux_shape() {
        assert!(hello(None).check_version().is_ok());
        let old = Hello {
            version: 0,
            ..hello(None)
        };
        let err = old.check_version().unwrap_err();
        // client.c:659-668 shape, vtmux numbers.
        assert_eq!(
            err.to_string(),
            "protocol version mismatch (client 0, server 2)"
        );
    }
}
