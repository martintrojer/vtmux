//! Pane scrollbars (`pane-scrollbars`, tmux 3.8): which panes show one,
//! where it sits, how its slider is placed, and the auto-hide timer
//! (window.c `window_pane_show_scrollbar` .. `window_pane_scrollbar_hide`,
//! style.c `style_set_scrollbar_style_from_option`, screen-redraw.c
//! `redraw_draw_scrollbar_span`).
//!
//! `on` reserves columns next to the pane ([`Model::fix_panes`] narrows
//! the pane); `modal` (copy and view mode only) and `auto-hide` draw over
//! the pane's own columns, and only while shown: something scrolled or the
//! pointer moved onto the bar, and `pane-scrollbars-timeout` has not run
//! out since. The scene draws the bar ([`crate::render::scene`]); the
//! mouse finds it ([`crate::mouse`]).

use std::time::{Duration, Instant};

use vtmux_core::ids::{PaneId, WindowId};
use vtmux_core::layout::Geometry;
use vtmux_core::options::TreeId;
use vtmux_core::style::{CellStyle, Style, Width};

use crate::mode::Mode;
use crate::model::{Model, Redraw};

/// `pane-scrollbars`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scrollbars {
    Off,
    /// Only in copy and view mode, over the pane, auto-hidden.
    Modal,
    /// Always, in columns of its own.
    On,
    /// Over the pane, auto-hidden.
    AutoHide,
}

impl Scrollbars {
    fn from_choice(n: i64) -> Self {
        match n {
            1 => Self::Modal,
            2 => Self::On,
            3 => Self::AutoHide,
            _ => Self::Off,
        }
    }

    /// `window_pane_scrollbar_auto_hide`.
    #[must_use]
    pub fn auto_hide(self) -> bool {
        matches!(self, Self::Modal | Self::AutoHide)
    }
}

/// `pane-scrollbars-style` as `style_set_scrollbar_style_from_option`
/// leaves it: the cell look, a width of at least 1 and the padding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BarStyle {
    pub cell: CellStyle,
    pub width: u32,
    pub pad: u32,
}

/// How a pane's scrollbar sits (`sb_w`, `sb_left`, `overlay` of
/// `redraw_mark_pane`): `width` columns (bar plus padding) left or right
/// of the pane, or over its own edge columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub width: u32,
    pub left: bool,
    pub overlay: bool,
}

/// A pane's auto-hide state (`sb_auto_visible`, `sb_auto_hover`,
/// `sb_auto_timer`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AutoHide {
    pub visible: bool,
    pub hover: bool,
    pub hide_at: Option<Instant>,
}

/// The default of `pane-scrollbars-style` (options-table.c).
const DEFAULT_STYLE: &str = "bg=themedarkgrey,fg=themelightgrey,width=1,pad=0";

/// `style_set_scrollbar_style_from_option`: the default style, then the
/// option over it (or the default again if the option does not parse).
/// The option is a format expanded with no target; vtmux expands nothing
/// in it (a style option with `#{` is rare and has no target to read).
fn bar_style(text: &str) -> BarStyle {
    let base = CellStyle::default();
    let mut sy = Style::new(&base);
    let _ = sy.parse(&base, DEFAULT_STYLE);
    let default = sy.clone();
    if sy.parse(&base, text).is_err() {
        sy = default;
    }
    let width = match sy.width {
        Some(Width::Cells(n)) => n,
        Some(Width::Percent(n)) => u32::from(n),
        None => 0,
    };
    BarStyle {
        cell: sy.cell,
        width: width.max(1),
        pad: sy.pad.unwrap_or(0),
    }
}

impl Model {
    /// `w->sb`.
    #[must_use]
    pub fn scrollbars(&self, w: WindowId) -> Scrollbars {
        Scrollbars::from_choice(self.number_option(TreeId::Window(w), "pane-scrollbars"))
    }

    /// `w->sb_pos == PANE_SCROLLBARS_LEFT`.
    #[must_use]
    pub fn scrollbar_left(&self, w: WindowId) -> bool {
        self.number_option(TreeId::Window(w), "pane-scrollbars-position") == 1
    }

    /// `wp->scrollbar_style`.
    #[must_use]
    pub fn scrollbar_style(&self, p: PaneId) -> BarStyle {
        let text = self
            .options
            .get(TreeId::Pane(p), "pane-scrollbars-style")
            .map(|e| e.value.to_text(e.def, None, false))
            .unwrap_or_default();
        bar_style(&text)
    }

    /// `window_pane_show_scrollbar` (window.c:2601-2622): not on the
    /// alternate screen, not while the zoomed window's active pane is in
    /// a mode that hides them (panes-mode), and per `pane-scrollbars`
    /// (`modal` only in copy or view mode).
    #[must_use]
    pub fn show_scrollbar(&self, p: PaneId) -> bool {
        let Some(pane) = self.panes.get(&p) else {
            return false;
        };
        if pane.term.alternate_on() {
            return false;
        }
        let Some(window) = self.windows.get(&pane.window) else {
            return false;
        };
        if window.zoomed()
            && self
                .panes
                .get(&window.active())
                .and_then(|a| a.modes.first())
                .is_some_and(Mode::hides_scrollbars)
        {
            return false;
        }
        match self.scrollbars(pane.window) {
            Scrollbars::On | Scrollbars::AutoHide => true,
            Scrollbars::Modal => pane.modes.first().is_some_and(|m| m.as_copy().is_some()),
            Scrollbars::Off => false,
        }
    }

    fn scrollbars_of(&self, p: PaneId) -> Scrollbars {
        self.panes
            .get(&p)
            .map_or(Scrollbars::Off, |x| self.scrollbars(x.window))
    }

    /// `window_pane_scrollbar_reserve`: the bar takes columns of its own.
    #[must_use]
    pub fn scrollbar_reserve(&self, p: PaneId) -> bool {
        self.show_scrollbar(p) && self.scrollbars_of(p) == Scrollbars::On
    }

    /// `window_pane_scrollbar_overlay`: the bar goes over the pane.
    #[must_use]
    pub fn scrollbar_overlay(&self, p: PaneId) -> bool {
        self.show_scrollbar(p) && self.scrollbars_of(p).auto_hide()
    }

    /// `window_pane_scrollbar_visible`.
    #[must_use]
    pub fn scrollbar_visible(&self, p: PaneId) -> bool {
        if !self.show_scrollbar(p) {
            return false;
        }
        !self.scrollbars_of(p).auto_hide() || self.panes.get(&p).is_some_and(|x| x.sb.visible)
    }

    /// The scrollbar as `redraw_mark_pane` places it, if one is drawn.
    #[must_use]
    pub fn scrollbar_placement(&self, p: PaneId) -> Option<Placement> {
        if !self.scrollbar_visible(p) {
            return None;
        }
        let pane = self.panes.get(&p)?;
        let style = self.scrollbar_style(p);
        let overlay = self.scrollbar_overlay(p);
        let mut width = style.width + style.pad;
        if overlay && width > pane.rect.sx {
            width = style.width.min(pane.rect.sx);
        }
        (width != 0).then(|| Placement {
            width,
            left: self.scrollbar_left(pane.window),
            overlay,
        })
    }

    /// `layout_fix_panes` (layout.c:451-473): a reserved scrollbar takes
    /// its columns from the pane's cell `rect`, leaving the pane at least
    /// `PANE_MINIMUM` wide.
    #[must_use]
    pub fn reserve_scrollbar(&self, p: PaneId, mut rect: Geometry) -> Geometry {
        if !self.scrollbar_reserve(p) {
            return rect;
        }
        let Some(pane) = self.panes.get(&p) else {
            return rect;
        };
        let style = self.scrollbar_style(p);
        let total = style.width + style.pad;
        let min = vtmux_core::layout::PANE_MINIMUM;
        let fits = i64::from(rect.sx) - i64::from(total) >= i64::from(min);
        if self.scrollbar_left(pane.window) {
            if fits {
                rect.sx -= total;
                rect.xoff += i32::try_from(total).unwrap_or(0);
            } else {
                rect.xoff += i32::try_from(rect.sx).unwrap_or(0) - i32::try_from(min).unwrap_or(0);
                rect.sx = min;
            }
        } else if fits {
            rect.sx -= total;
        } else {
            rect.sx = min;
        }
        rect
    }

    /// `window_pane_full_size_offset` (window.c:2194-2212): the pane with
    /// its reserved scrollbar, as `(xoff, sx)`.
    #[must_use]
    pub fn full_columns(&self, p: PaneId) -> Option<(i64, i64)> {
        let pane = self.panes.get(&p)?;
        let (xoff, sx) = (i64::from(pane.rect.xoff), i64::from(pane.rect.sx));
        if !self.scrollbar_reserve(p) {
            return Some((xoff, sx));
        }
        let style = self.scrollbar_style(p);
        let w = i64::from(style.width + style.pad);
        Some(if self.scrollbar_left(pane.window) {
            (xoff - w, sx + w)
        } else {
            (xoff, sx + w)
        })
    }

    /// `redraw_draw_scrollbar_span`'s slider (screen-redraw.c:1304-1331):
    /// its first row and height in a bar as tall as the pane. Without a
    /// mode it shows the screen within its history; in copy or view mode
    /// the view within the mode's history.
    #[must_use]
    pub fn scrollbar_slider(&self, p: PaneId) -> (u32, u32) {
        let Some(pane) = self.panes.get(&p) else {
            return (0, 0);
        };
        slider(pane, pane.rect.sy)
    }

    /// `window_pane_scrollbar_start_timer`: an auto-hidden bar that is
    /// shown hides `pane-scrollbars-timeout` milliseconds from now.
    pub fn scrollbar_start_timer(&mut self, p: PaneId, now: Instant) {
        let Some(pane) = self.panes.get(&p) else {
            return;
        };
        if !self.scrollbars(pane.window).auto_hide() || !pane.sb.visible {
            return;
        }
        let ms = self.number_option(TreeId::Window(pane.window), "pane-scrollbars-timeout");
        let at = now + Duration::from_millis(u64::try_from(ms).unwrap_or(0));
        if let Some(pane) = self.panes.get_mut(&p) {
            pane.sb.hide_at = Some(at);
        }
    }

    /// `window_pane_scrollbar_show`: show an auto-hidden bar, with the
    /// hide timer started again if `start_timer`.
    pub fn scrollbar_show(&mut self, p: PaneId, start_timer: bool, now: Instant) {
        if !self.scrollbars_of(p).auto_hide() || !self.show_scrollbar(p) {
            return;
        }
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        let changed = !pane.sb.visible;
        pane.sb.visible = true;
        pane.sb.hide_at = None;
        if start_timer {
            self.scrollbar_start_timer(p, now);
        }
        if changed {
            self.scrollbar_redraw(p);
        }
    }

    /// `window_pane_scrollbar_hide`.
    pub fn scrollbar_hide(&mut self, p: PaneId) {
        let Some(pane) = self.panes.get_mut(&p) else {
            return;
        };
        pane.sb.hide_at = None;
        pane.sb.hover = false;
        if !pane.sb.visible {
            return;
        }
        pane.sb.visible = false;
        self.scrollbar_redraw(p);
    }

    /// `window_pane_scrollbar_redraw_visibility`: the window again.
    fn scrollbar_redraw(&mut self, p: PaneId) {
        if let Some(w) = self.panes.get(&p).map(|x| x.window) {
            self.redraw_panes.insert(p);
            self.redraw(Redraw::Window(w));
        }
    }

    /// When the soonest auto-hide timer runs out.
    #[must_use]
    pub fn scrollbar_deadline(&self) -> Option<Instant> {
        self.panes.values().filter_map(|p| p.sb.hide_at).min()
    }

    /// `window_pane_scrollbar_timer` for every pane whose timer ran out.
    pub fn scrollbar_timers(&mut self, now: Instant) {
        let due: Vec<PaneId> = self
            .panes
            .values()
            .filter(|p| p.sb.hide_at.is_some_and(|t| t <= now))
            .map(|p| p.id)
            .collect();
        for p in due {
            if let Some(pane) = self.panes.get_mut(&p) {
                pane.sb.hover = false;
            }
            self.scrollbar_hide(p);
        }
    }

    /// `server_client_in_scrollbar_area` (server-client.c:658-684): window
    /// position `(x, y)` is on pane `p`'s auto-hide bar, shown or not.
    #[must_use]
    pub fn in_scrollbar_area(&self, p: PaneId, x: i64, y: i64) -> bool {
        if !self.scrollbar_overlay(p) {
            return false;
        }
        let Some(pane) = self.panes.get(&p) else {
            return false;
        };
        let r = pane.rect;
        let (xoff, yoff) = (i64::from(r.xoff), i64::from(r.yoff));
        let (sx, sy) = (i64::from(r.sx), i64::from(r.sy));
        if y < yoff || y >= yoff + sy {
            return false;
        }
        let style = self.scrollbar_style(p);
        let mut total = i64::from(style.width + style.pad);
        if total == 0 || total > sx {
            total = sx;
        }
        let (start, end) = if self.scrollbar_left(pane.window) {
            (xoff, xoff + total - 1)
        } else {
            (xoff + sx - total, xoff + sx - 1)
        };
        (start..=end).contains(&x)
    }

    /// `server_client_update_scrollbar_hover` (server-client.c:686-708) for
    /// a motion at window position `at` in window `w` (`None`: off the
    /// window): the bar under it shows, every other one's timer restarts.
    pub fn scrollbar_hover(&mut self, w: WindowId, at: Option<(i64, i64)>, now: Instant) {
        let Some(window) = self.windows.get(&w) else {
            return;
        };
        let panes: Vec<PaneId> = window
            .panes
            .panes
            .iter()
            .copied()
            .filter(|&p| window.is_visible(p))
            .collect();
        for p in panes {
            let on = at.is_some_and(|(x, y)| self.in_scrollbar_area(p, x, y));
            if let Some(pane) = self.panes.get_mut(&p) {
                pane.sb.hover = on;
            }
            if on {
                self.scrollbar_show(p, true, now);
            } else {
                self.scrollbar_start_timer(p, now);
            }
        }
    }
}

/// [`Model::scrollbar_slider`] for a bar `sb_h` rows tall.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    reason = "tmux's double arithmetic, truncated to u_int"
)]
fn slider(pane: &crate::model::Pane, sb_h: u32) -> (u32, u32) {
    let h = f64::from(sb_h);
    let (slider_h, slider_y) = match pane.modes.first() {
        Some(m) if let Some(copy) = m.as_copy() => {
            let (offset, size) = copy.current_offset();
            let total = size + sb_h as usize;
            if total == 0 {
                return (0, 0);
            }
            let pct = h / total as f64;
            let sh = (h * pct) as u32;
            let sy = (f64::from(sb_h + 1) * (offset as f64 / total as f64)) as u32;
            (sh, sy)
        }
        // A mode's own screen has no history: the slider fills the bar.
        Some(_) => (sb_h, 0),
        None => {
            let total = usize::from(pane.term.rows()) + pane.term.history_size();
            if total == 0 {
                return (0, 0);
            }
            let pct = h / total as f64;
            let sh = (h * pct) as u32;
            (sh, sb_h.wrapping_sub(sh))
        }
    };
    let slider_h = slider_h.max(1);
    let slider_y = if slider_y >= sb_h {
        sb_h.saturating_sub(1)
    } else {
        slider_y
    };
    (slider_y, slider_h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vtmux_core::colour::Colour;

    #[test]
    fn style_defaults_and_fallback() {
        let s = bar_style("bg=black,fg=white,width=2,pad=1");
        assert_eq!((s.width, s.pad), (2, 1));
        assert_eq!((s.cell.fg, s.cell.bg), (Colour::Ansi(7), Colour::Ansi(0)));
        // width=0 is at least 1; a bad style is the default.
        assert_eq!(bar_style("width=0").width, 1);
        let bad = bar_style("nonsense=1");
        assert_eq!((bad.width, bad.pad), (1, 0));
        assert_eq!(bad, bar_style(""));
    }
}
