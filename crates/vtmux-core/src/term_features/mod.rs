//! Terminal features and overrides: which of `terminal-features`,
//! `terminal-overrides`, the client's `-T` / `-2` flags and `COLORTERM` apply
//! to a client's terminal, as tty-features.c and tty-term.c decide them.
//!
//! vtmux reads no terminfo (D8): the terminals it knows are a table
//! generated from terminfo (`terminfo.rs`, `tests/terminfo_table.py`).
//! [`Term`] tracks every `tty_term_codes` name by presence, the
//! capabilities that reach what vtmux draws as [`Cap`], and the values of
//! the strings vtmux writes or decodes (D-007).

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

    /// The capabilities of `tty_feature_*_capabilities` that [`Cap`] models;
    /// the rest are [`Feature::other_caps`].
    fn caps(self) -> &'static [Cap] {
        match self {
            Self::Clipboard => &[Cap::Ms],
            Self::CursorStyle => &[Cap::Ss, Cap::Se],
            Self::Rgb => &[Cap::SetRgbF, Cap::SetRgbB],
            Self::Sync => &[Cap::Sync],
            Self::Title => &[Cap::Tsl, Cap::Fsl],
            Self::Osc7 => &[Cap::Fsl],
            Self::ExtendedKeys => &[Cap::Eneks, Cap::Dseks],
            Self::UnderlineStyles => &[Cap::Smulx, Cap::Setulc],
            _ => &[],
        }
    }

    /// The other capability names `tty_feature_*_capabilities` add (`kf0@` ...
    /// remove), the ones [`Cap`] does not model; `#{I/c:...}` sees them.
    fn other_caps(self) -> &'static [&'static str] {
        match self {
            Self::Colours256 | Self::Rgb => &["AX", "setab", "setaf"],
            Self::AppEsc => &["Enesc", "Dsesc"],
            Self::BracketedPaste => &["Enbp", "Dsbp"],
            Self::CursorColour => &["Cs", "Cr"],
            Self::Hyperlinks => &["Hls"],
            Self::Focus => &["Enfcs", "Dsfcs"],
            Self::Margins => &["Enmg", "Dsmg", "Clmg", "Cmg"],
            Self::Osc7 => &["Swd"],
            Self::Overline => &["Smol"],
            Self::ProgressBar => &["Spb"],
            Self::RectFill => &["Rect"],
            Self::Sixel => &["Sxl"],
            Self::Strikethrough => &["smxx"],
            Self::UnderlineStyles => &["Setulc1", "ol"],
            _ => &[],
        }
    }

    /// The names of every capability the feature sets, for
    /// `tty_feature_present`.
    fn cap_names(self) -> impl Iterator<Item = &'static str> {
        self.caps()
            .iter()
            .map(|c| c.name())
            .chain(self.other_caps().iter().copied())
            .chain((self == Self::Mouse).then_some("kmous"))
    }

    /// `#{I/f:name}`'s table entry (`tty_features`, by exact name).
    fn named(name: &str) -> Option<Self> {
        FEATURES.iter().find(|(_, n)| *n == name).map(|(f, _)| *f)
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
    /// Ask the terminal for extended keys, and stop (`extkeys`).
    Eneks,
    Dseks,
    /// `kmous`: the terminal sends mouse reports, so `tty_update_mode`
    /// may turn mouse modes on (tty.c:904) and `tty_start_tty` turns them
    /// off first (xterm's `\E[<`, screen's and tmux's `\E[M`; `linux` and
    /// the VT terminals have none).
    Kmous,
}

impl Cap {
    /// Every capability, in the order the goldens list them (`kmous`,
    /// which they do not list, last).
    pub const ALL: [Self; 15] = [
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
        Self::Eneks,
        Self::Dseks,
        Self::Kmous,
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
            Self::Eneks => "Eneks",
            Self::Dseks => "Dseks",
            Self::Kmous => "kmous",
        }
    }

    /// The value a feature gives a string capability
    /// (`tty_feature_*_capabilities`), escapes decoded.
    fn feature_value(self) -> Option<&'static str> {
        feature_value(self.name())
    }

    fn bit(self) -> u16 {
        1 << Self::ALL.iter().position(|c| *c == self).unwrap_or(0)
    }

    fn is_flag(self) -> bool {
        matches!(self, Self::Tc | Self::Rgb)
    }
}

/// `Ms` as the `clipboard` feature gives it (tty-features.c), escapes decoded.
const MS: &str = "\x1b]52;%p1%s;%p2%s\x07";

/// The value the feature that adds string capability `name` gives it
/// (`tty_feature_*_capabilities`, tty-features.c), escapes decoded; `None`
/// for one vtmux does not write.
fn feature_value(name: &str) -> Option<&'static str> {
    Some(match name {
        "Ms" => MS,
        "tsl" => "\x1b]0;",
        "fsl" => "\x07",
        "Eneks" => "\x1b[>4;2m",
        "Dseks" => "\x1b[>4m",
        // `cstyle`, `ccolour`.
        "Ss" => "\x1b[%p1%d q",
        "Se" => "\x1b[2 q",
        "Cs" => "\x1b]12;%p1%s\x07",
        "Cr" => "\x1b]112\x07",
        // `usstyle`, `overline`, `strikethrough`.
        "Smulx" => "\x1b[4:%p1%dm",
        "Setulc" => "\x1b[58:2:%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m",
        "Setulc1" => "\x1b[58:5:%p1%dm",
        "ol" => "\x1b[59m",
        "Smol" => "\x1b[53m",
        "smxx" => "\x1b[9m",
        _ => return None,
    })
}

/// Why a terminal cannot be opened (`tty_term_read_list`, `tty_term_create`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpenError {
    /// The terminal is not one vtmux knows: what tmux's client says when the
    /// terminfo database has no such entry (`setupterm` fails).
    Missing(String),
    /// `tty_term_create` requires `clear` and `cup`; the text says which is
    /// missing.
    Lacks(&'static str),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(name) => write!(f, "missing or unsuitable terminal: {name}"),
            Self::Lacks(cap) => write!(f, "terminal does not support {cap}"),
        }
    }
}

/// The terminfo entry of a terminal vtmux knows (D8, D-007): the generated
/// table's, else, for a name of the `xterm` family the table lacks, `xterm`'s
/// with the colours the name suggests (`xterm-direct*` with `RGB`). An
/// empty name has no entry at all, as tmux's client reads none.
fn entry(name: &str) -> Option<(&'static terminfo::Entry, Option<u32>, bool)> {
    const EMPTY: terminfo::Entry = terminfo::Entry {
        names: &[],
        off: &[],
        colours: None,
        strings: &[],
    };
    let find = |n: &str| {
        terminfo::TERMINALS
            .iter()
            .find(|(t, _)| *t == n)
            .map(|(_, e)| e)
    };
    if name.is_empty() {
        return Some((&EMPTY, None, false));
    }
    if let Some(e) = find(name) {
        return Some((e, e.colours, false));
    }
    if !name.starts_with("xterm") {
        return None;
    }
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
    Some((find("xterm")?, Some(colours), direct))
}

/// A client's terminal as `tty_term_create` leaves it.
#[expect(
    clippy::struct_excessive_bools,
    reason = "tty_term's independent flags"
)]
#[derive(Clone, Debug)]
pub struct Term {
    name: String,
    features: Features,
    /// Features whose capabilities have been applied (`applied_features`).
    applied: u32,
    caps: u16,
    /// `c->term_type`: what the terminal named itself in its XDA reply.
    version: Option<String>,
    /// `TERM_256COLOURS`.
    colours256: bool,
    /// `TERM_RGBCOLOURS`.
    rgb: bool,
    overrides: Vec<String>,
    /// The terminfo names it has that [`Cap`] does not model, after
    /// features and overrides (`term->codes` for them).
    other: std::collections::BTreeSet<&'static str>,
    /// The flags it has that are false.
    off: std::collections::BTreeSet<&'static str>,
    /// `colors`.
    colours: Option<u32>,
    /// The values of the string capabilities vtmux writes or decodes.
    values: std::collections::BTreeMap<&'static str, String>,
    /// `TERM_VT100LIKE`.
    vt100_like: bool,
    /// `CLIENT_UTF8`, which `tty_feature_present` reads for `utf8`.
    utf8: bool,
}

impl Term {
    /// `tty_term_create`, when the terminal is one vtmux knows. `client` is
    /// what the client sent (`-T`, `-2`); `features` and `overrides` are the
    /// items of the `terminal-features` and `terminal-overrides` options;
    /// `colorterm` is `$COLORTERM` from the client's environment.
    ///
    /// # Errors
    ///
    /// [`OpenError`] for a terminal vtmux has no entry for, or one left
    /// without `clear` or `cup`.
    pub fn open(
        name: &str,
        client: &[String],
        colorterm: Option<&str>,
        features: &[String],
        overrides: &[String],
    ) -> Result<Self, OpenError> {
        let (entry, colours, direct) =
            entry(name).ok_or_else(|| OpenError::Missing(name.to_owned()))?;
        let mut caps = Cap::ALL
            .into_iter()
            .filter(|c| entry.names.contains(&c.name()))
            .fold(0, |bits, c| bits | c.bit());
        if direct {
            caps |= Cap::Rgb.bit();
        }
        let modelled = |n: &&str| Cap::ALL.iter().any(|c| c.name() == *n);
        let mut term = Self {
            name: name.to_owned(),
            features: Features::from_client(client),
            applied: 0,
            caps,
            version: None,
            colours256: false,
            rgb: false,
            overrides: overrides.to_vec(),
            other: entry
                .names
                .iter()
                .copied()
                .filter(|n| !modelled(n))
                .collect(),
            off: entry.off.iter().copied().collect(),
            colours,
            values: entry
                .strings
                .iter()
                .map(|&(n, v)| (n, v.to_owned()))
                .collect(),
            vt100_like: false,
            utf8: false,
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
        for cap in ["clear", "cup"] {
            if !term.has_name(cap) {
                return Err(OpenError::Lacks(cap));
            }
        }
        // tty-term.c:673-677: XT, or a `clear` that starts with CSI.
        term.vt100_like =
            term.flag("XT") || term.string("clear").is_some_and(|s| s.starts_with("\x1b["));
        if term.vt100_like {
            term.features.parse("bpaste,focus,title", &[','], true);
        }
        if (term.flag(Cap::Tc.name()) || term.has(Cap::Rgb))
            && (!term.has(Cap::SetRgbF) || !term.has(Cap::SetRgbB))
        {
            term.features.parse("RGB", &[','], true);
        }
        if term.apply_features() {
            term.apply_overrides();
        }
        Ok(term)
    }

    /// The terminal answered the synchronised-update query: `tty_keys_sync`.
    pub fn sync_supported(&mut self) {
        self.features.parse("sync", &[','], true);
        self.update_features();
    }

    /// The terminal's primary device attributes (`CSI ? Ps ; ... c`), as
    /// `tty_keys_device_attributes` (tty-keys.c:1467-1544) reads them: a
    /// VT level 1-5 adds a feature for each attribute it names. tmux keeps
    /// each parameter in a `char`, so only its low byte counts.
    pub fn device_attributes(&mut self, params: &[u32]) {
        let p = |i: usize| params.get(i).map_or(0, |n| n.to_le_bytes()[0]);
        if (61..=65).contains(&p(0)) {
            for i in 1..params.len().min(32) {
                let name = match p(i) {
                    4 => "sixel",
                    21 => "margins",
                    28 => "rectfill",
                    52 => "clipboard",
                    _ => continue,
                };
                self.features.parse(name, &[','], true);
            }
        }
        self.update_features();
    }

    /// The terminal's secondary device attributes (`CSI > Pp ; ... c`):
    /// `tty_keys_device_attributes2` (tty-keys.c:1596-1670) knows mintty,
    /// tmux and rxvt-unicode by their first parameter.
    pub fn secondary_device_attributes(&mut self, first: u32) {
        match first.to_le_bytes()[0] {
            b'M' => self.default_features("mintty"),
            b'T' => self.default_features("tmux"),
            b'U' => self.default_features("rxvt-unicode"),
            _ => {}
        }
        self.update_features();
    }

    /// The terminal's XTVERSION reply (`DCS > | text ST`):
    /// `tty_keys_extended_device_attributes` (tty-keys.c:1676-1747) knows
    /// some terminals by name, and keeps the text as `#{client_termtype}`.
    pub fn extended_device_attributes(&mut self, text: &str) {
        let known = [
            ("iTerm2 ", "iTerm2"),
            ("tmux ", "tmux"),
            ("XTerm(", "XTerm"),
            ("mintty ", "mintty"),
            ("foot(", "foot"),
            ("WezTerm ", "WezTerm"),
            ("ghostty ", "ghostty"),
            ("Rio ", "Rio"),
        ];
        if let Some((_, name)) = known.iter().find(|(prefix, _)| text.starts_with(prefix)) {
            self.default_features(name);
        }
        self.version = Some(text.to_owned());
        self.update_features();
    }

    /// `#{client_termtype}`: empty until the terminal answered XTVERSION.
    #[must_use]
    pub fn term_type(&self) -> &str {
        self.version.as_deref().unwrap_or_default()
    }

    /// `tty_default_features` (tty-features.c:572-693) for a terminal
    /// that named itself; the version checks there never apply (tmux
    /// passes 0).
    fn default_features(&mut self, name: &str) {
        const MODERN: &str = "256,RGB,bpaste,clipboard,mouse,strikethrough,title";
        let extra = match name {
            "mintty" => "appesc,ccolour,cstyle,extkeys,margins,overline,usstyle",
            "tmux" => "ccolour,cstyle,extkeys,focus,overline,usstyle,hyperlinks,progressbar",
            "rxvt-unicode" => {
                self.features.parse(
                    "256,bpaste,ccolour,cstyle,mouse,title,ignorefkeys",
                    &[','],
                    true,
                );
                return;
            }
            "iTerm2" => "cstyle,extkeys,margins,usstyle,sync,osc7,hyperlinks,progressbar",
            "foot" => "ccolour,cstyle,extkeys,usstyle,sync,osc7,hyperlinks",
            "WezTerm" => "ccolour,cstyle,extkeys,focus,hyperlinks,margins,usstyle",
            "ghostty" => {
                "ccolour,cstyle,extkeys,focus,margins,overline,hyperlinks,osc7,sync,usstyle,progressbar"
            }
            "Rio" => "ccolour,cstyle,focus,overline,hyperlinks,osc7,sync,usstyle,progressbar",
            "XTerm" => "ccolour,cstyle,extkeys,focus",
            _ => return,
        };
        self.features
            .parse(&format!("{MODERN},{extra}"), &[','], true);
    }

    /// The feature half of `tty_update_features` (tty.c:552-555).
    fn update_features(&mut self) {
        if self.apply_features() {
            self.apply_overrides();
        }
    }

    /// `Eneks`: what asks the terminal for extended keys
    /// (`tty_update_features`, tty.c:561-562); `None` without it.
    #[must_use]
    pub fn enable_extended_keys(&self) -> Option<&str> {
        self.cap_string(Cap::Eneks)
    }

    /// `Dseks`: what turns extended keys off again (`tty_stop_tty`,
    /// tty.c:508); `None` without it.
    #[must_use]
    pub fn disable_extended_keys(&self) -> Option<&str> {
        self.cap_string(Cap::Dseks)
    }

    #[must_use]
    pub fn features(&self) -> Features {
        self.features
    }

    /// `CLIENT_UTF8`: the client said it is UTF-8 (`-u`, or its locale).
    pub fn set_utf8(&mut self, utf8: bool) {
        self.utf8 = utf8;
    }

    /// `tty_term_has_name` (tty-term.c:809), for `#{I/c:name}`: the terminal
    /// has that `tty_term_codes` capability.
    #[must_use]
    pub fn has_name(&self, name: &str) -> bool {
        match Cap::ALL.into_iter().find(|c| c.name() == name) {
            Some(cap) => self.has(cap),
            None => self.other.contains(name),
        }
    }

    /// `tty_feature_present` (tty-features.c:490), for `#{I/f:name}`: the
    /// feature was applied, or the terminal has every capability it sets and
    /// the flags it gives (`ignorefkeys` and `utf8` aside).
    #[must_use]
    pub fn feature_present(&self, name: &str) -> bool {
        if name == "utf8" {
            // tty_apply_features sets CLIENT_UTF8 for the feature too.
            return self.utf8 || self.applied & Feature::Utf8.bit() != 0;
        }
        let Some(feature) = Feature::named(name) else {
            return false;
        };
        if self.applied & feature.bit() != 0 {
            return true;
        }
        // tf->flags: `TERM_256COLOURS` comes only from a feature, and so do
        // `TERM_SIXEL` and, never set, `TERM_NOREPLACE` (`cstyle`);
        // `TERM_RGBCOLOURS`, `TERM_DECSLRM` and `TERM_DECFRA` follow the
        // capabilities checked below.
        let flags = match feature {
            Feature::IgnoreFunctionKeys | Feature::Sixel | Feature::CursorStyle => false,
            Feature::Colours256 | Feature::Rgb => self.colours256,
            _ => true,
        };
        flags && feature.cap_names().all(|c| self.has_name(c))
    }

    #[must_use]
    pub fn has(&self, cap: Cap) -> bool {
        self.caps & cap.bit() != 0
    }

    /// `tty_term_flag`: the terminal has the flag and it is true.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.has_name(name) && !self.off.contains(name)
    }

    /// `tty_term_string` for the capabilities vtmux writes or decodes
    /// (`tests/terminfo_table.py`'s `STRINGS`, or set by an override):
    /// `None` when the terminal lacks it.
    #[must_use]
    pub fn string(&self, name: &str) -> Option<&str> {
        self.values
            .get(name)
            .map(String::as_str)
            .filter(|_| self.has_name(name))
    }

    /// `tty_term_string_*` with `params`, for a capability vtmux writes:
    /// its value (the entry's, a feature's or an override's), or, where the
    /// table has the name but no value for it, the value tmux's feature for
    /// it gives (D-007); `None` when the terminal lacks it.
    #[must_use]
    pub fn code(&self, name: &str, params: &[Param]) -> Option<String> {
        if !self.has_name(name) {
            return None;
        }
        let template = self
            .values
            .get(name)
            .map(String::as_str)
            .or_else(|| feature_value(name))?;
        Some(tparm(template, params))
    }

    fn cap_string(&self, cap: Cap) -> Option<&str> {
        self.string(cap.name())
    }

    /// `TERM_VT100LIKE` (tty-term.c:673-677): the terminal has `XT` or its
    /// `clear` starts with CSI.
    #[must_use]
    pub fn vt100_like(&self) -> bool {
        self.vt100_like
    }

    /// The terminfo keys `tty_keys_build` adds to the key tree
    /// (`tty_default_code_keys`, tty-keys.c:283-360) that vtmux reads from
    /// the entry: `kf1` to `kf12`, `kich1`, `kdch1`, `khome`, `kend`, `knp`,
    /// `kpp` and `kcbt`, with their sequences. The arrows and the modified
    /// keys are xterm's rows in every entry vtmux knows.
    #[must_use]
    pub fn code_keys(&self) -> Vec<(&'static str, &str)> {
        const KEYS: [&str; 19] = [
            "kf1", "kf2", "kf3", "kf4", "kf5", "kf6", "kf7", "kf8", "kf9", "kf10", "kf11", "kf12",
            "kich1", "kdch1", "khome", "kend", "knp", "kpp", "kcbt",
        ];
        KEYS.into_iter()
            .filter_map(|k| Some((k, self.string(k).filter(|s| !s.is_empty())?)))
            .collect()
    }

    /// `tty_set_selection` (tty.c:2149-2166): `Ms` with the clipboard
    /// name and the base64 of `data` (`tty_putcode_ss`), or `None` when the
    /// terminal has no `Ms`.
    #[must_use]
    pub fn set_selection(&self, clip: &str, data: &[u8]) -> Option<Vec<u8>> {
        let ms = self.cap_string(Cap::Ms)?;
        Some(expand_ss(ms, clip, &base64(data)))
    }

    /// `tty_clipboard_query` (tty.c:3206-3216): `Ms` with an empty
    /// clipboard name and `?`, or `None` when the terminal has no `Ms`.
    #[must_use]
    pub fn clipboard_query(&self) -> Option<Vec<u8>> {
        let ms = self.cap_string(Cap::Ms)?;
        Some(expand_ss(ms, "", "?"))
    }

    /// `tty_set_title` (tty.c:743-752): `tsl`, the title and `fsl`, or
    /// `None` when the terminal lacks either.
    #[must_use]
    pub fn set_title(&self, title: &str) -> Option<Vec<u8>> {
        let tsl = self.cap_string(Cap::Tsl)?;
        let fsl = self.cap_string(Cap::Fsl)?;
        Some([tsl, title, fsl].concat().into_bytes())
    }

    /// `tty_set_path` (tty.c:755-764): `Swd`, the path and `fsl`, or
    /// `None` when the terminal lacks either.
    #[must_use]
    pub fn set_path(&self, path: &str) -> Option<Vec<u8>> {
        let swd = self.string("Swd")?;
        let fsl = self.cap_string(Cap::Fsl)?;
        Some([swd, path, fsl].concat().into_bytes())
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
            self.colours.unwrap_or(0)
        }
    }

    /// `#{client_colours}` (format.c `format_cb_client_colours`).
    #[must_use]
    pub fn colours(&self) -> u32 {
        if self.rgb {
            1 << 24
        } else if self.colours256 {
            256
        } else if self.colours.unwrap_or(0) < 8 {
            2
        } else if self.colours.unwrap_or(0) < 16 {
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
            if feature == Feature::Mouse {
                self.caps |= Cap::Kmous.bit();
            }
            if feature == Feature::IgnoreFunctionKeys {
                self.other.retain(|c| !is_function_key(c));
            }
            for &name in feature.other_caps() {
                self.other.insert(name);
                if let Some(value) = feature_value(name) {
                    self.values.insert(name, value.to_owned());
                }
            }
            // tty-features.c:63-66: `Swd=\E]7;`, replacing the terminal's.
            if feature == Feature::Osc7 {
                self.values.insert("Swd", "\x1b]7;".to_owned());
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
        if self.cap_string(Cap::Ms).is_some_and(|ms| !ms_is_valid(ms)) {
            self.clear(Cap::Ms);
        }
    }

    fn set(&mut self, cap: Cap, template: Option<&str>) {
        self.caps |= cap.bit();
        self.off.remove(cap.name());
        if !cap.is_flag() {
            self.values
                .insert(cap.name(), template.unwrap_or_default().to_owned());
        }
    }

    fn clear(&mut self, cap: Cap) {
        self.caps &= !cap.bit();
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
                if let Some(&(known, kind)) = terminfo::ALL.iter().find(|(n, _)| *n == name) {
                    self.apply_other(known, kind, value, remove);
                }
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

    /// One override of a capability [`Cap`] does not model.
    fn apply_other(
        &mut self,
        name: &'static str,
        kind: terminfo::Kind,
        value: Option<String>,
        remove: bool,
    ) {
        if remove {
            self.other.remove(name);
            self.off.remove(name);
            if name == "colors" {
                self.colours = None;
            }
            return;
        }
        let value = value.unwrap_or_default();
        match kind {
            terminfo::Kind::String => {
                self.values.insert(name, value);
            }
            terminfo::Kind::Number => {
                // strtonum(value, 0, INT_MAX): one that does not parse
                // changes nothing.
                let Some(n) = value
                    .parse::<i32>()
                    .ok()
                    .and_then(|n| u32::try_from(n).ok())
                else {
                    return;
                };
                if name == "colors" {
                    self.colours = Some(n);
                }
            }
            terminfo::Kind::Flag => {
                self.off.remove(name);
            }
        }
        self.other.insert(name);
    }
}

/// `kf0` to `kf63`, which the `ignorefkeys` feature removes.
fn is_function_key(name: &str) -> bool {
    name.strip_prefix("kf")
        .and_then(|n| n.parse::<u8>().ok())
        .is_some_and(|n| n <= 63)
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
#[must_use]
pub fn base64(data: &[u8]) -> String {
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
mod terminfo;
mod tparm;
pub use tparm::{Param, tparm};

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

#[cfg(test)]
mod table_tests {
    use super::*;

    fn open(name: &str, overrides: &[&str]) -> Result<Term, OpenError> {
        let overrides: Vec<String> = overrides.iter().map(|s| (*s).to_owned()).collect();
        Term::open(name, &[], None, &[], &overrides)
    }

    /// tty-term.c `tty_term_create` refuses a terminal without `clear` or
    /// `cup`, after the overrides; tmux's client refuses one its terminfo
    /// lacks (`tty_term_read_list`). Reference: `TERM=dumb tmux new` prints
    /// `open terminal failed: terminal does not support clear`, `TERM=bogus`
    /// `missing or unsuitable terminal: bogus`.
    #[test]
    fn refusals_are_tmux_s() {
        let text = |r: Result<Term, OpenError>| r.err().map(|e| e.to_string());
        assert_eq!(
            text(open("dumb", &[])).as_deref(),
            Some("terminal does not support clear")
        );
        assert_eq!(
            text(open("", &[])).as_deref(),
            Some("terminal does not support clear")
        );
        assert_eq!(
            text(open("bogus", &[])).as_deref(),
            Some("missing or unsuitable terminal: bogus")
        );
        assert_eq!(
            text(open("xterm-256color", &["xterm*:cup@"])).as_deref(),
            Some("terminal does not support cup")
        );
        // An override can give a terminal what it lacks.
        assert!(open("dumb", &["dumb:clear=\\E[H\\E[2J:cup=\\E[%i%p1%d;%p2%dH"]).is_ok());
        for name in [
            "foot",
            "linux",
            "alacritty",
            "xterm-kitty",
            "screen.xterm-256color",
            "tmux",
            "rxvt",
            "vt100",
        ] {
            assert!(open(name, &[]).is_ok(), "{name}");
        }
    }

    /// What the strings and flags vtmux writes with come from: the entry,
    /// then `terminal-overrides` (`tty_term_apply`).
    #[test]
    fn strings_and_flags_follow_the_entry_and_overrides() {
        let linux = open("linux", &[]).unwrap();
        assert_eq!(linux.string("smcup"), None);
        assert_eq!(linux.string("clear"), Some("\x1b[H\x1b[J"));
        assert!(linux.flag("bce") && linux.flag("am") && !linux.flag("XT"));
        assert!(linux.vt100_like());
        assert_eq!(linux.palette_size(), 8);
        let vt100 = open("vt100", &[]).unwrap();
        assert!(!vt100.flag("bce") && vt100.has_name("bce"));
        assert!(!vt100.has_name("setaf") && vt100.colours() == 2);
        // tty_term_strip drops the padding.
        assert_eq!(vt100.string("clear"), Some("\x1b[H\x1b[J"));
        let xterm = open("xterm-256color", &["xterm*:smcup@:bce@:colors=8:E3=x"]).unwrap();
        assert_eq!(xterm.string("smcup"), None);
        assert_eq!(xterm.string("rmcup"), Some("\x1b[?1049l\x1b[23;0;0t"));
        assert!(!xterm.flag("bce") && !xterm.has_name("bce"));
        assert_eq!((xterm.palette_size(), xterm.string("E3")), (8, Some("x")));
        let bce = open("vt100", &["vt100:bce:colors=x"]).unwrap();
        assert!(bce.flag("bce"));
        assert_eq!(bce.palette_size(), 0);
        // Not CSI and no XT: not VT100-like, so no bpaste, focus or title.
        let odd = open("xterm-256color", &["xterm*:XT@:clear=\\Ec"]).unwrap();
        assert!(!odd.vt100_like() && !odd.features().has(Feature::BracketedPaste));
    }

    /// `tty_default_code_keys`: the console's F1-F5, rxvt's Home and End,
    /// and `ignorefkeys` (rxvt's default feature) taking the F keys away.
    #[test]
    fn code_keys_are_the_entry_s() {
        let keys = |t: &Term, cap: &str| {
            t.code_keys()
                .into_iter()
                .find(|(c, _)| *c == cap)
                .map(|(_, s)| s.to_owned())
        };
        let linux = open("linux", &[]).unwrap();
        assert_eq!(keys(&linux, "kf1").as_deref(), Some("\x1b[[A"));
        assert_eq!(keys(&linux, "khome").as_deref(), Some("\x1b[1~"));
        let rxvt = Term::open(
            "rxvt-unicode-256color",
            &[],
            None,
            &["rxvt*:ignorefkeys".to_owned()],
            &[],
        )
        .unwrap();
        assert_eq!(keys(&rxvt, "khome").as_deref(), Some("\x1b[7~"));
        assert_eq!(keys(&rxvt, "kf1"), None);
        let xterm = open("xterm-256color", &["xterm*:kf1=\\EOX"]).unwrap();
        assert_eq!(keys(&xterm, "kf1").as_deref(), Some("\x1bOX"));
        assert_eq!(keys(&xterm, "khome").as_deref(), Some("\x1bOH"));
    }
}

#[cfg(test)]
mod code_tests {
    use super::*;

    fn open(name: &str, features: &[&str], overrides: &[&str]) -> Term {
        let list = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        Term::open(name, &[], None, &list(features), &list(overrides)).unwrap()
    }

    /// `tparm` as ncurses evaluates the templates terminfo and
    /// tty-features.c use: parameters, constants, arithmetic, `%i`,
    /// conditionals, and the stray `%;` of the `usstyle` `Setulc`.
    #[test]
    fn tparm_evaluates_terminfo_templates() {
        use Param::{Int, Str};
        assert_eq!(tparm("\x1b[%p1%d q", &[Int(5)]), "\x1b[5 q");
        assert_eq!(tparm("\x1b]12;%p1%s\x07", &[Str("red")]), "\x1b]12;red\x07");
        assert_eq!(
            tparm("\x1b[%i%p1%d;%p2%dH", &[Int(0), Int(9)]),
            "\x1b[1;10H"
        );
        assert_eq!(
            tparm(
                "\x1b[58:2:%p1%{65536}%/%d:%p1%{256}%/%{255}%&%d:%p1%{255}%&%d%;m",
                &[Int(0x01_02_03)]
            ),
            "\x1b[58:2:1:2:3m"
        );
        assert_eq!(
            tparm(
                "%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;",
                &[Int(3)]
            ),
            "33"
        );
        assert_eq!(
            tparm(
                "%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;",
                &[Int(12)]
            ),
            "94"
        );
        assert_eq!(
            tparm(
                "%?%p1%{8}%<%t3%p1%d%e%p1%{16}%<%t9%p1%{8}%-%d%e38;5;%p1%d%;",
                &[Int(200)]
            ),
            "38;5;200"
        );
        assert_eq!(tparm("100%%%'A'%c", &[]), "100%A");
    }

    /// The cursor and underline strings come from the entry, then the
    /// features (which replace them, but for `cstyle`'s `TERM_NOREPLACE`),
    /// then the overrides; a name the table has no value for takes the
    /// value tmux's feature gives it.
    #[test]
    fn cursor_and_underline_codes_follow_entry_features_and_overrides() {
        let code = |t: &Term, name: &str, p: &[Param]| t.code(name, p);
        let ghostty = open("xterm-ghostty", &[], &[]);
        assert_eq!(code(&ghostty, "Se", &[]).as_deref(), Some("\x1b[0 q"));
        assert_eq!(
            code(&ghostty, "Ss", &[Param::Int(6)]).as_deref(),
            Some("\x1b[6 q")
        );
        assert_eq!(code(&ghostty, "Cs", &[Param::Str("red")]), None);
        assert_eq!(
            code(&ghostty, "Setulc", &[Param::Int(0x01_02_03)]).as_deref(),
            Some("\x1b[58:2::1:2:3m")
        );
        // XTVERSION adds ghostty's features: `usstyle` replaces Setulc,
        // `cstyle` leaves Se, `ccolour` adds Cs and Cr.
        let mut named = open("xterm-ghostty", &[], &[]);
        named.extended_device_attributes("ghostty 1.2.0");
        assert_eq!(code(&named, "Se", &[]).as_deref(), Some("\x1b[0 q"));
        assert_eq!(
            code(&named, "Setulc", &[Param::Int(0x01_02_03)]).as_deref(),
            Some("\x1b[58:2:1:2:3m")
        );
        assert_eq!(
            code(&named, "Cs", &[Param::Str("rgb:ff/00/00")]).as_deref(),
            Some("\x1b]12;rgb:ff/00/00\x07")
        );
        assert_eq!(code(&named, "Cr", &[]).as_deref(), Some("\x1b]112\x07"));
        assert_eq!(code(&named, "Smol", &[]).as_deref(), Some("\x1b[53m"));
        let xterm = open("xterm-256color", &["xterm*:usstyle"], &[]);
        assert_eq!(code(&xterm, "Se", &[]).as_deref(), Some("\x1b[2 q"));
        assert_eq!(
            code(&xterm, "Smulx", &[Param::Int(3)]).as_deref(),
            Some("\x1b[4:3m")
        );
        assert_eq!(
            code(&xterm, "Setulc1", &[Param::Int(196)]).as_deref(),
            Some("\x1b[58:5:196m")
        );
        assert_eq!(code(&xterm, "Smol", &[]), None);
        let tmux = open("tmux-256color", &[], &[]);
        assert_eq!(
            code(&tmux, "Smulx", &[Param::Int(2)]).as_deref(),
            Some("\x1b[4:2m")
        );
        assert_eq!(code(&tmux, "Setulc", &[Param::Int(1)]), None);
        assert_eq!(code(&tmux, "cvvis", &[]).as_deref(), Some("\x1b[34l"));
        let screen = open("screen", &[], &[]);
        assert_eq!(code(&screen, "smso", &[]).as_deref(), Some("\x1b[3m"));
        // An override's value wins; `Se` without one (the common
        // `Se=\E[ q`) is written as given.
        let over = open("xterm-256color", &[], &["xterm*:Ss=\\E[%p1%d q:Se=\\E[ q"]);
        assert_eq!(code(&over, "Se", &[]).as_deref(), Some("\x1b[ q"));
        // mintty's entry has the names but the table no values.
        let mintty = open("mintty", &[], &[]);
        assert_eq!(code(&mintty, "Se", &[]).as_deref(), Some("\x1b[2 q"));
        assert_eq!(code(&mintty, "Cr", &[]).as_deref(), Some("\x1b]112\x07"));
    }
}
