//! A pane program's terminal queries that tmux answers from outside the
//! pane (input.c `input_osc_4`, `_10`, `_11`, `_12`, `_52`,
//! `input_report_current_theme`, `input_add_request`, `input_request_reply`),
//! and the colours and theme the client terminals report (tty-keys.c
//! `tty_keys_colours`, `tty_keys_palette`, `tty_keys_clipboard`,
//! server-client.c `server_client_report_theme`).
//!
//! A query only a client terminal can answer (a palette entry nobody set,
//! the clipboard under `get-clipboard request`) goes to the most recently
//! active client showing the pane's window and waits for its reply. While
//! one waits, the pane's other replies queue behind it, in order; a
//! request unanswered in [`REQUEST_TIMEOUT`] is dropped and the replies
//! behind it go out.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use vtmux_core::colour::{Colour, Theme};
use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::options::{ArrayKey, OptionValue, TreeId, Value};
use vtmux_core::protocol::ServerMsg;
use vtmux_core::style::{CellStyle, Style};
use vtmux_vt::{OscEnd, Reply, Rgb, colour_reply};

use crate::model::Model;
use crate::server::{ClientId, Server};

/// `INPUT_REQUEST_TIMEOUT` (input.c:75).
pub const REQUEST_TIMEOUT: Duration = Duration::from_millis(500);

/// What a client terminal reported about itself.
#[derive(Clone, Copy, Debug, Default)]
pub struct Reported {
    /// `tty->fg`, `tty->bg`: the OSC 10 and 11 replies.
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    /// `c->theme`: the last `CSI ? 997` report.
    pub theme: Option<Theme>,
    /// Its OSC 4 replies for palette entries 0-15, which the pane blend
    /// (D26) mixes from.
    pub palette: [Option<Rgb>; BLEND_PALETTE as usize],
    /// The entries [`colour_queries`] asked for that are not answered yet
    /// (bit `n` for entry `n`): their replies are vtmux's own and answer
    /// no pane's request.
    palette_out: u16,
}

impl Reported {
    /// What a snapshot carries (D27): no palette query is out.
    #[must_use]
    pub fn restored(
        fg: Option<Rgb>,
        bg: Option<Rgb>,
        theme: Option<Theme>,
        palette: [Option<Rgb>; BLEND_PALETTE as usize],
    ) -> Self {
        Self {
            fg,
            bg,
            theme,
            palette,
            palette_out: 0,
        }
    }

    /// `server_client_update_theme_colours` (server-client.c:1274-1310):
    /// the theme the colours come from, `c->theme`, else the background's
    /// (`colour_totheme(c->tty.bg)`); dark while neither is known.
    #[must_use]
    pub fn light(&self) -> bool {
        let theme = self
            .theme
            .or_else(|| self.bg.and_then(|(r, g, b)| Colour::Rgb(r, g, b).theme()));
        theme == Some(Theme::Light)
    }

    /// The colours the pane blend (D26) mixes from.
    #[must_use]
    pub fn blend_colours(&self) -> crate::render::blend::TermColours {
        crate::render::blend::TermColours {
            fg: self.fg,
            bg: self.bg,
            palette: self.palette,
        }
    }

    /// [`colour_queries`] went to the terminal.
    pub fn palette_sent(&mut self) {
        // One bit per entry: BLEND_PALETTE is 16.
        self.palette_out = u16::MAX;
    }
}

/// One entry of a pane's `ictx->requests`.
#[derive(Debug)]
enum Request {
    /// `INPUT_REQUEST_PALETTE`: an OSC 4 query sent to `client`.
    Palette {
        client: ClientId,
        index: u8,
        end: OscEnd,
    },
    /// `INPUT_REQUEST_CLIPBOARD`: an OSC 52 query sent to `client`.
    Clipboard { client: ClientId, end: OscEnd },
    /// `INPUT_REQUEST_QUEUE`: reply bytes waiting behind a request.
    Queued(Vec<u8>),
}

impl Request {
    fn client(&self) -> Option<ClientId> {
        match self {
            Self::Palette { client, .. } | Self::Clipboard { client, .. } => Some(*client),
            Self::Queued(_) => None,
        }
    }
}

/// A pane's outstanding requests (`ictx->requests`, `ictx->request_timer`)
/// and its theme report state (`wp->last_theme`, `PANE_THEMECHANGED`).
#[derive(Debug, Default)]
pub struct PaneRequests {
    requests: VecDeque<(Instant, Request)>,
    /// `wp->last_theme`; `None` is `THEME_UNKNOWN`.
    last_theme: Option<Theme>,
    /// `PANE_THEMECHANGED`.
    theme_changed: bool,
}

impl PaneRequests {
    /// When the oldest request times out.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.requests.front().map(|(t, _)| *t + REQUEST_TIMEOUT)
    }

    /// `input_free`: a new program starts with none (respawn).
    pub fn clear(&mut self) {
        self.requests.clear();
    }

    /// `PANE_THEMECHANGED` is set (`session_theme_changed`, a new window
    /// style, a pane moved).
    pub fn theme_changed(&mut self) {
        self.theme_changed = true;
    }
}

/// The pane's default colours as `tty_default_colours` works them out
/// (tty.c:3132-3185): the colours OSC 10 and 11 set with `window-style`
/// added (`style_add`), and for the active pane with `window-active-style`
/// added instead, used per colour where that is not the default; `ctx`
/// expands the styles (`tty_style_changed`'s pane format tree).
pub(crate) fn default_colours(
    model: &Model,
    p: PaneId,
    ctx: &dyn vtmux_core::format::FormatContext,
) -> (Colour, Colour) {
    let Some(pane) = model.panes.get(&p) else {
        return (Colour::Default, Colour::Default);
    };
    let (fg, bg) = pane.term.default_colours();
    let rgb = |c: Option<Rgb>| c.map_or(Colour::Default, |(r, g, b)| Colour::Rgb(r, g, b));
    let style = |name: &str| {
        let mut cell = (rgb(fg), rgb(bg));
        let text = option_text(model, TreeId::Pane(p), name);
        let text = if text.contains("#{") {
            vtmux_core::format::expand_str_nojobs(&text, ctx)
        } else {
            text
        };
        // An invalid style adds nothing (options_string_to_style is NULL).
        let mut sy = Style::new(&CellStyle::default());
        if sy.parse(&CellStyle::default(), &text).is_ok() {
            if sy.cell.fg != Colour::Default {
                cell.0 = sy.cell.fg;
            }
            if sy.cell.bg != Colour::Default {
                cell.1 = sy.cell.bg;
            }
        }
        cell
    };
    let normal = style("window-style");
    if model
        .windows
        .get(&pane.window)
        .is_none_or(|w| w.active() != p)
    {
        return normal;
    }
    // tty_default_colours: the active pane's colours are the palette's
    // with window-active-style added (cached_active_gc), each used where
    // it is not 8; so a colour OSC 10 or 11 set beats window-style there.
    let on = style("window-active-style");
    let pick = |a: Colour, n: Colour| if a == Colour::Default { n } else { a };
    (pick(on.0, normal.0), pick(on.1, normal.1))
}

fn option_text(model: &Model, tree: TreeId, name: &str) -> String {
    model
        .options
        .get(tree, name)
        .map(|e| e.value.to_text(e.def, None, false))
        .unwrap_or_default()
}

/// `COLOUR_DEFAULT`: 8 or 9.
fn is_default(c: Colour) -> bool {
    matches!(c, Colour::Default | Colour::Terminal)
}

impl Server {
    /// The attached terminal clients whose session has window `w`, in
    /// client order (`CLIENT_UNATTACHEDFLAGS` and `session_has`).
    fn window_clients(&self, w: WindowId) -> Vec<ClientId> {
        self.clients
            .iter()
            .filter(|(_, c)| c.control.is_none() && !c.suspended && !c.closing && !c.dead)
            .filter(|(_, c)| {
                c.attached
                    .and_then(|s| self.model.sessions.get(&s))
                    .is_some_and(|s| s.windows.values().any(|x| *x == w))
            })
            .map(|(&id, _)| id)
            .collect()
    }

    /// `window_pane_get_bg` / `window_pane_get_fg`: the window style's
    /// colour, else the first client's reported one.
    fn pane_default(&self, p: PaneId, fg: bool) -> Option<Rgb> {
        let ctx = self.format_ctx(crate::target::Found::from_pane(&self.model, p), None);
        let (dfg, dbg) = default_colours(&self.model, p, &ctx);
        let c = if fg { dfg } else { dbg };
        if !is_default(c) {
            return c.rgb();
        }
        let w = self.model.panes.get(&p)?.window;
        self.window_clients(w).into_iter().find_map(|id| {
            let r = self.clients.get(&id)?.reported;
            if fg { r.fg } else { r.bg }
        })
    }

    /// `window_pane_get_theme` (window.c:2795-2835): the clients' reported
    /// theme where they agree, else a guess from the background.
    fn pane_theme(&self, p: PaneId) -> Option<Theme> {
        let w = self.model.panes.get(&p)?.window;
        let (mut light, mut dark) = (false, false);
        for id in self.window_clients(w) {
            match self.clients.get(&id).and_then(|c| c.reported.theme) {
                Some(Theme::Light) => light = true,
                Some(Theme::Dark) => dark = true,
                None => {}
            }
        }
        match (light, dark) {
            (true, false) => Some(Theme::Light),
            (false, true) => Some(Theme::Dark),
            _ => self
                .pane_default(p, false)
                .and_then(|(r, g, b)| Colour::Rgb(r, g, b).theme()),
        }
    }

    /// `input_add_request`'s client: the most recently active attached
    /// client showing the pane's window whose terminal has started.
    fn request_client(&self, p: PaneId) -> Option<ClientId> {
        let w = self.model.panes.get(&p)?.window;
        self.window_clients(w)
            .into_iter()
            .filter(|id| self.clients.get(id).is_some_and(|c| c.term.is_some()))
            .max_by_key(|id| self.clients.get(id).map_or(0, |c| c.activity))
    }

    /// Answer what pane `p`'s program asked (`take_answers`): write the
    /// replies, in order, queueing behind an outstanding request.
    pub(crate) fn pane_answers(&mut self, p: PaneId, answers: Vec<Reply>) {
        for answer in answers {
            match answer {
                Reply::Bytes(b) => self.pane_reply(p, b, true),
                Reply::Foreground(end) => {
                    // input_osc_10: the window style, else a client's.
                    if let Some(c) = self.pane_default(p, true) {
                        self.pane_reply(p, colour_reply(10, None, c, end), true);
                    }
                }
                Reply::Background(end) => {
                    if let Some(c) = self.pane_default(p, false) {
                        self.pane_reply(p, colour_reply(11, None, c, end), true);
                    }
                }
                Reply::Cursor { colour, end } => {
                    // input_osc_12: s->ccolour, else s->default_ccolour
                    // (`cursor-colour`).
                    let c = colour.or_else(|| self.cursor_colour(p));
                    if let Some(c) = c {
                        self.pane_reply(p, colour_reply(12, None, c, end), true);
                    }
                }
                Reply::Palette { index, end } => self.palette_query(p, index, end),
                Reply::Clipboard { clip, end } => self.clipboard_query_pane(p, &clip, end),
                Reply::SetClipboard { clip, data } => self.set_clipboard_pane(p, &clip, &data),
                Reply::Theme => self.report_theme(p),
                Reply::ThemeUpdates(on) => {
                    // input.c:1985-1989, 2083-2088.
                    let theme = on.then(|| self.pane_theme(p)).flatten();
                    if let Some(pane) = self.model.panes.get_mut(&p) {
                        if on {
                            pane.requests.last_theme = theme;
                        }
                        pane.requests.theme_changed = false;
                    }
                }
            }
        }
    }

    /// `input_reply`: write `bytes` now, or (`add` and a request out)
    /// queue them behind it.
    fn pane_reply(&mut self, p: PaneId, bytes: Vec<u8>, add: bool) {
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        if add && !pane.requests.requests.is_empty() {
            pane.requests
                .requests
                .push_back((Instant::now(), Request::Queued(bytes)));
            return;
        }
        if let Some(io) = pane.io.as_mut() {
            io.write(&bytes);
        }
    }

    /// `cursor-colour` for the pane (`screen_set_default_cursor`).
    fn cursor_colour(&self, p: PaneId) -> Option<Rgb> {
        let text = option_text(&self.model, TreeId::Pane(p), "cursor-colour");
        let mut sy = Style::new(&CellStyle::default());
        // style_apply on a colour option: `style_parse_colour`.
        sy.parse(&CellStyle::default(), &format!("fg={text}"))
            .ok()?;
        sy.cell.fg.rgb()
    }

    /// `input_osc_4` for an entry the program did not set: the `pane-colours`
    /// entry (`colour_palette_get`'s `default_palette`), else a request.
    fn palette_query(&mut self, p: PaneId, index: u8, end: OscEnd) {
        let set = match self
            .model
            .options
            .get(TreeId::Pane(p), "pane-colours")
            .map(|e| &e.value)
        {
            Some(OptionValue::Array(items)) => {
                match items.get(&ArrayKey::Index(u32::from(index))) {
                    Some(Value::Colour(c)) => c.rgb(),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(c) = set {
            self.pane_reply(p, colour_reply(4, Some(index), c, end), true);
            return;
        }
        let Some(client) = self.request_client(p) else {
            return;
        };
        self.add_request(p, Request::Palette { client, index, end });
        // input_add_request: always with ST.
        let query = format!("\x1b]4;{index};?\x1b\\").into_bytes();
        self.send(client, &ServerMsg::Frame(query));
    }

    /// `input_osc_52_parse` and `input_osc_52_reply` for a query:
    /// nothing unless `set-clipboard on`; `get-clipboard buffer` answers
    /// from the top paste buffer, `request` and `both` ask the terminal.
    fn clipboard_query_pane(&mut self, p: PaneId, clip: &[u8], end: OscEnd) {
        if self.model.number_option(TreeId::Server, "set-clipboard") != 2 {
            return;
        }
        match self.model.number_option(TreeId::Server, "get-clipboard") {
            0 => {}
            1 => {
                let Some(top) = self.model.paste.top() else {
                    return;
                };
                let reply = clipboard_reply(clip.first().copied(), &top.data, end);
                // input_reply_clipboard writes straight to the pane.
                self.pane_reply(p, reply, false);
            }
            _ => {
                let Some(client) = self.request_client(p) else {
                    return;
                };
                let query = self
                    .clients
                    .get(&client)
                    .and_then(|c| c.term.as_ref())
                    .and_then(|t| t.clipboard_query());
                self.add_request(p, Request::Clipboard { client, end });
                if let Some(q) = query {
                    self.send(client, &ServerMsg::Frame(q));
                }
            }
        }
    }

    /// `input_osc_52` setting the clipboard under `set-clipboard on`: to
    /// every client showing the pane (`screen_write_setselection`),
    /// pane-set-clipboard, then a paste buffer.
    fn set_clipboard_pane(&mut self, p: PaneId, clip: &[u8], data: &[u8]) {
        if self.model.number_option(TreeId::Server, "set-clipboard") != 2 {
            return;
        }
        let Some(decoded) = crate::input::base64_decode(data).filter(|d| !d.is_empty()) else {
            return;
        };
        let window = self.model.panes.get(&p).map(|pane| pane.window);
        let clip = String::from_utf8_lossy(clip).into_owned();
        let clients: Vec<_> = self
            .clients
            .keys()
            .copied()
            .filter(|&c| window.is_some() && self.client_window(c) == window)
            .collect();
        for c in clients {
            self.set_selection_clip(c, &clip, &decoded);
        }
        self.model.fire_pane("pane-set-clipboard", p);
        let limit = crate::commands::buffer_limit(self);
        self.model.paste.add(decoded, limit);
    }

    fn add_request(&mut self, p: PaneId, r: Request) {
        if let Some(pane) = self.model.panes.get_mut(&p) {
            pane.requests.requests.push_back((Instant::now(), r));
        }
        self.update_requests_out();
    }

    /// Keep each client's decoder waiting longer while a request to it is
    /// out (tty-keys.c:1002-1009, `!TAILQ_EMPTY(&c->input_requests)`).
    fn update_requests_out(&mut self) {
        let out: std::collections::BTreeSet<ClientId> = self
            .model
            .panes
            .values()
            .flat_map(|p| p.requests.requests.iter().filter_map(|(_, r)| r.client()))
            .collect();
        for (id, c) in &mut self.clients {
            c.decoder.requests_out = out.contains(id);
        }
    }

    /// `input_request_reply` (input.c:3668-3711): a reply from client
    /// `id`'s terminal answers its oldest request of the kind (for a
    /// palette reply, of that entry); its older requests of another kind
    /// or entry are dropped. In that request's pane, the replies queued
    /// before it go first, then its answer, then those queued up to the
    /// next request; requests before it there are dropped.
    fn request_reply(
        &mut self,
        id: ClientId,
        palette: Option<u8>,
        answer: impl Fn(&Request) -> Option<Vec<u8>>,
    ) {
        // c->input_requests: the client's requests in the order they went.
        let mut mine: Vec<(Instant, PaneId, usize)> = self
            .model
            .panes
            .iter()
            .flat_map(|(&p, pane)| {
                pane.requests
                    .requests
                    .iter()
                    .enumerate()
                    .filter(|(_, (_, r))| r.client() == Some(id))
                    .map(move |(i, (t, _))| (*t, p, i))
            })
            .collect();
        mine.sort();
        let mut found = None;
        let mut drop = Vec::new();
        for (_, p, i) in mine {
            let r = &self.model.panes[&p].requests.requests[i].1;
            let matches = match (r, palette) {
                (Request::Palette { index, .. }, Some(want)) => *index == want,
                (Request::Clipboard { .. }, None) => true,
                _ => false,
            };
            if matches {
                found = Some((p, i));
                break;
            }
            drop.push((p, i));
        }
        // Drop from the back so the indices before stay right.
        drop.sort_unstable_by(|a, b| b.cmp(a));
        for (p, i) in drop {
            if let Some(pane) = self.model.panes.get_mut(&p) {
                if let Some((fp, fi)) = found
                    && fp == p
                    && i < fi
                {
                    found = Some((fp, fi - 1));
                }
                pane.requests.requests.remove(i);
            }
        }
        if let Some((p, at)) = found {
            let pane = self.model.panes.get_mut(&p).expect("found");
            let mut out = Vec::new();
            let mut complete = false;
            let mut popped = 0;
            while let Some((_, r)) = pane.requests.requests.front() {
                if complete && !matches!(r, Request::Queued(_)) {
                    break;
                }
                let (_, r) = pane.requests.requests.pop_front().expect("front");
                match r {
                    Request::Queued(b) => out.extend_from_slice(&b),
                    r if popped == at => {
                        out.extend(answer(&r).unwrap_or_default());
                        complete = true;
                    }
                    _ => {}
                }
                popped += 1;
            }
            if let Some(io) = pane.io.as_mut().filter(|_| !out.is_empty()) {
                io.write(&out);
            }
        }
        self.update_requests_out();
    }

    /// `input_request_timer_callback` (input.c:3521-3538): requests older
    /// than [`REQUEST_TIMEOUT`] go; the replies among them are written.
    pub(crate) fn request_timers(&mut self, now: Instant) {
        let mut any = false;
        for pane in self.model.panes.values_mut() {
            let mut out = Vec::new();
            while let Some((t, _)) = pane.requests.requests.front() {
                if *t + REQUEST_TIMEOUT > now {
                    break;
                }
                any = true;
                if let Some((_, Request::Queued(b))) = pane.requests.requests.pop_front() {
                    out.extend_from_slice(&b);
                }
            }
            if let Some(io) = pane.io.as_mut().filter(|_| !out.is_empty()) {
                io.write(&out);
            }
        }
        if any {
            self.update_requests_out();
        }
    }

    /// The soonest request timeout.
    pub(crate) fn request_deadline(&self) -> Option<Instant> {
        self.model
            .panes
            .values()
            .filter_map(|p| p.requests.deadline())
            .min()
    }

    /// `input_cancel_requests` (input.c:3713-3720): a client going drops
    /// the requests sent to it.
    pub(crate) fn cancel_requests(&mut self, id: ClientId) {
        for pane in self.model.panes.values_mut() {
            pane.requests
                .requests
                .retain(|(_, r)| r.client() != Some(id));
        }
        self.update_requests_out();
    }

    /// What client `id`'s terminal reported about itself: its colours,
    /// a palette entry, the clipboard or its theme.
    pub(crate) fn client_report(&mut self, id: ClientId, event: crate::input::Event) {
        use crate::input::Event;
        match event {
            Event::Clipboard(clip, data) => {
                // tty_keys_clipboard: a pane's query is answered, then
                // with a refresh-client -l query out, the reply becomes a
                // paste buffer.
                let query = self
                    .clients
                    .get_mut(&id)
                    .and_then(|c| c.clipboard_query.take())
                    .is_some_and(|until| until > Instant::now());
                self.client_clipboard(id, clip, &data);
                if query {
                    let limit = crate::commands::buffer_limit(self);
                    self.model.paste.add(data, limit);
                }
            }
            Event::Colour(fg, colour) => self.client_colour(id, fg, colour),
            Event::Palette(index, colour) => self.client_palette(id, index, colour),
            Event::Theme(theme) => self.client_theme(id, theme),
            Event::PixelSize(width, height) => self.client_pixel_size(id, width, height),
            _ => {}
        }
    }

    /// `tty_resize` (tty.c:124-160) for client `id`, whose terminal is now
    /// `cols` x `rows` and `pixels` in size: its cell size in pixels, and,
    /// when it doesn't give one, the size queries `CSI 18 t` and `CSI 14 t`
    /// (`TTY_WINSIZEQUERY`), once its terminal is open and unless they are
    /// out already. Every terminal vtmux opens is `TERM_VT100LIKE`. The
    /// pixels are kept in the client's hello, where tmux would read them
    /// with `TIOCGWINSZ` again ([`Server::tty_resize_again`]).
    pub(crate) fn tty_resize(&mut self, id: ClientId, cols: u32, rows: u32, pixels: (u32, u32)) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        if let Some(h) = c.hello.as_mut() {
            h.ext.pixels = pixels;
        }
        c.cell_pixels = cell_pixels(cols, rows, pixels);
        let (x, y) = c.cell_pixels;
        if (x == 0 || y == 0) && c.term.is_some() && !c.decoder.winsize_query() {
            c.decoder.winsize_sent();
            self.send(id, &ServerMsg::Frame(b"\x1b[18t\x1b[14t".to_vec()));
        }
    }

    /// [`Server::tty_resize`] with the size client `id`'s terminal last
    /// gave, as tmux's other `tty_resize` callers read it again
    /// (`popup_modify`, popup.c:551).
    pub(crate) fn tty_resize_again(&mut self, id: ClientId) {
        let Some(h) = self.clients.get(&id).and_then(|c| c.hello.as_ref()) else {
            return;
        };
        let Some(tty) = &h.tty else {
            return;
        };
        let (cols, rows, pixels) = (tty.cols, tty.rows, h.ext.pixels);
        self.tty_resize(id, cols, rows, pixels);
    }

    /// The terminal's reply to `CSI 14 t` (tty-keys.c:724-735): the text
    /// area's pixel size over the terminal's size in cells.
    fn client_pixel_size(&mut self, id: ClientId, width: u32, height: u32) {
        let Some(c) = self.clients.get_mut(&id) else {
            return;
        };
        let (cols, rows) = c
            .hello
            .as_ref()
            .and_then(|h| h.tty.as_ref())
            .map_or((80, 24), |t| (t.cols, t.rows));
        c.cell_pixels = cell_pixels(cols, rows, (width, height));
    }

    /// An OSC 4 reply from client `id`'s terminal (`tty_keys_palette`):
    /// entries 0-15 are kept for the pane blend; the reply to one of
    /// [`colour_queries`]' own queries goes no further (tmux sends none, so
    /// it must not answer, or drop, a pane's request).
    pub(crate) fn client_palette(&mut self, id: ClientId, index: u8, c: Rgb) {
        if let Some(client) = self.clients.get_mut(&id)
            && let Some(slot) = client.reported.palette.get_mut(usize::from(index))
        {
            *slot = Some(c);
            let bit = 1u16 << index;
            if client.reported.palette_out & bit != 0 {
                client.reported.palette_out &= !bit;
                return;
            }
        }
        self.request_reply(id, Some(index), |r| match r {
            Request::Palette { index, end, .. } => Some(colour_reply(4, Some(*index), c, *end)),
            _ => None,
        });
    }

    /// An OSC 52 reply from client `id`'s terminal for a pane's query
    /// (`input_request_clipboard_reply`, input.c:3643-3665): nothing under
    /// `get-clipboard off` or `buffer`; `both` also makes a paste buffer.
    pub(crate) fn client_clipboard(&mut self, id: ClientId, clip: Option<u8>, data: &[u8]) {
        let state = self.model.number_option(TreeId::Server, "get-clipboard");
        let has = self.model.panes.values().any(|p| {
            p.requests
                .requests
                .iter()
                .any(|(_, r)| matches!(r, Request::Clipboard { client, .. } if *client == id))
        });
        if has && state == 3 {
            let limit = crate::commands::buffer_limit(self);
            self.model.paste.add(data.to_vec(), limit);
        }
        self.request_reply(id, None, |r| match r {
            Request::Clipboard { end, .. } if state >= 2 => Some(clipboard_reply(clip, data, *end)),
            _ => None,
        });
    }

    /// An OSC 10 or 11 reply (`tty_keys_colours`): the client's colours;
    /// a new background redoes the theme colours and marks the session's
    /// panes `PANE_THEMECHANGED` (`session_theme_changed`).
    pub(crate) fn client_colour(&mut self, id: ClientId, fg: bool, c: Rgb) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        if fg {
            client.reported.fg = Some(c);
        } else {
            client.reported.bg = Some(c);
            // tty-keys.c:833-834: server_client_update_theme_colours.
            client.update_theme();
        }
        let s = client.attached;
        self.session_theme_changed(s);
    }

    /// `server_client_report_theme` (server-client.c:3288-3316): the
    /// terminal's theme report fires client-light-theme or
    /// client-dark-theme; a change redraws the client; the colours are
    /// asked for again.
    pub(crate) fn client_theme(&mut self, id: ClientId, theme: Theme) {
        let Some(client) = self.clients.get_mut(&id) else {
            return;
        };
        let old = client.reported.theme.replace(theme);
        // server-client.c:3303-3305: server_client_update_theme_colours,
        // then the redraw below.
        client.update_theme();
        self.fire_client(
            match theme {
                Theme::Light => "client-light-theme",
                Theme::Dark => "client-dark-theme",
            },
            id,
        );
        if old != Some(theme) {
            self.redraw_client(id);
        }
        // tty_repeat_requests(tty, 1).
        if let Some(c) = self.clients.get_mut(&id).filter(|c| c.term.is_some()) {
            c.decoder.colours_sent(Instant::now());
            c.reported.palette_sent();
            self.send(id, &ServerMsg::Frame(colour_queries()));
        }
    }

    /// `session_theme_changed`: every pane of the session.
    pub(crate) fn session_theme_changed(&mut self, s: Option<vtmux_core::ids::SessionId>) {
        let Some(session) = s.and_then(|s| self.model.sessions.get(&s)) else {
            return;
        };
        let windows: Vec<WindowId> = session.windows.values().copied().collect();
        for pane in self.model.panes.values_mut() {
            if windows.contains(&pane.window) {
                pane.requests.theme_changed();
            }
        }
    }

    /// `input_report_current_theme` (input.c:3723-3745): DSR 996 is
    /// answered at once, never queued.
    fn report_theme(&mut self, p: PaneId) {
        let theme = self.pane_theme(p);
        let Some(pane) = self.model.panes.get_mut(&p) else {
            return;
        };
        pane.requests.last_theme = theme;
        pane.requests.theme_changed = false;
        if let Some(t) = theme {
            self.pane_reply(p, t.report().to_vec(), false);
        }
    }

    /// `window_pane_send_theme_update` for every pane (server-client.c:
    /// 1945-1949): a live pane with theme updates on whose theme changed.
    pub(crate) fn send_theme_updates(&mut self) {
        let due: Vec<PaneId> = self
            .model
            .panes
            .iter()
            .filter(|(_, p)| p.requests.theme_changed && p.io.is_some() && p.term.theme_updates())
            .map(|(&id, _)| id)
            .collect();
        for p in due {
            let theme = self.pane_theme(p);
            let Some(pane) = self.model.panes.get_mut(&p) else {
                continue;
            };
            if theme == pane.requests.last_theme {
                continue;
            }
            pane.requests.last_theme = theme;
            pane.requests.theme_changed = false;
            if let (Some(t), Some(io)) = (theme, pane.io.as_mut()) {
                io.write(t.report());
            }
        }
    }
}

/// The terminal's colours asked for: OSC 10 and 11 as tmux does
/// (`tty_send_requests`, `tty_repeat_requests`), then, vtmux-only (D-033),
/// OSC 4 for palette entries 0-15, which themes change and the pane blend
/// (D26) mixes from.
#[must_use]
pub fn colour_queries() -> Vec<u8> {
    let mut out = b"\x1b]10;?\x1b\\\x1b]11;?\x1b\\".to_vec();
    for i in 0..BLEND_PALETTE {
        out.extend_from_slice(format!("\x1b]4;{i};?\x1b\\").as_bytes());
    }
    out
}

/// `tty_resize`'s cell size (tty.c:130-142): the terminal's pixel size
/// over its size in cells, 0 for a dimension of no cells.
#[must_use]
pub fn cell_pixels(cols: u32, rows: u32, (xpixel, ypixel): (u32, u32)) -> (u32, u32) {
    let per = |px: u32, n: u32| px.checked_div(n).unwrap_or(0);
    (per(xpixel, cols), per(ypixel, rows))
}

/// How many palette entries [`colour_queries`] asks for.
pub const BLEND_PALETTE: u8 = 16;

/// `input_reply_clipboard` (input.c:3484-3509).
fn clipboard_reply(clip: Option<u8>, data: &[u8], end: OscEnd) -> Vec<u8> {
    let mut out = b"\x1b]52;".to_vec();
    out.extend(clip);
    out.push(b';');
    out.extend_from_slice(vtmux_core::term_features::base64(data).as_bytes());
    out.extend_from_slice(end.bytes());
    out
}
