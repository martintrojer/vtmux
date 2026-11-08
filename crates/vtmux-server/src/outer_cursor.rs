//! The cursor style and colour of a client's terminal (tty.c
//! `tty_update_cursor`, `tty_force_cursor_colour`): what the screen the
//! client looks at asked for (a pane's DECSCUSR and OSC 12, else its
//! `cursor-style` and `cursor-colour`), written as the terminal's `Ss`,
//! `Se`, `cvvis`, `Cs` and `Cr` when it changes.

use vtmux_core::options::TreeId;
use vtmux_core::term_features::{Param, Term};
use vtmux_vt::CursorShape;

/// What a screen says of its cursor: tmux's `s->cstyle`, `s->ccolour`,
/// the cursor bits of `s->mode` and the defaults `screen_set_default_cursor`
/// (or `prompt_draw`) gives it.
#[expect(clippy::struct_excessive_bools, reason = "tmux's mode bits")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CursorScreen {
    pub shape: CursorShape,
    /// `MODE_CURSOR_BLINKING`, `MODE_CURSOR_BLINKING_SET`,
    /// `MODE_CURSOR_VERY_VISIBLE`.
    pub blinking: bool,
    pub blinking_set: bool,
    pub very_visible: bool,
    /// `s->default_cstyle` and `MODE_CURSOR_BLINKING` of
    /// `s->default_mode`.
    pub default_shape: CursorShape,
    pub default_blinking: bool,
    /// `s->ccolour`, `s->default_ccolour`: `None` is -1.
    pub colour: Option<(u8, u8, u8)>,
    pub default_colour: Option<(u8, u8, u8)>,
}

impl CursorScreen {
    /// The defaults of a `cursor-style` value (`screen_set_cursor_style`,
    /// screen.c:220-251): 0 default, then block, underline and bar, each
    /// blinking (odd) and steady (even).
    pub fn set_default_style(&mut self, style: i64) {
        (self.default_shape, self.default_blinking) = match style {
            1 => (CursorShape::Block, true),
            2 => (CursorShape::Block, false),
            3 => (CursorShape::Underline, true),
            4 => (CursorShape::Underline, false),
            5 => (CursorShape::Bar, true),
            6 => (CursorShape::Bar, false),
            _ => (CursorShape::Default, false),
        };
    }
}

/// The cursor bits of `tty->mode` that `tty_update_cursor` compares
/// (`CURSOR_MODES`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mode {
    visible: bool,
    blinking: bool,
    very_visible: bool,
}

/// What the terminal was last told: `tty->mode`'s cursor bits (`None`
/// after `tty_invalidate`, which sets every mode), `tty->cstyle` and
/// `tty->ccolour`.
#[derive(Clone, Copy, Debug, Default)]
pub struct OuterCursor {
    mode: Option<Mode>,
    shape: CursorShape,
    colour: Option<(u8, u8, u8)>,
}

impl OuterCursor {
    /// `tty_invalidate`: the next update writes the cursor again.
    pub fn invalidate(&mut self) {
        self.mode = None;
    }

    /// `tty_start_tty`: the colour the terminal was given is gone (the
    /// teardown reset it).
    pub fn restart(&mut self) {
        self.colour = None;
        self.mode = None;
    }

    /// `tty_update_cursor` for a frame whose cursor is `visible`, showing
    /// screen `s` (`None`: no screen, as for an overlay without one).
    /// Returns the bytes for the terminal.
    pub fn update(&mut self, term: &Term, visible: bool, s: Option<&CursorScreen>) -> String {
        let mut out = String::new();
        if let Some(s) = s {
            self.force_colour(term, s.colour.or(s.default_colour), &mut out);
        }
        let blinking = s.is_some_and(|s| s.blinking);
        let very_visible = s.is_some_and(|s| s.very_visible);
        if !visible {
            // The frame hides the cursor itself (`civis`).
            self.mode = Some(Mode {
                visible,
                blinking,
                very_visible,
            });
            return out;
        }
        let (shape, blinking) = match s {
            None => (self.shape, blinking),
            Some(s) if s.shape == CursorShape::Default => {
                let blinking = if s.blinking_set {
                    blinking
                } else {
                    s.default_blinking
                };
                (s.default_shape, blinking)
            }
            Some(s) => (s.shape, blinking),
        };
        let mode = Mode {
            visible,
            blinking,
            very_visible,
        };
        if self.mode == Some(mode) && shape == self.shape {
            return out;
        }
        let code = |name: &str, p: &[Param]| term.code(name, p).unwrap_or_default();
        out.push_str(&code("cnorm", &[]));
        let ss = |n: i64| code("Ss", &[Param::Int(n)]);
        let steady = |n: i64| if blinking { n } else { n + 1 };
        match shape {
            CursorShape::Default => {
                if self.shape != CursorShape::Default {
                    out.push_str(&reset_style(term));
                }
                if blinking || very_visible {
                    out.push_str(&code("cvvis", &[]));
                }
            }
            CursorShape::Block | CursorShape::Underline | CursorShape::Bar => {
                let n = match shape {
                    CursorShape::Block => 1,
                    CursorShape::Underline => 3,
                    _ => 5,
                };
                if term.has_name("Ss") {
                    out.push_str(&ss(steady(n)));
                } else if blinking {
                    out.push_str(&code("cvvis", &[]));
                }
            }
        }
        self.shape = shape;
        self.mode = Some(mode);
        out
    }

    /// `tty_force_cursor_colour`: `Cs` with the colour as `rgb:rr/gg/bb`,
    /// or `Cr` for none, when it is not what the terminal has.
    fn force_colour(&mut self, term: &Term, colour: Option<(u8, u8, u8)>, out: &mut String) {
        if colour == self.colour {
            return;
        }
        let code = match colour {
            None => term.code("Cr", &[]),
            Some((r, g, b)) => {
                let spec = format!("rgb:{r:02x}/{g:02x}/{b:02x}");
                term.code("Cs", &[Param::Str(&spec)])
            }
        };
        out.push_str(&code.unwrap_or_default());
        self.colour = colour;
    }

    /// The cursor part of `tty_stop_tty` (tty.c:489-496): the style back
    /// to the terminal's default, and its colour.
    #[must_use]
    pub fn teardown(&self, term: &Term) -> String {
        let mut out = String::new();
        if self.shape != CursorShape::Default {
            out.push_str(&reset_style(term));
        }
        if self.colour.is_some() {
            out.push_str(&term.code("Cr", &[]).unwrap_or_default());
        }
        out
    }
}

/// `Se`, else `Ss` with 0.
fn reset_style(term: &Term) -> String {
    term.code("Se", &[])
        .or_else(|| term.code("Ss", &[Param::Int(0)]))
        .unwrap_or_default()
}

use crate::server::ClientId;

impl crate::server::Server {
    /// The cursor of the screen client `id` looks at, as
    /// `server_client_reset_state` (server-client.c:2197-2207) picks it:
    /// a popup's, a menu's, the active pane's (its mode's while it has
    /// one), or the status line's under a prompt.
    fn cursor_screen(&self, id: ClientId) -> Option<CursorScreen> {
        let c = self.clients.get(&id)?;
        let s = c.attached?;
        let (_, w, active) = self.model.current_of(s)?;
        let from_term = |t: &vtmux_vt::PaneTerm, defaults: TreeId| {
            let m = t.cursor_style();
            let mut screen = CursorScreen {
                shape: m.shape,
                blinking: m.blinking,
                blinking_set: m.blinking_set,
                very_visible: m.very_visible,
                colour: t.cursor_colour(),
                default_colour: self.colour_option(defaults, "cursor-colour"),
                ..CursorScreen::default()
            };
            screen.set_default_style(self.model.number_option(defaults, "cursor-style"));
            screen
        };
        if let Some(p) = c.popup.as_ref() {
            // popup.c:624: the global window options.
            return Some(from_term(&p.term, TreeId::GlobalWindow));
        }
        if self.model.windows.get(&w).is_some_and(|x| x.menu.is_some()) {
            return Some(CursorScreen::default());
        }
        if let Some(prompt) = c.status.prompt() {
            // prompt_set_options and prompt_draw: the status line's screen
            // takes the prompt's cursor.
            let tree = TreeId::Session(s);
            let (style, colour) = if prompt.editor.command_mode() {
                (
                    "prompt-command-cursor-style",
                    "prompt-command-cursor-colour",
                )
            } else {
                ("prompt-cursor-style", "prompt-cursor-colour")
            };
            let mut screen = CursorScreen {
                default_colour: self.colour_option(tree, colour),
                ..CursorScreen::default()
            };
            screen.set_default_style(self.model.number_option(tree, style));
            return Some(screen);
        }
        let pane = self.model.panes.get(&active)?;
        match pane.modes.first().map(crate::mode::PaneMode::name) {
            // window-copy.c:590-591: copy and view mode's screen takes the
            // global window options; other modes' screen_init nothing.
            Some("copy-mode" | "view-mode") => {
                let mut screen = CursorScreen {
                    default_colour: self.colour_option(TreeId::GlobalWindow, "cursor-colour"),
                    ..CursorScreen::default()
                };
                screen.set_default_style(
                    self.model
                        .number_option(TreeId::GlobalWindow, "cursor-style"),
                );
                Some(screen)
            }
            Some(_) => Some(CursorScreen::default()),
            None => Some(from_term(&pane.term, TreeId::Pane(active))),
        }
    }

    /// `tty_update_cursor` for client `id` after a frame: the bytes for a
    /// change of its terminal's cursor style or colour.
    pub(crate) fn outer_cursor_bytes(&mut self, id: ClientId) -> Vec<u8> {
        let screen = self.cursor_screen(id);
        let Some(c) = self.clients.get_mut(&id) else {
            return Vec::new();
        };
        let Some(term) = c.term.as_ref() else {
            return Vec::new();
        };
        let visible = c.compositor.cursor_shown();
        c.cursor.update(term, visible, screen.as_ref()).into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(name: &str) -> Term {
        let features = vec!["xterm*:clipboard:ccolour:cstyle:focus:title".to_owned()];
        Term::open(name, &[], None, &features, &[]).unwrap()
    }

    fn bar(blinking: bool) -> CursorScreen {
        CursorScreen {
            shape: CursorShape::Bar,
            blinking,
            ..CursorScreen::default()
        }
    }

    /// The writes tmux makes on xterm-256color (reference probe): a bar
    /// from DECSCUSR 5 is `cnorm` then `Ss` 5, once; a colour is `Cs`
    /// once; back to the default is `Se` and `Cr`.
    #[test]
    fn style_and_colour_are_written_once_per_change() {
        let t = term("xterm-256color");
        let mut c = OuterCursor::default();
        let mut s = bar(true);
        s.colour = Some((255, 0, 0));
        assert_eq!(
            c.update(&t, true, Some(&s)),
            "\x1b]12;rgb:ff/00/00\x07\x1b[?12l\x1b[?25h\x1b[5 q"
        );
        assert_eq!(c.update(&t, true, Some(&s)), "");
        // Hidden, then shown again: tmux writes the style again.
        assert_eq!(c.update(&t, false, Some(&s)), "");
        assert_eq!(c.update(&t, true, Some(&s)), "\x1b[?12l\x1b[?25h\x1b[5 q");
        assert_eq!(c.teardown(&t), "\x1b[2 q\x1b]112\x07");
        let plain = CursorScreen::default();
        assert_eq!(
            c.update(&t, true, Some(&plain)),
            "\x1b]112\x07\x1b[?12l\x1b[?25h\x1b[2 q"
        );
        assert_eq!(c.teardown(&t), "");
        // tty_invalidate: the next update writes the style again.
        c.update(&t, true, Some(&bar(false)));
        c.invalidate();
        assert_eq!(
            c.update(&t, true, Some(&bar(false))),
            "\x1b[?12l\x1b[?25h\x1b[6 q"
        );
    }

    /// A screen without a style of its own takes its default
    /// (`cursor-style`), blinking as the default says unless the program
    /// set it; Ghostty's `Se` is its own.
    #[test]
    fn the_default_style_and_the_terminal_s_reset() {
        let t = term("xterm-ghostty");
        let mut c = OuterCursor::default();
        let mut s = CursorScreen::default();
        s.set_default_style(3);
        assert_eq!(c.update(&t, true, Some(&s)), "\x1b[?12l\x1b[?25h\x1b[3 q");
        s.blinking_set = true;
        assert_eq!(c.update(&t, true, Some(&s)), "\x1b[?12l\x1b[?25h\x1b[4 q");
        s.set_default_style(0);
        assert_eq!(c.update(&t, true, Some(&s)), "\x1b[?12l\x1b[?25h\x1b[0 q");
        // No style, blinking: `cvvis`.
        s.blinking = true;
        assert_eq!(
            c.update(&t, true, Some(&s)),
            "\x1b[?12l\x1b[?25h\x1b[?12;25h"
        );
    }
}
