//! The client's status lines (status.c `status_redraw`): each
//! `status-format[]` line expanded through the format engine and drawn with
//! [`styled::draw`] (`format_draw`) over spaces in the status style, then
//! written as one `tty_draw_line` span per row (screen-redraw.c:1793-1811).
//!
//! Everything the status line shows (status-left/right and their styles and
//! lengths, status-justify, the window list with window-status-* formats,
//! styles and separator, the list markers) lives in the default
//! `status-format[0]`, so it comes from the options, not from code here.

use vtmux_core::colour::Colour;
use vtmux_core::ids::SessionId;
use vtmux_core::options::{ArrayKey, OptionValue, TreeId, Value};
use vtmux_core::style::CellStyle;
use vtmux_core::styled;
use vtmux_vt::Snapshot;

use super::Caps;
use super::compose::draw_line;
use super::window::Source;
use crate::format_ctx::Ctx;

/// Draw session `s`'s status lines into rows `y..y + lines` of `screen`.
/// `ctx` is the client's status format tree (`format_create(c, NULL,
/// FORMAT_NONE, FORMAT_STATUS)` + `format_defaults(ft, c, ...)`).
pub fn draw(
    source: &Source<'_>,
    ctx: &Ctx<'_>,
    s: SessionId,
    caps: Caps,
    screen: &mut Snapshot,
    y: u16,
    lines: u16,
) {
    let tree = TreeId::Session(s);
    let base = base_style(source, ctx, tree);
    let theme = source.theme(caps);
    let cols = usize::from(screen.cols);
    for (i, row) in (0..u32::from(lines)).zip(y..screen.rows) {
        // status.c:277-282: a line with no format is spaces in the style.
        let expanded = format(source, tree, i)
            .map(|f| vtmux_core::format::expand_time(&f, ctx))
            .unwrap_or_default();
        let line = styled::draw(&base, cols, &expanded, false, false);
        let cells = &mut screen.cells[usize::from(row) * cols..][..cols];
        for (cell, drawn) in cells.iter_mut().zip(&line.cells) {
            *cell = theme.cell(drawn);
        }
        draw_line(cells, caps.bce, false);
    }
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
