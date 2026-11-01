//! Terminal features and overrides: which of `terminal-features`,
//! `terminal-overrides`, the client's `-T` / `-2` flags and `COLORTERM` apply
//! to a client's terminal, as tty-features.c and tty-term.c decide them.
//!
//! vtmux has no terminfo model (D8), so [`Term`] tracks the capabilities
//! that reach what vtmux draws: [`Cap`]. Other override names are accepted
//! and ignored (D-007).

use crate::fnmatch;

/// A named terminal feature (tty-features.c `tty_features`). The table order
/// is the order `#{client_termfeatures}` lists them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Colours256,
    AppEsc,
    BracketedPaste,
    CursorColour,
    Clipboard,
    Hyperlinks,
    CursorStyle,
    ExtendedKeys,
    Focus,
    IgnoreFunctionKeys,
    Margins,
    Mouse,
    Osc7,
    Overline,
    ProgressBar,
    RectFill,
    Rgb,
    Sixel,
    Strikethrough,
    Sync,
    Title,
    UnderlineStyles,
    Utf8,
}

/// Every feature in table order, with its name.
const FEATURES: [(Feature, &str); 23] = [
    (Feature::Colours256, "256"),
    (Feature::AppEsc, "appesc"),
    (Feature::BracketedPaste, "bpaste"),
    (Feature::CursorColour, "ccolour"),
    (Feature::Clipboard, "clipboard"),
    (Feature::Hyperlinks, "hyperlinks"),
    (Feature::CursorStyle, "cstyle"),
    (Feature::ExtendedKeys, "extkeys"),
    (Feature::Focus, "focus"),
    (Feature::IgnoreFunctionKeys, "ignorefkeys"),
    (Feature::Margins, "margins"),
    (Feature::Mouse, "mouse"),
    (Feature::Osc7, "osc7"),
    (Feature::Overline, "overline"),
    (Feature::ProgressBar, "progressbar"),
    (Feature::RectFill, "rectfill"),
    (Feature::Rgb, "RGB"),
    (Feature::Sixel, "sixel"),
    (Feature::Strikethrough, "strikethrough"),
    (Feature::Sync, "sync"),
    (Feature::Title, "title"),
    (Feature::UnderlineStyles, "usstyle"),
    (Feature::Utf8, "utf8"),
];

impl Feature {
    fn bit(self) -> u32 {
        1 << FEATURES.iter().position(|(f, _)| *f == self).unwrap_or(0)
    }

    /// The capabilities of `tty_feature_*_capabilities` that [`Cap`] models.
    fn caps(self) -> &'static [Cap] {
        match self {
            Self::Clipboard => &[Cap::Ms],
            Self::CursorStyle => &[Cap::Ss, Cap::Se],
            Self::Rgb => &[Cap::SetRgbF, Cap::SetRgbB],
            Self::Sync => &[Cap::Sync],
            Self::Title => &[Cap::Tsl, Cap::Fsl],
            Self::Osc7 => &[Cap::Fsl],
            Self::UnderlineStyles => &[Cap::Smulx, Cap::Setulc],
            _ => &[],
        }
    }
}

/// The features of one client: `c->term_features` and `c->term_nofeatures`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Features {
    enabled: u32,
    /// Features a trailing `@` removed from a list that tracks removals; they
    /// cannot be added again by a later list.
    disabled: u32,
}

impl Features {
    /// `tty_parse_features`. Names are case-insensitive; the first unknown
    /// name ends the list; a trailing `@` removes a feature. With `track`
    /// (`disabled != NULL`) a removal also bars later additions.
    pub fn parse(&mut self, list: &str, seps: &[char], track: bool) {
        for item in list.split(|c| seps.contains(&c)) {
            let (name, remove) = match item.strip_suffix('@') {
                Some(name) => (name, true),
                None => (item, false),
            };
            let Some(&(feature, _)) = FEATURES
                .iter()
                .find(|(_, known)| known.eq_ignore_ascii_case(name))
            else {
                break;
            };
            let bit = feature.bit();
            if remove {
                self.enabled &= !bit;
                if track {
                    self.disabled |= bit;
                }
            } else if !(track && self.disabled & bit != 0) {
                self.enabled |= bit;
            }
        }
    }

    /// What the client sent: each `-T` argument (`-2` is `256`) parsed with
    /// `:` and `,` in order, as tmux.c does into `feat`.
    #[must_use]
    pub fn from_client(args: &[String]) -> Self {
        let mut features = Self::default();
        for arg in args {
            features.parse(arg, &[':', ','], false);
        }
        features
    }

    #[must_use]
    pub fn has(self, feature: Feature) -> bool {
        self.enabled & feature.bit() != 0
    }

    /// `tty_get_features`: the enabled names in table order, comma-joined.
    #[must_use]
    pub fn list(self) -> String {
        FEATURES
            .iter()
            .filter(|(f, _)| self.has(*f))
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// A terminal capability (`tty_term_codes`) that vtmux acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cap {
    /// `Tc` flag.
    Tc,
    /// `RGB` flag.
    Rgb,
    SetRgbF,
    SetRgbB,
    Sync,
    /// Styled underlines.
    Smulx,
    Setulc,
    /// Cursor style set and reset.
    Ss,
    Se,
    /// OSC 52 clipboard.
    Ms,
    /// Start and end of a title (`tsl`, `fsl`).
    Tsl,
    Fsl,
}

impl Cap {
    /// Every capability, in the order the goldens list them.
    pub const ALL: [Self; 12] = [
        Self::Tc,
        Self::Rgb,
        Self::SetRgbF,
        Self::SetRgbB,
        Self::Sync,
        Self::Smulx,
        Self::Setulc,
        Self::Ss,
        Self::Se,
        Self::Ms,
        Self::Tsl,
        Self::Fsl,
    ];

    /// The terminfo name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Tc => "Tc",
            Self::Rgb => "RGB",
            Self::SetRgbF => "setrgbf",
            Self::SetRgbB => "setrgbb",
            Self::Sync => "Sync",
            Self::Smulx => "Smulx",
            Self::Setulc => "Setulc",
            Self::Ss => "Ss",
            Self::Se => "Se",
            Self::Ms => "Ms",
            Self::Tsl => "tsl",
            Self::Fsl => "fsl",
        }
    }

    /// The value a feature gives a string capability
    /// (`tty_feature_*_capabilities`), escapes decoded.
    fn feature_value(self) -> Option<&'static str> {
        match self {
            Self::Ms => Some(MS),
            Self::Tsl => Some("\x1b]0;"),
            Self::Fsl => Some("\x07"),
            _ => None,
        }
    }

    fn bit(self) -> u16 {
        1 << Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    fn is_flag(self) -> bool {
        matches!(self, Self::Tc | Self::Rgb)
    }
}

/// `Ms` as terminfo (and the `clipboard` feature) gives it, escapes decoded.
const MS: &str = "\x1b]52;%p1%s;%p2%s\x07";

/// The terminals vtmux knows without terminfo (D8).
#[derive(Clone, Copy, Debug)]
struct Base {
    /// `colors#`.
    colours: u32,
    caps: u16,
    xterm: bool,
    /// terminfo's `tsl` (`fsl` is BEL where there is one).
    tsl: Option<&'static str>,
}

impl Base {
    fn of(name: &str) -> Option<Self> {
        let caps = |list: &[Cap]| list.iter().fold(0, |bits, c| bits | c.bit());
        let xterm_caps = [Cap::Ss, Cap::Se, Cap::Ms];
        if name == "xterm-ghostty" {
            // Ghostty 1.4's terminfo.
            return Some(Self {
                colours: 256,
                caps: caps(&[
                    Cap::Tc,
                    Cap::SetRgbF,
                    Cap::SetRgbB,
                    Cap::Sync,
                    Cap::Smulx,
                    Cap::Setulc,
                    Cap::Ss,
                    Cap::Se,
                    Cap::Ms,
                    Cap::Tsl,
                    Cap::Fsl,
                ]),
                xterm: true,
                tsl: Some("\x1b]2;"),
            });
        }
        if name.starts_with("xterm") {
            let direct = name.starts_with("xterm-direct");
            let colours = if direct {
                1 << 24
            } else if name.contains("256") {
                256
            } else if name.contains("88color") {
                88
            } else if name.contains("16color") {
                16
            } else if name.contains("mono") {
                2
            } else {
                8
            };
            return Some(Self {
                colours,
                caps: caps(&xterm_caps) | if direct { Cap::Rgb.bit() } else { 0 },
                xterm: true,
                tsl: None,
            });
        }
        let (colours, caps, tsl) = match name {
            "tmux-256color" => (
                256,
                caps(&[Cap::Smulx, Cap::Ss, Cap::Se, Cap::Ms, Cap::Tsl, Cap::Fsl]),
                Some("\x1b]0;"),
            ),
            "screen" => (8, 0, None),
            "screen-256color" => (256, 0, None),
            _ => return None,
        };
        Some(Self {
            colours,
            caps,
            xterm: false,
            tsl,
        })
    }
}

/// A client's terminal as `tty_term_create` leaves it.
#[derive(Clone, Debug)]
pub struct Term {
    name: String,
    base: Base,
    features: Features,
    /// Features whose capabilities have been applied (`applied_features`).
    applied: u32,
    caps: u16,
    ms: Option<String>,
    tsl: Option<String>,
    fsl: Option<String>,
    /// `TERM_256COLOURS`.
    colours256: bool,
    /// `TERM_RGBCOLOURS`.
    rgb: bool,
    overrides: Vec<String>,
}

impl Term {
    /// `tty_term_create`, when the terminal is one vtmux knows. `client` is
    /// what the client sent (`-T`, `-2`); `features` and `overrides` are the
    /// items of the `terminal-features` and `terminal-overrides` options;
    /// `colorterm` is `$COLORTERM` from the client's environment.
    #[must_use]
    pub fn open(
        name: &str,
        client: &[String],
        colorterm: Option<&str>,
        features: &[String],
        overrides: &[String],
    ) -> Option<Self> {
        let base = Base::of(name)?;
        let mut term = Self {
            name: name.to_owned(),
            base,
            features: Features::from_client(client),
            applied: 0,
            caps: base.caps,
            ms: (base.caps & Cap::Ms.bit() != 0).then(|| MS.to_owned()),
            tsl: base.tsl.map(str::to_owned),
            fsl: base.tsl.map(|_| "\x07".to_owned()),
            colours256: false,
            rgb: false,
            overrides: overrides.to_vec(),
        };
        for item in features {
            if let Some((pattern, rest)) = split_entry(item)
                && fnmatch::matches(&pattern, name, false)
            {
                term.features.parse(rest, &[':'], true);
            }
        }
        if let Some(value) = colorterm {
            if value.eq_ignore_ascii_case("truecolor") || value.eq_ignore_ascii_case("24bit") {
                term.features.parse("RGB", &[','], true);
            } else if value.contains("256") {
                term.features.parse("256", &[','], true);
            }
        }
        term.apply_overrides();
        // Every terminal vtmux knows is VT100-like.
        term.features.parse("bpaste,focus,title", &[','], true);
        if (term.has(Cap::Tc) || term.has(Cap::Rgb))
            && (!term.has(Cap::SetRgbF) || !term.has(Cap::SetRgbB))
        {
            term.features.parse("RGB", &[','], true);
        }
        if term.apply_features() {
            term.apply_overrides();
        }
        Some(term)
    }

    /// The terminal answered the synchronised-update query: `tty_keys_sync`.
    pub fn sync_supported(&mut self) {
        self.features.parse("sync", &[','], true);
        if self.apply_features() {
            self.apply_overrides();
        }
    }

    #[must_use]
    pub fn features(&self) -> Features {
        self.features
    }

    #[must_use]
    pub fn has(&self, cap: Cap) -> bool {
        self.caps & cap.bit() != 0
    }

    /// `tty_set_selection` (tty.c:2149-2166): `Ms` with the clipboard
    /// name and the base64 of `data` (`tty_putcode_ss`), or `None` when the
    /// terminal has no `Ms`.
    #[must_use]
    pub fn set_selection(&self, clip: &str, data: &[u8]) -> Option<Vec<u8>> {
        let ms = self.ms.as_deref().filter(|_| self.has(Cap::Ms))?;
        Some(expand_ss(ms, clip, &base64(data)))
    }

    /// `tty_clipboard_query` (tty.c:3206-3216): `Ms` with an empty
    /// clipboard name and `?`, or `None` when the terminal has no `Ms`.
    #[must_use]
    pub fn clipboard_query(&self) -> Option<Vec<u8>> {
        let ms = self.ms.as_deref().filter(|_| self.has(Cap::Ms))?;
        Some(expand_ss(ms, "", "?"))
    }

    /// `tty_set_title` (tty.c:743-752): `tsl`, the title and `fsl`, or
    /// `None` when the terminal lacks either.
    #[must_use]
    pub fn set_title(&self, title: &str) -> Option<Vec<u8>> {
        let tsl = self.tsl.as_deref().filter(|_| self.has(Cap::Tsl))?;
        let fsl = self.fsl.as_deref().filter(|_| self.has(Cap::Fsl))?;
        Some([tsl, title, fsl].concat().into_bytes())
    }

    /// The terminal is xterm-derived: it has `XT` and `bce` in terminfo.
    #[must_use]
    pub fn is_xterm(&self) -> bool {
        self.base.xterm
    }

    /// terminfo's `khome`/`kend` are `\E[1~`/`\E[4~` (screen, tmux)
    /// rather than xterm's `\EOH`/`\EOF`; tty-keys.c adds them to the key
    /// tree (`tty_default_code_keys`).
    #[must_use]
    pub fn tilde_home_end(&self) -> bool {
        !self.base.xterm
    }

    /// `TERM_RGBCOLOURS`.
    #[must_use]
    pub fn rgb(&self) -> bool {
        self.rgb
    }

    /// `TERM_256COLOURS`: the 256 or RGB feature is on.
    #[must_use]
    pub fn colours256(&self) -> bool {
        self.colours256
    }

    /// The colours `tty_check_fg` assumes: 256 under the feature, else
    /// terminfo's `colors`.
    #[must_use]
    pub fn palette_size(&self) -> u32 {
        if self.colours256 {
            256
        } else {
            self.base.colours
        }
    }

    /// `#{client_colours}` (format.c `format_cb_client_colours`).
    #[must_use]
    pub fn colours(&self) -> u32 {
        if self.rgb {
            1 << 24
        } else if self.colours256 {
            256
        } else if self.base.colours < 8 {
            2
        } else if self.base.colours < 16 {
            8
        } else {
            16
        }
    }

    /// `tty_apply_features`: whether any feature was newly applied.
    fn apply_features(&mut self) -> bool {
        let wanted = self.features.enabled & !self.features.disabled;
        if wanted == 0 {
            return false;
        }
        for &(feature, _) in &FEATURES {
            let bit = feature.bit();
            if self.applied & bit != 0 || wanted & bit == 0 {
                continue;
            }
            for &cap in feature.caps() {
                // TERM_NOREPLACE: `cstyle` leaves a cursor style the terminal has.
                if feature == Feature::CursorStyle && self.has(cap) {
                    continue;
                }
                self.set(cap, cap.feature_value());
            }
            if matches!(feature, Feature::Colours256 | Feature::Rgb) {
                self.colours256 = true;
            }
        }
        if self.applied | wanted == self.applied {
            return false;
        }
        self.applied |= wanted;
        true
    }

    /// `tty_term_apply_overrides`.
    fn apply_overrides(&mut self) {
        for item in std::mem::take(&mut self.overrides) {
            if let Some((pattern, rest)) = split_entry(&item)
                && fnmatch::matches(&pattern, &self.name, false)
            {
                self.apply(rest);
            }
            self.overrides.push(item);
        }
        self.rgb = self.has(Cap::SetRgbF) && self.has(Cap::SetRgbB);
        // tty_term_validate: an `Ms` that does not take two string
        // parameters is dropped.
        if self.ms.as_deref().is_some_and(|ms| !ms_is_valid(ms)) {
            self.clear(Cap::Ms);
        }
    }

    fn set(&mut self, cap: Cap, template: Option<&str>) {
        self.caps |= cap.bit();
        let value = || Some(template.unwrap_or_default().to_owned());
        match cap {
            Cap::Ms => self.ms = value(),
            Cap::Tsl => self.tsl = value(),
            Cap::Fsl => self.fsl = value(),
            _ => {}
        }
    }

    fn clear(&mut self, cap: Cap) {
        self.caps &= !cap.bit();
        match cap {
            Cap::Ms => self.ms = None,
            Cap::Tsl => self.tsl = None,
            Cap::Fsl => self.fsl = None,
            _ => {}
        }
    }

    /// `tty_term_apply` with `flags == 0`: `name=value` sets, `name@`
    /// removes, a bare name sets an empty value.
    fn apply(&mut self, capabilities: &str) {
        let mut at = 0;
        while let Some(token) = next_token(capabilities, &mut at) {
            if token.is_empty() {
                continue;
            }
            let (name, value, remove) = if let Some((name, value)) = token.split_once('=') {
                (name, Some(unvis(value)), false)
            } else if let Some(name) = token.strip_suffix('@') {
                (name, None, true)
            } else {
                (token.as_str(), Some(String::new()), false)
            };
            let Some(cap) = Cap::ALL.into_iter().find(|c| c.name() == name) else {
                continue;
            };
            if remove {
                self.clear(cap);
            } else {
                debug_assert!(cap.is_flag() || value.is_some());
                self.set(cap, value.as_deref());
            }
        }
    }
}

/// `tty_term_override_next`: the next `:`-separated token of `s` from `*at`,
/// where `::` is a literal colon. `None` at the end, or past 8191 bytes.
fn next_token(s: &str, at: &mut usize) -> Option<String> {
    const LIMIT: usize = 8192 - 1;
    let bytes = s.as_bytes();
    if *at >= bytes.len() {
        return None;
    }
    let mut value = Vec::new();
    let mut i = *at;
    while i < bytes.len() {
        if bytes[i] == b':' {
            if bytes.get(i + 1) == Some(&b':') {
                value.push(b':');
                i += 2;
            } else {
                break;
            }
        } else {
            value.push(bytes[i]);
            i += 1;
        }
        if value.len() == LIMIT {
            return None;
        }
    }
    *at = if i < bytes.len() { i + 1 } else { i };
    Some(String::from_utf8_lossy(&value).into_owned())
}

/// The pattern of a `terminal-features` / `terminal-overrides` item and the
/// rest of it after the first lone `:`.
fn split_entry(item: &str) -> Option<(String, &str)> {
    let mut at = 0;
    let pattern = next_token(item, &mut at)?;
    Some((pattern, &item[at..]))
}

/// Whether `tiparm_s(Ms, "c", "?")` gives a non-empty string
/// (tty-term.c `tty_term_validate`). A small model of ncurses 6.6's check,
/// fitted to 40 probed templates (D-007): both parameters must be consumed
/// as strings (`%s`, `%l`), none above 2 may be pushed, and some output must
/// remain. Conditionals, variables and arithmetic are not evaluated.
fn ms_is_valid(template: &str) -> bool {
    let bytes = template.as_bytes();
    let mut stack: Vec<u8> = Vec::new();
    let mut as_string = [false; 3];
    let mut output = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            output = true;
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            Some(b'p') if bytes.get(i + 1).is_some_and(u8::is_ascii_digit) => {
                let n = bytes[i + 1] - b'0';
                if !(1..=2).contains(&n) {
                    return false;
                }
                stack.push(n);
                i += 1;
            }
            Some(b @ (b's' | b'l')) => {
                if let Some(n) = stack.pop() {
                    as_string[usize::from(n)] = true;
                }
                output |= *b == b's';
            }
            Some(b'%') => output = true,
            _ => {}
        }
        i += 1;
    }
    as_string[1] && as_string[2] && output
}

/// `tiparm_s(template, p1, p2)` for the `%` operations an `Ms` that
/// passed [`ms_is_valid`] can use: `%pN` pushes a parameter, `%s` prints
/// and `%l` prints the length of the top, `%%` is a `%`. Anything else is
/// dropped, as the validity model does not evaluate it either.
fn expand_ss(template: &str, p1: &str, p2: &str) -> Vec<u8> {
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut stack: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1;
        match bytes.get(i) {
            Some(b'%') => out.push(b'%'),
            Some(b'p') => {
                match bytes.get(i + 1) {
                    Some(b'1') => stack.push(p1),
                    Some(b'2') => stack.push(p2),
                    _ => stack.push(""),
                }
                i += 1;
            }
            Some(b's') => out.extend_from_slice(stack.pop().unwrap_or("").as_bytes()),
            Some(b'l') => {
                let n = stack.pop().unwrap_or("").len();
                out.extend_from_slice(n.to_string().as_bytes());
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// `b64_ntop`: standard base64 with padding.
fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (k, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if k <= chunk.len() {
                out.push(char::from(ALPHABET[(n >> shift) as usize & 63]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `strunvis` (compat/unvis.c); the input unchanged when it is malformed,
/// as `tty_term_apply` keeps it.
fn unvis(s: &str) -> String {
    decode(s.as_bytes()).map_or_else(
        || s.to_owned(),
        |b| String::from_utf8_lossy(&b).into_owned(),
    )
}

fn decode(src: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < src.len() {
        if src[i] != b'\\' {
            out.push(src[i]);
            i += 1;
            continue;
        }
        i += 1;
        let c = *src.get(i)?;
        i += 1;
        match c {
            b'\\' => out.push(b'\\'),
            b'0'..=b'7' => {
                let mut n = c - b'0';
                for _ in 0..2 {
                    match src.get(i) {
                        Some(d @ b'0'..=b'7') => {
                            n = (n << 3).wrapping_add(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(n);
            }
            b'M' => {
                let mut b = 0x80;
                match src.get(i)? {
                    b'-' => {
                        b |= *src.get(i + 1)?;
                        i += 2;
                    }
                    b'^' => {
                        b |= ctrl(*src.get(i + 1)?, b);
                        i += 2;
                    }
                    _ => return None,
                }
                out.push(b);
            }
            b'^' => {
                out.push(ctrl(*src.get(i)?, 0));
                i += 1;
            }
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(8),
            b'a' => out.push(7),
            b'v' => out.push(11),
            b't' => out.push(b'\t'),
            b'f' => out.push(12),
            b's' => out.push(b' '),
            b'E' => out.push(0x1b),
            b'\n' | b'$' => {}
            _ => return None,
        }
    }
    Some(out)
}

/// The control-character step of unvis: `^?` is DEL, else the low five bits.
fn ctrl(c: u8, base: u8) -> u8 {
    if c == b'?' {
        base | 0o177
    } else {
        base | (c & 0o37)
    }
}

#[cfg(test)]
mod golden;

#[cfg(test)]
mod selection_tests {
    use super::*;

    /// Reference PTY probe: `setb -w hello-off` -> `\e]52;;aGVsbG8tb2Zm\a`.
    #[test]
    fn set_selection_is_osc52_with_base64() {
        let term = Term::open("xterm-256color", &[], None, &[], &[]).unwrap();
        assert_eq!(
            term.set_selection("", b"hello-off").unwrap(),
            b"\x1b]52;;aGVsbG8tb2Zm\x07"
        );
        assert_eq!(term.clipboard_query().unwrap(), b"\x1b]52;;?\x07");
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(&[0xff, 0x80]), "/4A=");
        let swapped = Term::open(
            "xterm-256color",
            &[],
            None,
            &[],
            &["xterm*:Ms=<%p2%s|%p1%s>".to_owned()],
        )
        .unwrap();
        assert_eq!(swapped.set_selection("c", b"x").unwrap(), b"<eA==|c>");
        let none =
            Term::open("xterm-256color", &[], None, &[], &["xterm*:Ms@".to_owned()]).unwrap();
        assert!(none.set_selection("", b"x").is_none());
    }
}
