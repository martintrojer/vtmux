//! D33: what a graphics command makes the server allocate is bounded by
//! the screen's storage limit (`vtmux-graphics-limit`), not by Ghostty's
//! own 400 MB (`max_size`). An image that decodes past the limit is
//! refused as `addImage` refuses it (`ENOMEM`), without first being
//! decoded: a compressed payload of a few hundred KB inflates to 256 MB,
//! a PNG of as little to 64 MB of grey then 256 MB of RGBA.
//!
//! The peak resident size from `/proc/self/status` (`VmHWM`, reset through
//! `clear_refs`), so Linux only; its own test binary, so no other test's
//! allocations move the number; its tests measure one at a time.

#![cfg(target_os = "linux")]

use std::io::Write;

use vtmux_vt::PaneTerm;

/// The pixels each image has: 8000 x 8000, 256 MB as RGBA.
const SIDE: u32 = 8000;

fn status_kib(key: &str) -> usize {
    let status = std::fs::read_to_string("/proc/self/status").expect("status");
    status
        .lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches(" kB").parse().ok())
        .expect(key)
}

/// The peak resident size from now on starts at the current one.
fn reset_peak() {
    std::fs::write("/proc/self/clear_refs", "5").expect("clear_refs");
}

fn base64(raw: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(raw.len().div_ceil(3) * 4);
    for c in raw.chunks(3) {
        let n = c
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, &b)| n | u32::from(b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= c.len() {
                char::from(ABC[(n >> (18 - 6 * i)) as usize & 63])
            } else {
                '='
            });
        }
    }
    out
}

/// `rows` rows of `row` zero bytes, zlib-compressed, written a row at a
/// time so the test never holds them.
fn zlib_zeros(row: usize, rows: u32) -> Vec<u8> {
    let zeros = vec![0u8; row];
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
    for _ in 0..rows {
        z.write_all(&zeros).unwrap();
    }
    z.finish().unwrap()
}

fn grey_png() -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(std::io::Cursor::new(&mut out), SIDE, SIDE);
    e.set_color(png::ColorType::Grayscale);
    e.set_depth(png::BitDepth::Eight);
    e.set_compression(png::Compression::High);
    let mut w = e.write_header().unwrap();
    let mut s = w.stream_writer().unwrap();
    let row = vec![0u8; SIDE as usize];
    for _ in 0..SIDE {
        s.write_all(&row).unwrap();
    }
    s.finish().unwrap();
    w.finish().unwrap();
    out
}

/// A `side` x `side` PNG of colour `ct`, its chunks before the image
/// data `extra`, and its image data `idat` (all zeros if `None`).
fn png_with(
    side: u32,
    ct: png::ColorType,
    extra: &[(png::chunk::ChunkType, Vec<u8>)],
    idat: Option<&[u8]>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut e = png::Encoder::new(std::io::Cursor::new(&mut out), side, side);
    e.set_color(ct);
    e.set_depth(png::BitDepth::Eight);
    let mut w = e.write_header().unwrap();
    for (kind, data) in extra {
        w.write_chunk(*kind, data).unwrap();
    }
    if let Some(idat) = idat {
        w.write_chunk(png::chunk::IDAT, idat).unwrap();
        drop(w);
        return out;
    }
    let mut s = w.stream_writer().unwrap();
    let row = vec![0u8; side as usize * ct.samples()];
    for _ in 0..side {
        s.write_all(&row).unwrap();
    }
    s.finish().unwrap();
    w.finish().unwrap();
    out
}

/// What feeding `command` to a granted screen with a 1 MB limit raised
/// the peak resident size by, in KiB, and the answer.
fn peak_cost(command: &[u8]) -> (usize, Vec<u8>) {
    peak_cost_at(command, 1_000_000)
}

/// [`peak_cost`] at a limit of `limit` bytes.
fn peak_cost_at(command: &[u8], limit: u64) -> (usize, Vec<u8>) {
    // The peak is the process's: one measurement at a time.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut t = PaneTerm::new(80, 24, 100);
    t.set_graphics_share(limit);
    t.set_graphics(true);
    reset_peak();
    let before = status_kib("VmRSS:");
    t.feed(command);
    let cost = status_kib("VmHWM:").saturating_sub(before);
    (cost, t.take_replies())
}

#[test]
fn an_image_past_the_limit_is_refused_before_it_is_decoded() {
    let zlib = base64(&zlib_zeros(SIDE as usize * 4, SIDE));
    let (kib, reply) =
        peak_cost(format!("\x1b_Ga=t,f=32,o=z,s={SIDE},v={SIDE},i=1;{zlib}\x1b\\").as_bytes());
    assert_eq!(reply, b"\x1b_Gi=1;ENOMEM: out of memory\x1b\\");
    assert!(kib < 64 * 1024, "zlib: the peak rose {kib} KiB");

    let png = base64(&grey_png());
    let (kib, reply) = peak_cost(format!("\x1b_Ga=t,f=100,i=2;{png}\x1b\\").as_bytes());
    assert_eq!(reply, b"\x1b_Gi=2;ENOMEM: out of memory\x1b\\");
    assert!(kib < 64 * 1024, "PNG: the peak rose {kib} KiB");
}

/// A query (`a=q`) attempts the load a transmit would (`graphics_exec.zig`
/// `query`), held to the same limit: a PNG of a few hundred bytes whose
/// header names 9000 x 9000 pixels (324 MB as RGBA) and a query of zlib
/// data inflating to 256 MB are refused at a 100 KB limit as a transmit
/// is, without allocating that.
#[test]
fn a_query_is_held_to_the_limit() {
    let png = png_with(
        9000,
        png::ColorType::Rgba,
        &[],
        Some(&[0x78, 0x9c, 0x03, 0x00]),
    );
    assert!(png.len() < 400, "{}", png.len());
    let png = base64(&png);
    let (kib, reply) = peak_cost_at(
        format!("\x1b_Ga=q,f=100,i=3;{png}\x1b\\").as_bytes(),
        100_000,
    );
    assert_eq!(reply, b"\x1b_Gi=3;ENOMEM: out of memory\x1b\\");
    assert!(kib < 16 * 1024, "PNG query: the peak rose {kib} KiB");

    let zlib = base64(&zlib_zeros(SIDE as usize * 4, SIDE));
    let (kib, reply) = peak_cost_at(
        format!("\x1b_Ga=q,f=32,o=z,s={SIDE},v={SIDE},i=4;{zlib}\x1b\\").as_bytes(),
        1_000_000,
    );
    assert_eq!(reply, b"\x1b_Gi=4;ENOMEM: out of memory\x1b\\");
    assert!(kib < 16 * 1024, "zlib query: the peak rose {kib} KiB");
}

/// A PNG's ancillary chunks are not the image: an ICC profile (`iCCP`) of
/// 35 KB inflating to 36 MB is not inflated, and the image loads at a
/// 1 MB limit.
#[test]
fn a_pngs_profile_is_not_inflated() {
    let mut iccp = b"icc\0\0".to_vec();
    iccp.extend(zlib_zeros(36_000, 1000));
    assert!(iccp.len() < 40_000, "{}", iccp.len());
    let png = base64(&png_with(
        2,
        png::ColorType::Rgb,
        &[(png::chunk::iCCP, iccp)],
        None,
    ));
    let (kib, reply) = peak_cost(format!("\x1b_Ga=t,f=100,i=5;{png}\x1b\\").as_bytes());
    assert_eq!(reply, b"\x1b_Gi=5;OK\x1b\\");
    assert!(kib < 8 * 1024, "iCCP: the peak rose {kib} KiB");
}

/// A PNG under the limit decodes straight into the image's buffer, held
/// by the limit: a 3000 x 3000 RGBA PNG (36 MB) at a 40 MB limit raises
/// the peak by not much more than the image.
#[test]
fn a_png_decodes_into_one_buffer() {
    const RGBA: usize = 3000 * 3000 * 4;
    let png = base64(&png_with(3000, png::ColorType::Rgba, &[], None));
    let (kib, reply) = peak_cost_at(
        format!("\x1b_Ga=t,f=100,i=6;{png}\x1b\\").as_bytes(),
        40_000_000,
    );
    assert_eq!(reply, b"\x1b_Gi=6;OK\x1b\\");
    assert!(
        kib < RGBA / 1024 * 5 / 4,
        "the peak rose {kib} KiB for {} KiB of image",
        RGBA / 1024
    );
}
