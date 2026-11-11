//! D33 (spec, *Protocol extensions (D28)*): a recording client, an attach
//! test keeping a Ghostty client's bytes, checks the escapes it is sent:
//! the range freed at attach, the upload before the frame that shows an
//! image, the virtual placement, placeholder cells with vtmux's ids and
//! the right row and column diacritics, and the free when no frame shows
//! the image any more.

use super::{attach_test_client, frames, share_server};
use crate::render::graphics::{FIRST, LAST};
use crate::server::snapshot::tests::ok;
use vtmux_vt::graphics::{DIACRITICS, PLACEHOLDER};

const XTVERSION: &[u8] = b"\x1bP>|ghostty 1.2.3\x1b\\";

/// A 2x2 RGB image `id`, sent and placed over `c` x `r` cells (`a=T`).
fn image(id: u32, c: u32, r: u32) -> Vec<u8> {
    // Twelve bytes of 7, base64.
    format!("\x1b_Ga=T,f=24,s=2,v=2,i={id},c={c},r={r},q=2;BwcHBwcHBwcHBwcH\x1b\\").into_bytes()
}

fn has(sent: &[u8], what: &[u8]) -> bool {
    find(sent, what).is_some()
}

fn find(sent: &[u8], what: &[u8]) -> Option<usize> {
    sent.windows(what.len()).position(|w| w == what)
}

/// The placeholder text of cell (`row`, `col`) of vtmux's image `id`.
fn cell(id: u32, row: usize, col: usize) -> Vec<u8> {
    let mut s = String::new();
    s.push(PLACEHOLDER);
    s.push(DIACRITICS[row]);
    s.push(DIACRITICS[col]);
    s.push(DIACRITICS[usize::try_from(id >> 24).unwrap()]);
    s.into_bytes()
}

/// A Ghostty client gets the range freed, the upload, the virtual
/// placement, then the frame with placeholder cells; a client of another
/// terminal none of it. Away in another window the image is freed, and it
/// is uploaded again on the way back.
#[test]
fn a_ghostty_client_is_sent_uploads_placements_and_placeholder_cells() {
    let (mut server, p, s, dir) = share_server("gfx-upload");
    let (ghostty, mut gpeer) = attach_test_client(&mut server, s, &dir);
    server.on_client_input(ghostty, XTVERSION).unwrap();
    let (other, mut opeer) = attach_test_client(&mut server, s, &dir);
    ok(&mut server, &dir, &["set", "-s", "vtmux-graphics", "on"]);
    server.render_clients();
    frames(&mut server, ghostty, &mut gpeer);
    frames(&mut server, other, &mut opeer);
    server.pane_input(p, b"ab\r\n", false);
    server.pane_input(p, &image(7, 3, 2), false);
    server.render_clients();
    let sent = frames(&mut server, ghostty, &mut gpeer);
    let id = FIRST;
    let range = format!("\x1b_Ga=d,d=R,x={FIRST},y={LAST},q=2\x1b\\");
    let upload = format!("\x1b_Ga=t,f=24,s=2,v=2,o=z,i={id},q=2");
    let place = format!("\x1b_Ga=p,U=1,i={id},p={},c=3,r=2,q=2\x1b\\", 0x00F8_0001);
    let at = |what: &[u8]| {
        find(&sent, what).unwrap_or_else(|| {
            panic!(
                "{:?} in {:?}",
                String::from_utf8_lossy(what),
                String::from_utf8_lossy(&sent)
            )
        })
    };
    assert!(at(range.as_bytes()) < at(upload.as_bytes()));
    assert!(at(upload.as_bytes()) < at(place.as_bytes()));
    // The cells, after the placement: row 0 and 1 of the image, at the
    // cursor's row (1) and column (0).
    assert!(at(place.as_bytes()) < at(&cell(id, 0, 0)));
    for (r, c) in [(0, 0), (0, 1), (0, 2), (1, 0), (1, 2)] {
        assert!(has(&sent, &cell(id, r, c)), "cell {r},{c}");
    }
    // The image id's low 24 bits are the foreground (0xEE000000: 0,0,0);
    // the placement id the underline colour.
    assert!(has(&sent, b"\x1b[38;2;0;0;0m"));
    assert!(has(&sent, b"\x1b[58:2:248:0:1m"));
    let plain = frames(&mut server, other, &mut opeer);
    assert!(!has(&plain, b"\x1b_G"), "another terminal: no uploads");
    assert!(
        !has(&plain, "\u{10EEEE}".as_bytes()),
        "nor placeholder cells"
    );
    // Nothing new to send while it shows.
    server.pane_input(p, b"x", false);
    server.render_clients();
    let again = frames(&mut server, ghostty, &mut gpeer);
    assert!(
        !has(&again, b"\x1b_G"),
        "{:?}",
        String::from_utf8_lossy(&again)
    );
    // Another window: freed. Back: uploaded again.
    ok(&mut server, &dir, &["new-window", "-t", "a", "cat", "-"]);
    server.render_clients();
    let away = frames(&mut server, ghostty, &mut gpeer);
    assert!(has(
        &away,
        format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").as_bytes()
    ));
    ok(&mut server, &dir, &["select-window", "-t", "a:0"]);
    server.render_clients();
    let back = frames(&mut server, ghostty, &mut gpeer);
    assert!(has(&back, upload.as_bytes()) && has(&back, place.as_bytes()));
    assert!(!has(&back, range.as_bytes()), "the range only at attach");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A ready Ghostty client on a fresh server with `vtmux-graphics` on, its
/// first frames read.
fn ghostty_server(
    name: &str,
) -> (
    crate::server::Server,
    vtmux_core::ids::PaneId,
    (crate::server::ClientId, mio::net::UnixStream),
    std::path::PathBuf,
) {
    let (mut server, p, s, dir) = share_server(name);
    let (id, mut peer) = attach_test_client(&mut server, s, &dir);
    server.on_client_input(id, XTVERSION).unwrap();
    ok(&mut server, &dir, &["set", "-s", "vtmux-graphics", "on"]);
    server.render_clients();
    frames(&mut server, id, &mut peer);
    (server, p, (id, peer), dir)
}

/// The image's cells are pane cells: one wider than its pane is clipped at
/// the pane's edge, a zoomed pane shows it whole, and a popup over it
/// hides its cells (and the image, freed, once none shows).
#[test]
fn placeholder_cells_clip_zoom_and_hide_under_a_popup() {
    let (mut server, p, (id, mut peer), dir) = ghostty_server("gfx-clip");
    ok(
        &mut server,
        &dir,
        &[
            "split-window",
            "-h",
            "-d",
            "-t",
            "%0",
            "-l",
            "20",
            "cat",
            "-",
        ],
    );
    server.render_clients();
    frames(&mut server, id, &mut peer);
    // %0 is 19 wide; the image 30 cells.
    server.pane_input(p, &image(1, 30, 1), false);
    server.render_clients();
    let sent = frames(&mut server, id, &mut peer);
    assert!(has(&sent, &cell(FIRST, 0, 18)), "inside the pane");
    assert!(!has(&sent, &cell(FIRST, 0, 19)), "clipped at its edge");
    ok(&mut server, &dir, &["resize-pane", "-Z", "-t", "%0"]);
    server.render_clients();
    let zoomed = frames(&mut server, id, &mut peer);
    assert!(has(&zoomed, &cell(FIRST, 0, 29)), "zoomed: all of it");
    // A popup over the whole client.
    let out = crate::server::snapshot::tests::run(
        &mut server,
        &dir,
        &[
            "display-popup",
            "-c",
            "/dev/null",
            "-B",
            "-x",
            "0",
            "-y",
            "0",
            "-w",
            "40",
            "-h",
            "10",
            "cat",
            "-",
        ],
    );
    assert!(server.clients[&id].popup.is_some(), "{out}");
    server.render_clients();
    let under = frames(&mut server, id, &mut peer);
    assert!(!has(&under, "\u{10EEEE}".as_bytes()));
    assert!(
        has(
            &under,
            format!("\x1b_Ga=d,d=I,i={FIRST},q=2\x1b\\").as_bytes()
        ),
        "none shows: freed"
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Placeholder cells a program prints naming one of vtmux's ids (they
/// are handed out in order, so easy to guess) show no image: not in a
/// pane holding none, nor in another window or session than the image's.
/// The client is sent no upload and the cells blanked.
#[test]
fn placeholder_cells_naming_vtmuxs_ids_show_no_other_panes_image() {
    let (mut server, p, (id, mut peer), dir) = ghostty_server("gfx-forge");
    server.pane_input(p, &image(1, 2, 1), false);
    server.render_clients();
    let upload = format!("\x1b_Ga=t,f=24,s=2,v=2,o=z,i={FIRST},q=2");
    assert!(has(&frames(&mut server, id, &mut peer), upload.as_bytes()));
    ok(&mut server, &dir, &["new-window", "-t", "a", "cat", "-"]);
    let q = *server.model.panes.keys().find(|&&k| k != p).unwrap();
    let forged = [
        b"\x1b[38;2;0;0;0m".as_slice(),
        &cell(FIRST, 0, 0),
        &cell(FIRST, 0, 1),
        b"\x1b[m",
    ]
    .concat();
    let check = |sent: &[u8], what: &str| {
        let text = String::from_utf8_lossy(sent);
        assert!(!has(sent, b"\x1b_Ga=t"), "{what}: an upload in {text:?}");
        assert!(
            !has(sent, "\u{10EEEE}".as_bytes()),
            "{what}: cells in {text:?}"
        );
    };
    server.pane_input(q, &forged, false);
    server.render_clients();
    check(&frames(&mut server, id, &mut peer), "another window");
    // Another session's client.
    ok(
        &mut server,
        &dir,
        &["new-session", "-d", "-s", "b", "cat", "-"],
    );
    let b = server
        .model
        .sessions
        .iter()
        .find(|(_, s)| s.name == "b")
        .map(|(&s, _)| s)
        .unwrap();
    let (other, mut opeer) = attach_test_client(&mut server, b, &dir);
    server.on_client_input(other, XTVERSION).unwrap();
    server.render_clients();
    frames(&mut server, other, &mut opeer);
    let r = *server
        .model
        .panes
        .keys()
        .find(|&&k| k != p && k != q)
        .unwrap();
    server.pane_input(r, &forged, false);
    server.render_clients();
    check(&frames(&mut server, other, &mut opeer), "another session");
    std::fs::remove_dir_all(dir).unwrap();
}

/// A program's own placeholder cells (a virtual placement, `U=1`) go out
/// with vtmux's id, every diacritic written; its virtual placement is
/// uploaded as it made it. A client of another terminal gets the cells as
/// the program wrote them, as tmux does.
#[test]
fn a_programs_placeholder_cells_carry_vtmuxs_ids() {
    let (mut server, p, (id, mut peer), dir) = ghostty_server("gfx-virtual");
    let s = server.clients[&id].attached.unwrap();
    let (other, mut opeer) = attach_test_client(&mut server, s, &dir);
    server.render_clients();
    frames(&mut server, id, &mut peer);
    frames(&mut server, other, &mut opeer);
    server.pane_input(
        p,
        b"\x1b_Ga=T,U=1,f=24,s=2,v=2,i=5,c=2,r=1,q=2;BwcHBwcHBwcHBwcH\x1b\\",
        false,
    );
    let mut text = String::from("\x1b[38;5;5m");
    text.push(PLACEHOLDER);
    text.push(DIACRITICS[0]);
    text.push(DIACRITICS[0]);
    text.push(PLACEHOLDER);
    text.push_str("\x1b[m");
    server.pane_input(p, text.as_bytes(), false);
    server.render_clients();
    let sent = frames(&mut server, id, &mut peer);
    let place = format!("\x1b_Ga=p,U=1,i={FIRST},c=2,r=1,q=2\x1b\\");
    assert!(
        has(&sent, place.as_bytes()),
        "{:?}",
        String::from_utf8_lossy(&sent)
    );
    assert!(has(&sent, &cell(FIRST, 0, 0)) && has(&sent, &cell(FIRST, 0, 1)));
    let plain = frames(&mut server, other, &mut opeer);
    assert!(!has(&plain, b"\x1b_G"));
    let mut theirs = String::from(PLACEHOLDER);
    theirs.push(DIACRITICS[0]);
    theirs.push(DIACRITICS[0]);
    theirs.push(PLACEHOLDER);
    assert!(
        has(&plain, theirs.as_bytes()),
        "as written: {:?}",
        String::from_utf8_lossy(&plain)
    );
    std::fs::remove_dir_all(dir).unwrap();
}

/// Copy mode shows an image where its text went: scrolled into history
/// and back on screen in copy mode, with the copy's own placement.
#[test]
fn copy_mode_shows_an_image_in_history() {
    let (mut server, p, (id, mut peer), dir) = ghostty_server("gfx-copy");
    server.pane_input(p, &image(1, 2, 1), false);
    // Twelve lines: the image's row is in history.
    server.pane_input(
        p,
        b"\r\n1\r\n2\r\n3\r\n4\r\n5\r\n6\r\n7\r\n8\r\n9\r\n10\r\n11\r\n12",
        false,
    );
    server.render_clients();
    let sent = frames(&mut server, id, &mut peer);
    assert!(!has(&sent, "\u{10EEEE}".as_bytes()), "scrolled off");
    ok(&mut server, &dir, &["copy-mode", "-t", "%0"]);
    ok(
        &mut server,
        &dir,
        &["send-keys", "-X", "-t", "%0", "history-top"],
    );
    server.render_clients();
    let copy = frames(&mut server, id, &mut peer);
    assert!(
        has(&copy, &cell(FIRST, 0, 0)) && has(&copy, &cell(FIRST, 0, 1)),
        "{:?}",
        String::from_utf8_lossy(&copy)
    );
    assert!(has(&copy, b"\x1b_Ga=p,U=1,"), "its own placement");
    std::fs::remove_dir_all(dir).unwrap();
}

/// No uploads, placements or placeholder cells while the option has never
/// granted a pane, so off costs nothing: the frames are those of a server
/// without D33.
#[test]
fn off_a_ghostty_client_gets_nothing_of_graphics() {
    let (mut server, p, s, dir) = share_server("gfx-off");
    let (id, mut peer) = attach_test_client(&mut server, s, &dir);
    server.on_client_input(id, XTVERSION).unwrap();
    server.render_clients();
    frames(&mut server, id, &mut peer);
    server.pane_input(p, &image(1, 2, 1), false);
    server.render_clients();
    let sent = frames(&mut server, id, &mut peer);
    assert!(!has(&sent, b"\x1b_G") && !has(&sent, "\u{10EEEE}".as_bytes()));
    assert!(!server.model.graphics_used);
    std::fs::remove_dir_all(dir).unwrap();
}

/// Two Ghostty clients of one cell size share the drawing and frames of
/// a window with an image (the ids are the same for both); one with
/// another cell size shares none, and gets the same upload and placement,
/// which its terminal fits to the same cells.
#[test]
#[expect(
    clippy::many_single_char_names,
    reason = "three clients, a pane, a session"
)]
fn frames_are_shared_only_within_one_cell_size() {
    let (mut server, p, s, dir) = share_server("gfx-share");
    // What a shared drawing needs (the render::share tests): nothing in
    // it that reads the client.
    ok(&mut server, &dir, &["set", "-g", "status-right", "x"]);
    ok(&mut server, &dir, &["set", "-g", "window-size", "manual"]);
    ok(&mut server, &dir, &["select-pane", "-T", "t"]);
    ok(&mut server, &dir, &["set", "-s", "vtmux-graphics", "on"]);
    let (a, mut apeer) = attach_test_client(&mut server, s, &dir);
    let (b, mut bpeer) = attach_test_client(&mut server, s, &dir);
    let (c, mut cpeer) = attach_test_client(&mut server, s, &dir);
    for id in [a, b, c] {
        server.on_client_input(id, XTVERSION).unwrap();
    }
    server.clients.get_mut(&c).unwrap().cell_pixels = (7, 15);
    server.render_clients();
    for (id, peer) in [(a, &mut apeer), (b, &mut bpeer), (c, &mut cpeer)] {
        frames(&mut server, id, peer);
    }
    let before: Vec<_> = [a, b, c]
        .iter()
        .map(|id| server.clients[id].window_cache.shared.get())
        .collect();
    server.pane_input(p, &image(1, 2, 1), false);
    server.render_clients();
    let sent: Vec<Vec<u8>> = [(a, &mut apeer), (b, &mut bpeer), (c, &mut cpeer)]
        .into_iter()
        .map(|(id, peer)| frames(&mut server, id, peer))
        .collect();
    let after: Vec<_> = [a, b, c]
        .iter()
        .map(|id| server.clients[id].window_cache.shared.get())
        .collect();
    let upload = format!("\x1b_Ga=t,f=24,s=2,v=2,o=z,i={FIRST},q=2");
    let place = format!(
        "\x1b_Ga=p,U=1,i={FIRST},p={},c=2,r=1,q=2\x1b\\",
        0x00F8_0001
    );
    for bytes in &sent {
        assert!(has(bytes, upload.as_bytes()) && has(bytes, place.as_bytes()));
    }
    assert!(after[1].0 > before[1].0, "b took a's drawing");
    assert_eq!(after[2], before[2], "c, another cell size, drew its own");
    std::fs::remove_dir_all(dir).unwrap();
}
