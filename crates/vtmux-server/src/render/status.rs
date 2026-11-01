//! The client's status lines (status.c): `status_redraw` expands each
//! `status-format[]` line through the format engine and draws it with
//! [`styled::draw`] (`format_draw`) over spaces in the status style;
//! `status_message_redraw` draws a message over a copy of those lines; the
//! result is written as one `tty_draw_line` span per row
//! (screen-redraw.c:1793-1811).
//!
//! Everything the status line shows (status-left/right and their styles and
//! lengths, status-justify, the window list with window-status-* formats,
//! styles and separator, the list markers) lives in the default
//! `status-format[0]`, so it comes from the options, not from code here.
//!
//! As in tmux, the lines are not expanded every frame: [`StatusLine`] keeps
//! what was drawn last and redraws only when asked (`CLIENT_REDRAWSTATUS`,
//! from the `server_status_*` / `server_redraw_*` calls and the
//! `status-interval` timer), so a change nothing announces stays unseen
//! until the next tick.

use std::time::{Duration, Instant};

use vtmux_core::colour::Colour;
use vtmux_core::ids::SessionId;
use vtmux_core::options::{ArrayKey, OptionValue, TreeId, Value};
use vtmux_core::style::{Align, CellStyle, Style, Width};
use vtmux_core::styled::{self, Cell};
use vtmux_vt::Snapshot;

use super::Caps;
use super::compose::draw_line_by;
use super::window::Source;
use crate::format_ctx::Ctx;
use crate::prompt::OpenPrompt;

/// A status-line message (`c->message_string` and its flags).
#[derive(Debug)]
struct Message {
    text: String,
    /// `message_ignore_styles`: `#` is escaped, so `#[...]` in the text is
    /// literal.
    ignore_styles: bool,
}

/// How a message shows (`status_message_set`'s flags).
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageFlags {
    /// `#[...]` in the text is literal.
    pub ignore_styles: bool,
    /// Keys do not clear the message (`-N`).
    pub ignore_keys: bool,
    /// The pane area keeps drawing under the message (`-C`).
    pub no_freeze: bool,
}

/// A client's status line state (`struct status_line` plus the client's
/// status flags and timers).
#[expect(
    clippy::struct_excessive_bools,
    reason = "independent CLIENT_* and TTY_* flags"
)]
#[derive(Debug)]
pub struct StatusLine {
    /// `CLIENT_REDRAWSTATUS`: redraw at the next frame.
    redraw: bool,
    /// `CLIENT_REDRAWWINDOW`: the pane area is drawn again at the next
    /// frame even while frozen.
    redraw_window: bool,
    /// `sl->screen`: the status lines as last drawn, one row per line. It
    /// keeps its last content while the status is off.
    screen: Vec<Vec<Cell>>,
    /// `sl->active` while a message shows: the message over a copy of
    /// `screen`.
    active: Vec<Vec<Cell>>,
    message: Option<Message>,
    /// `c->prompt`. A message shows over it.
    prompt: Option<OpenPrompt>,
    /// `sl->prompt_cx` and the prompt's line in `active`.
    prompt_cursor: (u16, usize),
    /// `TTY_FREEZE`: pane output is not written to the terminal while a
    /// message or prompt shows (tty.c:1646 `tty_client_ready`), so the
    /// pane area keeps what it showed until a full redraw; `-C` turns it
    /// off. Clearing a message over a prompt leaves it (status.c:401-402).
    frozen: bool,
    /// `TTY_NOCURSOR`: set by a message, cleared with it unless a prompt
    /// is up, and by clearing the prompt.
    nocursor: bool,
    /// `c->message_timer`. Clearing a message leaves it running and a
    /// `-d 0` message does not re-arm it (status.c:372-379,
    /// `status_message_clear`), so an earlier timer still clears a later
    /// message when it fires.
    message_timer: Option<Instant>,
    /// `c->message_ignore_keys`. Only a message with a delay sets it
    /// (status.c:381-382), so a later `-d 0` message keeps the old value.
    ignore_keys: bool,
    /// `c->status.timer`: the next `status-interval` tick.
    timer: Option<Instant>,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self {
            redraw: true,
            redraw_window: false,
            screen: Vec::new(),
            active: Vec::new(),
            message: None,
            prompt: None,
            prompt_cursor: (0, 0),
            frozen: false,
            nocursor: false,
            message_timer: None,
            ignore_keys: false,
            timer: None,
        }
    }
}

impl StatusLine {
    /// `server_status_client` / `server_redraw_client`.
    pub fn request_redraw(&mut self) {
        self.redraw = true;
    }

    /// `server_redraw_client` (`CLIENT_ALLREDRAWFLAGS`): the status and the
    /// pane area.
    pub fn request_full_redraw(&mut self) {
        self.redraw = true;
        self.redraw_window = true;
    }

    /// Whether this frame keeps the pane area the terminal shows (frozen
    /// and no full redraw asked for); the window redraw request is used up.
    pub fn hold_window(&mut self) -> bool {
        let redraw = std::mem::take(&mut self.redraw_window);
        self.frozen && !redraw
    }

    /// `status_timer_start`: with the status on, tick now and every
    /// `interval` seconds after; off, no timer.
    pub fn start_timer(&mut self, now: Instant, on: bool, interval: u64) {
        self.timer = None;
        if on {
            self.fire_timer(now, interval);
        }
    }

    /// `status_timer_callback`: redraw unless a message or prompt is up,
    /// and come back in `interval` seconds (0 stops the timer).
    fn fire_timer(&mut self, now: Instant, interval: u64) {
        if self.message.is_none() && self.prompt.is_none() {
            self.redraw = true;
        }
        self.timer = (interval != 0).then(|| now + Duration::from_secs(interval));
    }

    /// `status_message_set` with a resolved delay in milliseconds (0 waits
    /// for a key).
    pub fn set_message(&mut self, now: Instant, text: String, delay_ms: u64, flags: MessageFlags) {
        self.clear_message();
        if delay_ms != 0 {
            self.message_timer = Some(now + Duration::from_millis(delay_ms));
            self.ignore_keys = flags.ignore_keys;
        }
        self.message = Some(Message {
            text,
            ignore_styles: flags.ignore_styles,
        });
        if !flags.no_freeze {
            self.frozen = true;
        }
        self.nocursor = true;
        self.redraw = true;
    }

    /// `status_message_clear`: the tty is unfrozen (unless a prompt is up)
    /// and the whole client redrawn, as the pane area may have changed
    /// under the message.
    pub fn clear_message(&mut self) {
        if self.message.take().is_some() {
            if self.prompt.is_none() {
                self.frozen = false;
                self.nocursor = false;
            }
            self.request_full_redraw();
        }
    }

    /// `status_prompt_set`: any message goes, then any prompt (returned,
    /// for its owner to be told); the pane area freezes unless `-C`.
    pub fn set_prompt(&mut self, prompt: OpenPrompt) -> Option<OpenPrompt> {
        self.clear_message();
        let old = self.clear_prompt();
        // status.c:612-613.
        if !prompt.editor.no_freeze && prompt.editor.kind != crate::prompt::Kind::Incremental {
            self.frozen = true;
        }
        self.prompt = Some(prompt);
        self.redraw = true;
        old
    }

    /// `status_prompt_clear`: unfrozen and redrawn in full.
    pub fn clear_prompt(&mut self) -> Option<OpenPrompt> {
        let old = self.prompt.take()?;
        self.frozen = false;
        self.nocursor = false;
        self.request_full_redraw();
        Some(old)
    }

    /// The prompt shown, if any.
    #[must_use]
    pub fn prompt(&self) -> Option<&OpenPrompt> {
        self.prompt.as_ref()
    }

    /// The prompt, to edit; the status is drawn again.
    pub fn prompt_mut(&mut self) -> Option<&mut OpenPrompt> {
        self.redraw |= self.prompt.is_some();
        self.prompt.as_mut()
    }

    /// The message shown, if any (`c->message_string`).
    #[must_use]
    pub fn message(&self) -> Option<&str> {
        self.message.as_ref().map(|m| m.text.as_str())
    }

    /// A key arrived (server-client.c:1786-1790): `true` when the message
    /// swallows it; otherwise the message goes and the key carries on.
    pub fn key(&mut self) -> bool {
        if self.message.is_none() {
            return false;
        }
        if self.ignore_keys {
            return true;
        }
        self.clear_message();
        false
    }

    /// Run the timers due at `now`: the message expires
    /// (`status_message_callback`) and the status ticks, every `interval`
    /// seconds.
    pub fn tick(&mut self, now: Instant, interval: u64) {
        if self.message_timer.is_some_and(|d| d <= now) {
            self.message_timer = None;
            self.clear_message();
        }
        if self.timer.is_some_and(|t| t <= now) {
            self.fire_timer(now, interval);
        }
    }

    /// When [`Self::tick`] next has something to do.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        match (self.message_timer, self.timer) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    /// Session `s`'s status, as this client should show it in `screen`,
    /// redrawn first when needed. `ctx` is the client's status format tree
    /// (`format_create(c, NULL, FORMAT_NONE, FORMAT_STATUS)` +
    /// `format_defaults(ft, c, ...)`); `lines` is `status_line_size(c)`; `top`
    /// is `status-position top`. A message takes at least one row, over the
    /// pane area when the status is off (screen-redraw.c:1795-1800); the
    /// cursor is hidden while it shows (`TTY_NOCURSOR`).
    #[expect(clippy::too_many_arguments, reason = "status_redraw's inputs")]
    pub fn draw(
        &mut self,
        source: &Source<'_>,
        ctx: &mut Ctx<'_>,
        s: SessionId,
        caps: Caps,
        screen: &mut Snapshot,
        lines: u16,
        top: bool,
    ) {
        let cols = usize::from(screen.cols);
        let lines = usize::from(lines);
        if self.needs_redraw(cols, lines) {
            self.redraw = false;
            if let Some(m) = &self.message {
                self.active = message_rows(source, ctx, s, &self.screen, m, cols, lines);
            } else if let Some(p) = &self.prompt {
                let (rows, cursor) = prompt_rows(source, s, &self.screen, p, cols, lines);
                self.active = rows;
                self.prompt_cursor = cursor;
            } else if lines != 0 {
                self.screen = status_rows(source, ctx, s, cols, lines);
            }
        }
        if self.nocursor {
            screen.cursor_visible = false;
        }
        let rows = if self.message.is_some() || self.prompt.is_some() {
            &self.active
        } else if lines == 0 {
            return;
        } else {
            &self.screen
        };
        let n = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        let y = if top {
            0
        } else {
            screen.rows.saturating_sub(n)
        };
        if self.prompt.is_some() {
            // server-client.c:2220-2222: while a prompt is set the cursor
            // goes to status_prompt_cursor, hidden or not (a message over
            // the prompt, TTY_NOCURSOR kept after it). A column past the
            // last one stays pending wrap (tty_cursor, tty.c:2468).
            let (x, line) = self.prompt_cursor;
            let row = y.saturating_add(u16::try_from(line).unwrap_or(0));
            screen.cursor = Some((x.min(screen.cols), row.min(screen.rows - 1)));
            screen.cursor_visible = !self.nocursor;
        }
        let theme = source.theme(caps);
        for (row, drawn) in (y..screen.rows).zip(rows) {
            let cells = &mut screen.cells[usize::from(row) * cols..][..cols];
            for (cell, drawn) in cells.iter_mut().zip(drawn) {
                *cell = theme.cell(drawn);
            }
            let bgs: Vec<Colour> = drawn.iter().map(|c| c.style.bg).collect();
            draw_line_by(cells, &bgs, Colour::Default, caps.clear(), false);
        }
    }

    /// Whether the next frame must draw the status again: asked for, or the
    /// drawn lines no longer fit `cols` x `lines` (tmux redraws on every
    /// size change, `server_redraw_client`).
    fn needs_redraw(&self, cols: usize, lines: usize) -> bool {
        let fits =
            |rows: &[Vec<Cell>], n: usize| rows.len() == n && rows.iter().all(|r| r.len() == cols);
        self.redraw
            || if self.message.is_some() || self.prompt.is_some() {
                !fits(&self.active, lines.max(1))
            } else {
                lines != 0 && !fits(&self.screen, lines)
            }
    }
}

/// `status_redraw` (status.c:215-312): every line from its
/// `status-format[]` entry over spaces in the status style.
fn status_rows(
    source: &Source<'_>,
    ctx: &Ctx<'_>,
    s: SessionId,
    cols: usize,
    lines: usize,
) -> Vec<Vec<Cell>> {
    let tree = TreeId::Session(s);
    let base = base_style(source, ctx, tree);
    (0..u32::try_from(lines).unwrap_or(u32::MAX))
        .map(|i| {
            // status.c:277-282: a line with no format is spaces in the style.
            let expanded = format(source, tree, i)
                .map(|f| vtmux_core::format::expand_time(&f, ctx))
                .unwrap_or_default();
            styled::draw(&base, cols, &expanded, false, false).cells
        })
        .collect()
}

/// The `range=` ranges of each status line as `status_redraw` records them
/// (`sl->entries[i].ranges`), for `display-popup -x W`.
#[must_use]
pub fn status_ranges(
    source: &Source<'_>,
    ctx: &Ctx<'_>,
    s: SessionId,
    cols: usize,
    lines: usize,
) -> Vec<Vec<styled::StyleRange>> {
    let tree = TreeId::Session(s);
    let base = base_style(source, ctx, tree);
    (0..u32::try_from(lines).unwrap_or(u32::MAX))
        .map(|i| {
            let expanded = format(source, tree, i)
                .map(|f| vtmux_core::format::expand_time(&f, ctx))
                .unwrap_or_default();
            styled::draw(&base, cols, &expanded, true, false).ranges
        })
        .collect()
}

/// What the status lines show under a message or prompt
/// (`status_push_screen`, `screen_write_fast_copy`): what of the status
/// fits in `lines` (at least one) rows of `cols`, the rest default cells.
fn status_copy(status: &[Vec<Cell>], cols: usize, lines: usize) -> Vec<Vec<Cell>> {
    let blank = Cell {
        text: " ".into(),
        width: 1,
        style: CellStyle::default(),
    };
    (0..lines.max(1))
        .map(|y| {
            (0..cols)
                .map(|x| {
                    status
                        .get(y)
                        .and_then(|r| r.get(x))
                        .unwrap_or(&blank)
                        .clone()
                })
                .collect()
        })
        .collect()
}

/// `status_prompt_line_at`: `message-line`, at most the last line.
fn prompt_line(source: &Source<'_>, tree: TreeId, lines: usize) -> usize {
    usize::try_from(source.model.number_option(tree, "message-line"))
        .unwrap_or(0)
        .min(lines.max(1) - 1)
}

/// `status_prompt_redraw` and `prompt_draw` (prompt.c:601-660): the
/// expanded `message-format` (with `#{message}` the expanded prompt) then
/// the visible part of the input, in the area `message-style` gives, over
/// a copy of the status lines; and where the cursor goes, as a column and
/// a line of the rows.
///
/// The formats are expanded in the prompt's own tree
/// (`prompt_format_tree`): its target, no client, and `prompt_input`,
/// `prompt_flags`, `prompt_type` and `command_prompt`.
fn prompt_rows(
    source: &Source<'_>,
    s: SessionId,
    status: &[Vec<Cell>],
    prompt: &OpenPrompt,
    cols: usize,
    lines: usize,
) -> (Vec<Vec<Cell>>, (u16, usize)) {
    let tree = TreeId::Session(s);
    let mut rows = status_copy(status, cols, lines);
    let line = prompt_line(source, tree, lines);
    let (ax, aw) = message_area(source, tree, cols);
    let cursor_x = draw_prompt(source, tree, prompt, &mut rows[line], ax, aw);
    (rows, (u16::try_from(cursor_x).unwrap_or(u16::MAX), line))
}

/// `redraw_draw_pane_prompt` (screen-redraw.c:1625-1673): a pane prompt
/// is drawn on a blank line as wide as the pane, its whole width the
/// prompt's area; and the cursor's column in it (`wp->prompt_cx`).
#[must_use]
pub fn pane_prompt_row(
    source: &Source<'_>,
    tree: TreeId,
    prompt: &OpenPrompt,
    width: usize,
) -> (Vec<Cell>, usize) {
    let blank = Cell {
        text: " ".into(),
        width: 1,
        style: CellStyle::default(),
    };
    let mut row = vec![blank; width];
    let cursor_x = draw_prompt(source, tree, prompt, &mut row, 0, width);
    (row, cursor_x)
}

/// `prompt_draw` in the area `ax`, `aw` of `row`; returns the cursor's
/// column.
fn draw_prompt(
    source: &Source<'_>,
    tree: TreeId,
    prompt: &OpenPrompt,
    row: &mut Vec<Cell>,
    ax: usize,
    aw: usize,
) -> usize {
    let editor = &prompt.editor;
    let mut ctx = Ctx::new(source.model, source.info, source.env, prompt.state);
    for (k, v) in editor.format_vars() {
        ctx.extra.push((k.into(), v));
    }
    let sy = prompt_style(source, tree, editor.command_mode(), &ctx);
    // prompt_expand1.
    let label = vtmux_core::format::expand_time(&editor.label, &ctx);
    ctx.extra.push(("message".into(), label));
    let template = source
        .model
        .options
        .get(tree, "message-format")
        .map(|e| e.value.to_text(e.def, None, false))
        .unwrap_or_default();
    let expanded = vtmux_core::format::expand_time(&template, &ctx);

    // prompt_layout (prompt.c:464-556).
    let label_width = styled::format_width(&expanded).min(aw);
    let cells = editor.cells();
    let pcursor = editor.cursor_width();
    let pwidth: usize = cells.iter().map(|c| usize::from(c.1)).sum();
    let avail = aw - label_width;
    let (offset, input_width) = if avail == 0 {
        (0, 0)
    } else if pcursor >= avail {
        (pcursor - avail + 1, avail)
    } else {
        (0, pwidth.min(avail))
    };
    let content_width = (label_width + input_width).min(aw);
    let content_x = match sy.align {
        Align::Centre | Align::AbsoluteCentre => ax + (aw - content_width) / 2,
        Align::Right => ax + aw - content_width,
        Align::Default | Align::Left => ax,
    };
    let input_x = content_x + label_width;
    let cursor_x = if avail == 0 {
        content_x + label_width
    } else {
        input_x + pcursor - offset
    };

    // screen_write_clearcharacter in the fill colour.
    if sy.fill != Colour::Default {
        for cell in &mut row[ax..ax + aw] {
            *cell = Cell {
                text: " ".into(),
                width: 1,
                style: CellStyle {
                    bg: sy.fill,
                    ..CellStyle::default()
                },
            };
        }
    }
    if content_width != 0 {
        if label_width != 0 {
            let under = row[content_x..content_x + label_width].to_vec();
            let drawn = styled::draw_onto(&sy.cell, under, &expanded, false, false).cells;
            row.splice(content_x..content_x + label_width, drawn);
        }
        draw_input(row, input_x, &cells, offset, input_width, &sy.cell);
    }
    cursor_x
}

/// `prompt_effective_style`: `message-style`, or `message-command-style`
/// in vi command mode, expanded in the prompt's tree over the default
/// cell; a style that does not parse is the default cell.
fn prompt_style(source: &Source<'_>, tree: TreeId, command_mode: bool, ctx: &Ctx<'_>) -> Style {
    let name = if command_mode {
        "message-command-style"
    } else {
        "message-style"
    };
    let text = source
        .model
        .options
        .get(tree, name)
        .map(|e| e.value.to_text(e.def, None, false))
        .unwrap_or_default();
    let base = CellStyle::default();
    let mut sy = Style::new(&base);
    if sy
        .parse(&base, &vtmux_core::format::expand_time(&text, ctx))
        .is_err()
    {
        sy = Style::new(&base);
    }
    sy
}

/// `prompt_redraw_character`: the whole characters from column `offset`
/// that fit in `width`, from `x` on; a control character is `^X`.
fn draw_input(
    row: &mut [Cell],
    mut x: usize,
    cells: &[(String, u8)],
    offset: usize,
    width: usize,
    style: &CellStyle,
) {
    let mut used = 0;
    let mut put = |text: String, w: u8| {
        if let Some(cell) = row.get_mut(x) {
            *cell = Cell {
                text,
                width: w,
                style: style.clone(),
            };
        }
        x += 1;
    };
    for (text, w) in cells {
        let w = usize::from(*w);
        if used < offset {
            used += w;
            continue;
        }
        if used + w > offset + width {
            break;
        }
        used += w;
        if text.starts_with('^') && text.len() == 2 {
            for c in text.chars() {
                put(c.to_string(), 1);
            }
        } else if w == 2 {
            put(text.clone(), 2);
            put(String::new(), 0);
        } else {
            put(text.clone(), 1);
        }
    }
}

/// `status_message_redraw` (status.c:461-532): `message-format` with
/// `#{message}` set, drawn in `message-style` on the `message-line` row of
/// a copy of the status lines, inside the area `message-style`'s `width`
/// and `align` give.
fn message_rows(
    source: &Source<'_>,
    ctx: &mut Ctx<'_>,
    s: SessionId,
    status: &[Vec<Cell>],
    m: &Message,
    cols: usize,
    lines: usize,
) -> Vec<Vec<Cell>> {
    let tree = TreeId::Session(s);
    let line = prompt_line(source, tree, lines);
    let mut rows = status_copy(status, cols, lines);
    let (ax, aw) = message_area(source, tree, cols);
    let text = if m.ignore_styles {
        m.text.replace('#', "##")
    } else {
        m.text.clone()
    };
    ctx.extra.push(("message".into(), text));
    ctx.extra.push(("command_prompt".into(), "0".into()));
    let gc = source.style(tree, "message-style", ctx);
    let template = source
        .model
        .options
        .get(tree, "message-format")
        .map(|e| e.value.to_text(e.def, None, false))
        .unwrap_or_default();
    let expanded = vtmux_core::format::expand_time(&template, ctx);
    let row = &mut rows[line];
    let under = row[ax..ax + aw].to_vec();
    let drawn = styled::draw_onto(&gc, under, &expanded, false, false).cells;
    row.splice(ax..ax + aw, drawn);
    rows
}

/// `status_message_area` (status.c:412-450): `message-style` parsed
/// without a format tree (`options_string_to_style(.., NULL)`), so a style
/// with `#{` in it, like the default, fails and the area is the whole row.
fn message_area(source: &Source<'_>, tree: TreeId, cols: usize) -> (usize, usize) {
    let text = source
        .model
        .options
        .get(tree, "message-style")
        .map(|e| e.value.to_text(e.def, None, false))
        .unwrap_or_default();
    let base = CellStyle::default();
    let mut sy = Style::new(&base);
    if sy.parse(&base, &text).is_err() {
        return (0, cols);
    }
    let w = match sy.width {
        Some(Width::Cells(n)) => usize::try_from(n).unwrap_or(usize::MAX),
        Some(Width::Percent(p)) => cols * usize::from(p) / 100,
        None => cols,
    };
    let w = if w == 0 || w > cols { cols } else { w };
    let x = match sy.align {
        Align::Centre | Align::AbsoluteCentre => (cols - w) / 2,
        Align::Right => cols - w,
        Align::Default | Align::Left => 0,
    };
    (x, w)
}

/// status.c:247-254: `status-style` applied over the default cell, then
/// `status-fg` and `status-bg` where they are not `default`/`terminal`
/// (`COLOUR_DEFAULT`).
fn base_style(source: &Source<'_>, ctx: &Ctx<'_>, tree: TreeId) -> CellStyle {
    let mut base = source.style(tree, "status-style", ctx);
    let colour = |name: &str| match source.model.options.get(tree, name).map(|e| &e.value) {
        Some(OptionValue::Single(Value::Colour(c)))
            if !matches!(c, Colour::Default | Colour::Terminal) =>
        {
            Some(*c)
        }
        _ => None,
    };
    if let Some(fg) = colour("status-fg") {
        base.fg = fg;
    }
    if let Some(bg) = colour("status-bg") {
        base.bg = bg;
    }
    base
}

/// `status-format[line]` (`options_array_getv`), if set.
fn format(source: &Source<'_>, tree: TreeId, line: u32) -> Option<String> {
    match &source.model.options.get(tree, "status-format")?.value {
        OptionValue::Array(items) => match items.get(&ArrayKey::Index(line))? {
            Value::String(f) => Some(f.clone()),
            _ => None,
        },
        OptionValue::Single(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(sl: &mut StatusLine) -> bool {
        std::mem::take(&mut sl.redraw)
    }

    fn flags(ignore_keys: bool) -> MessageFlags {
        MessageFlags {
            ignore_keys,
            ..MessageFlags::default()
        }
    }

    /// status.c `status_timer_callback`: a tick every `status-interval`
    /// seconds marks the status for redraw, and nothing in between does.
    #[test]
    fn status_interval_ticks_redraw() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut sl = StatusLine::default();
        assert!(drawn(&mut sl), "a new client draws its status");
        sl.start_timer(t0, true, 2);
        assert!(drawn(&mut sl), "starting the timer ticks at once");
        assert_eq!(sl.deadline(), Some(t0 + s(2)));
        sl.tick(t0 + s(1), 2);
        assert!(!drawn(&mut sl), "nothing before the interval");
        sl.tick(t0 + s(2), 2);
        assert!(drawn(&mut sl));
        assert_eq!(sl.deadline(), Some(t0 + s(4)));
        sl.tick(t0 + s(3), 2);
        assert!(!drawn(&mut sl));
        sl.tick(t0 + s(4), 2);
        assert!(drawn(&mut sl));
    }

    /// `status-interval 0` and `status off` keep no timer.
    #[test]
    fn status_interval_zero_or_status_off_has_no_timer() {
        let t0 = Instant::now();
        let mut sl = StatusLine::default();
        sl.start_timer(t0, true, 0);
        assert_eq!(sl.deadline(), None);
        sl.start_timer(t0, false, 15);
        assert_eq!(sl.deadline(), None);
    }

    /// A tick while a message shows does not redraw (the message stays),
    /// but the timer carries on; the message clears after its delay.
    #[test]
    fn message_holds_off_the_tick_until_it_expires() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut sl = StatusLine::default();
        sl.start_timer(t0, true, 1);
        drawn(&mut sl);
        sl.set_message(t0, "hello".into(), 1500, flags(false));
        assert!(drawn(&mut sl));
        assert_eq!(sl.deadline(), Some(t0 + ms(1000)));
        sl.tick(t0 + ms(1000), 1);
        assert!(!drawn(&mut sl), "tick under a message");
        assert_eq!(sl.message(), Some("hello"));
        assert_eq!(sl.deadline(), Some(t0 + ms(1500)));
        sl.tick(t0 + ms(1500), 1);
        assert!(drawn(&mut sl), "the message cleared");
        assert_eq!(sl.message(), None);
    }

    /// server-client.c:1786-1790 and status.c:381-382.
    #[test]
    fn keys_clear_or_are_swallowed() {
        let t0 = Instant::now();
        let mut sl = StatusLine::default();
        sl.set_message(t0, "m".into(), 0, flags(false));
        assert_eq!(sl.deadline(), None, "-d 0 waits for a key");
        assert!(!sl.key());
        assert_eq!(sl.message(), None);
        sl.set_message(t0, "n".into(), 500, flags(true));
        assert!(sl.key(), "-N swallows the key");
        assert_eq!(sl.message(), Some("n"));
        // A delay-0 message keeps the last ignore-keys setting.
        sl.set_message(t0, "o".into(), 0, flags(false));
        assert!(sl.key());
        assert_eq!(sl.message(), Some("o"));
    }

    /// status.c:372-379: clearing a message leaves `c->message_timer`
    /// running and a `-d 0` message does not re-arm it, so the earlier
    /// timer clears the later message.
    #[test]
    fn an_earlier_timer_clears_a_wait_for_key_message() {
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        let mut sl = StatusLine::default();
        sl.set_message(t0, "first".into(), 500, flags(false));
        sl.set_message(t0 + ms(100), "second".into(), 0, flags(false));
        assert_eq!(sl.deadline(), Some(t0 + ms(500)));
        sl.tick(t0 + ms(500), 0);
        assert_eq!(sl.message(), None);
        assert_eq!(sl.deadline(), None);

        // A key clears the first message; the timer still fires.
        sl.set_message(t0, "first".into(), 1500, flags(false));
        assert!(!sl.key());
        sl.set_message(t0 + ms(100), "second".into(), 0, flags(false));
        sl.tick(t0 + ms(1000), 0);
        assert_eq!(sl.message(), Some("second"));
        sl.tick(t0 + ms(1500), 0);
        assert_eq!(sl.message(), None);

        // A later timed message re-arms it.
        sl.set_message(t0, "first".into(), 500, flags(false));
        sl.set_message(t0 + ms(100), "third".into(), 1000, flags(false));
        sl.tick(t0 + ms(500), 0);
        assert_eq!(sl.message(), Some("third"));
        sl.tick(t0 + ms(1100), 0);
        assert_eq!(sl.message(), None);
    }

    /// `TTY_FREEZE`: a message without `-C` holds the pane area until a
    /// full redraw (`server_redraw_client`) or the message clears.
    #[test]
    fn a_message_freezes_the_pane_area_unless_no_freeze() {
        let t0 = Instant::now();
        let mut sl = StatusLine::default();
        assert!(!sl.hold_window());
        sl.set_message(t0, "m".into(), 0, flags(false));
        assert!(sl.hold_window());
        sl.request_full_redraw();
        assert!(!sl.hold_window(), "a full redraw draws the panes once");
        assert!(sl.hold_window());
        sl.clear_message();
        assert!(!sl.hold_window());
        sl.set_message(
            t0,
            "c".into(),
            0,
            MessageFlags {
                no_freeze: true,
                ..MessageFlags::default()
            },
        );
        assert!(!sl.hold_window(), "-C");
    }
}
