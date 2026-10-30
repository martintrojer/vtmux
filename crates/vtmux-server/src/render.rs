//! Compose the final picture before diffing it (D14). Never stream pane
//! output directly: overlays and status are in the same snapshot.
use std::fmt::Write as _;
use vtmux_core::protocol::Hello;
use vtmux_vt::{Cell, Color, Snapshot, Style, Underline};

/// M0 terminfo-free table for the three supported terminal families. Full
/// overrides, theme detection and extended styles are M1 (D8).
#[expect(clippy::struct_excessive_bools, reason = "independent terminal capabilities")]
#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub rgb: bool,
    pub palette: bool,
    pub theme_palette: bool,
    pub sync: bool,
    pub bce: bool,
    pub xterm: bool,
}
impl Caps {
    #[must_use]
    pub fn from_hello(hello: &Hello) -> Option<Self> {
        let tty = hello.tty.as_ref()?;
        let xterm = tty.term.starts_with("xterm");
        if !xterm
            && !matches!(
                tty.term.as_str(),
                "tmux-256color" | "screen" | "screen-256color"
            )
        {
            return None;
        }
        let color = hello
            .env
            .iter()
            .find(|(k, _)| k == "COLORTERM")
            .map_or("", |(_, v)| v.as_str());
        let feature = |s: &str| {
            tty.features
                .iter()
                .any(|f| f.split([',', ':']).any(|x| x == s))
        };
        let rgb = color.eq_ignore_ascii_case("truecolor") || color == "24bit" || feature("RGB");
        let theme_palette = rgb || color.contains("256") || feature("256");
        Some(Self {
            rgb,
            palette: tty.term.contains("256") || theme_palette,
            theme_palette,
            sync: feature("sync"),
            bce: xterm,
            xterm,
        })
    }
    #[must_use]
    pub fn startup(self) -> Vec<u8> {
        let title = if self.xterm { "\x1b[22;0;0t" } else { "" };
        format!("\x1b[?1049h{title}\x1b[H\x1b[2J\x1b[?1h\x1b=\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004h\x1b[?2031h\x1b[?2026$p").into_bytes()
    }
    #[must_use]
    pub fn teardown(self) -> Vec<u8> {
        let title = if self.xterm { "\x1b[23;0;0t" } else { "" };
        format!("\x1b[r\x1b[0m\x1b[?1l\x1b>\x1b[H\x1b[2J\x1b[?12l\x1b[?25h\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?1005l\x1b[?2004l\x1b[?1004l\x1b[?1049l{title}\x1b[?2031l").into_bytes()
    }
    /// `server_client_update_theme_colours` + `format_cb_client_colours`: a
    /// plain xterm-256color reports 16 unless the 256/RGB feature is set.
    #[must_use]
    pub fn status_style(self) -> Style {
        Style {
            fg: if self.theme_palette {
                Color::Rgb(13, 13, 13)
            } else {
                Color::Palette(0)
            },
            bg: if self.theme_palette {
                Color::Rgb(154, 205, 50)
            } else {
                Color::Palette(2)
            },
            ..Style::default()
        }
    }
}

#[derive(Default)]
pub struct Compositor {
    last: Option<Snapshot>,
}
impl Compositor {
    pub fn invalidate(&mut self) {
        self.last = None;
    }
    /// One frame per tick. The caller supplies a fully composed screen;
    /// therefore changes behind opaque overlays cannot escape into a frame.
    pub fn frame(&mut self, screen: Snapshot, caps: Caps) -> Vec<u8> {
        if self.last.as_ref() == Some(&screen) {
            return Vec::new();
        }
        let mut out = String::new();
        if caps.sync {
            out.push_str("\x1b[?2026h");
        }
        out.push_str("\x1b[?25l");
        let same_size = self
            .last
            .as_ref()
            .is_some_and(|s| (s.cols, s.rows) == (screen.cols, screen.rows));
        let mut style = Style::default();
        out.push_str("\x1b[0m");
        for y in 0..screen.rows {
            let mut x = 0;
            while x < screen.cols {
                let cell = screen.cell(x, y).expect("screen cell");
                let changed =
                    !same_size || self.last.as_ref().and_then(|s| s.cell(x, y)) != Some(cell);
                if !changed || cell.width == 0 {
                    x += 1;
                    continue;
                }
                let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
                // tmux tty_draw_line clears long empty runs with BCE: the
                // erased cells retain background, not foreground. Keep this
                // observable distinction for the status gap (oracle trace).
                let blank = cell.text.is_empty() || cell.text == " ";
                if blank
                    && cell.style == Style::default()
                    && (x..screen.cols).all(|cx| {
                        screen.cell(cx, y).is_some_and(|c| {
                            (c.text.is_empty() || c.text == " ") && c.style == cell.style
                        })
                    })
                    && caps.bce
                {
                    set_style(&mut out, &mut style, cell.style, caps);
                    out.push_str("\x1b[K");
                    break;
                }
                set_style(&mut out, &mut style, cell.style, caps);
                out.push_str(if cell.text.is_empty() {
                    " "
                } else {
                    &cell.text
                });
                x += u16::from(cell.width.max(1));
            }
        }
        out.push_str("\x1b[0m");
        if let Some((x, y)) = screen.cursor {
            let _ = write!(out, "\x1b[{};{}H", y + 1, x + 1);
        }
        if screen.cursor_visible {
            out.push_str("\x1b[?25h");
        }
        if caps.sync {
            out.push_str("\x1b[?2026l");
        }
        self.last = Some(screen);
        out.into_bytes()
    }
}
fn set_style(out: &mut String, current: &mut Style, wanted: Style, caps: Caps) {
    if *current == wanted {
        return;
    }
    out.push_str("\x1b[0m");
    for (on, code) in [
        (wanted.bold, 1),
        (wanted.faint, 2),
        (wanted.italic, 3),
        (wanted.underline != Underline::None, 4),
        (wanted.blink, 5),
        (wanted.inverse, 7),
        (wanted.invisible, 8),
        (wanted.strikethrough, 9),
    ] {
        if on {
            let _ = write!(out, "\x1b[{code}m");
        }
    }
    color(out, wanted.fg, false, caps);
    color(out, wanted.bg, true, caps);
    *current = wanted;
}
fn color(out: &mut String, c: Color, bg: bool, caps: Caps) {
    let base = if bg { 40 } else { 30 };
    match c {
        Color::Default => {}
        Color::Palette(n) if n < 8 => {
            let _ = write!(out, "\x1b[{}m", base + u16::from(n));
        }
        Color::Palette(n) if n < 16 => {
            let _ = write!(out, "\x1b[{}m", base + 60 + u16::from(n) - 8);
        }
        Color::Palette(n) => {
            let n = if caps.palette { n } else { n % 8 };
            let _ = write!(out, "\x1b[{};5;{n}m", base + 8);
        }
        Color::Rgb(r, g, b) if caps.rgb => {
            let _ = write!(out, "\x1b[{};2;{r};{g};{b}m", base + 8);
        }
        Color::Rgb(r, g, b) => color(out, Color::Palette(rgb_palette(r, g, b)), bg, caps),
    }
}
#[allow(clippy::many_single_char_names)] // RGB components and cube coordinates
fn rgb_palette(r: u8, g: u8, b: u8) -> u8 {
    // colour.c colour_find_rgb: closest cube or grey.
    let cube = |v: u8| {
        if v < 48 {
            0
        } else if v < 114 {
            1
        } else {
            (v - 35) / 40
        }
    };
    let q = [cube(r), cube(g), cube(b)];
    let levels = [0i32, 95, 135, 175, 215, 255];
    let dist = |red: i32, green: i32, blue: i32| {
        (red - i32::from(r)).pow(2) + (green - i32::from(g)).pow(2) + (blue - i32::from(b)).pow(2)
    };
    let avg = (i32::from(r) + i32::from(g) + i32::from(b)) / 3;
    let grey = ((avg - 3) / 10).clamp(0, 23);
    let v = 8 + 10 * grey;
    if dist(v, v, v)
        < dist(
            levels[q[0] as usize],
            levels[q[1] as usize],
            levels[q[2] as usize],
        )
    {
        232 + u8::try_from(grey).unwrap()
    } else {
        16 + 36 * q[0] + 6 * q[1] + q[2]
    }
}

/// Put the default status directly on the bottom row. Full status-format is
/// M1. Limits/order: options-table.c status-left/right/window-status defaults.
pub fn status(
    screen: &mut Snapshot,
    left: &str,
    windows: &[(String, bool)],
    right: &str,
    caps: Caps,
) {
    use unicode_width::UnicodeWidthChar;
    let y = screen.rows - 1;
    let cols = usize::from(screen.cols);
    let base = caps.status_style();
    for x in 0..screen.cols {
        screen.cells[usize::from(y) * cols + usize::from(x)] = Cell {
            text: String::new(),
            width: 1,
            style: Style {
                fg: Color::Default,
                ..base
            },
        };
    }
    let right_width = vtmux_core::format::trim::width(right).min(cols);
    let left = vtmux_core::format::trim::trim_left(left, 10.min(cols.saturating_sub(right_width)));
    let left_width = vtmux_core::format::trim::width(&left);
    let mut put = |start: usize, text: &str, style: Style, limit: usize| {
        let mut x = start;
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if w == 0 {
                if x > start {
                    screen.cells[usize::from(y) * cols + x - 1].text.push(c);
                }
                continue;
            }
            if x + w > limit {
                break;
            }
            screen.cells[usize::from(y) * cols + x] = Cell {
                text: c.to_string(),
                width: u8::try_from(w).unwrap_or(1),
                style,
            };
            if w == 2 {
                screen.cells[usize::from(y) * cols + x + 1] = Cell {
                    text: String::new(),
                    width: 0,
                    style,
                };
            }
            x += w;
        }
    };
    put(0, &left, base, cols);
    let mut x = left_width;
    for (text, current) in windows {
        let style = Style {
            underline: if *current {
                Underline::Single
            } else {
                Underline::None
            },
            ..base
        };
        put(x, text, style, cols.saturating_sub(right_width));
        x += vtmux_core::format::trim::width(text);
    }
    put(cols - right_width, right, base, cols);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn diff_and_sync_are_one_final_picture() {
        let caps = Caps {
            rgb: false,
            palette: true,
            theme_palette: false,
            sync: true,
            bce: true,
            xterm: true,
        };
        let mut pane = vtmux_vt::PaneTerm::new(4, 2, 0);
        pane.feed(b"abc");
        let mut screen = vtmux_vt::snapshot(&pane);
        screen.cells[0].text = "O".into(); // opaque overlay
        let mut compositor = Compositor::default();
        let frame = compositor.frame(screen.clone(), caps);
        assert!(frame.starts_with(b"\x1b[?2026h\x1b[?25l"));
        assert!(frame.ends_with(b"\x1b[?2026l"));
        assert!(!frame.windows(3).any(|x| x == b"abc"));
        assert!(compositor.frame(screen.clone(), caps).is_empty());
        screen.cells[1].text = "X".into();
        let frame = compositor.frame(screen, caps);
        assert!(frame.contains(&b'X'));
        assert!(!frame.contains(&b'O'));
    }
}
