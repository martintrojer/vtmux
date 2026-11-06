//! vtmux's own version (D25): the tmux release it is tested against, with
//! tmux-style letter releases and pre-release suffixes. Cargo carries a
//! semver, so `3.8.0-beta.1` displays as `3.8-beta1` and `3.8.1` as `3.8a`.

/// The tmux version vtmux reports as `tmux -V` and `#{version}` (D21): the
/// bare release plugins gate on.
pub const TMUX_VERSION: &str = "3.8";

/// The display version of this build, from the workspace's Cargo version.
#[must_use]
pub fn display() -> String {
    display_from_cargo(env!("CARGO_PKG_VERSION"))
}

/// Maps a Cargo version to the display version: `MAJOR.MINOR.PATCH` is
/// `MAJOR.MINOR` plus the letter `a` + PATCH - 1 (none for 0), and a
/// pre-release `-beta.1` becomes `-beta1`. Input that is not
/// `MAJOR.MINOR.PATCH[-pre]` with PATCH <= 26 comes back unchanged.
#[must_use]
pub fn display_from_cargo(cargo: &str) -> String {
    let (core, pre) = match cargo.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (cargo, None),
    };
    let mut parts = core.split('.');
    let (Some(major), Some(minor), Some(patch), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return cargo.to_owned();
    };
    let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let letter = match patch.parse::<u8>() {
        Ok(0) => None,
        Ok(n @ 1..=26) => Some(char::from(b'a' + n - 1)),
        _ => return cargo.to_owned(),
    };
    if !numeric(major) || !numeric(minor) {
        return cargo.to_owned();
    }
    let mut out = format!("{major}.{minor}");
    out.extend(letter);
    if let Some(pre) = pre {
        out.push('-');
        out.push_str(&pre.replace('.', ""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cargo_to_display() {
        for (cargo, shown) in [
            ("3.8.0-beta.1", "3.8-beta1"),
            ("3.8.0-rc.2", "3.8-rc2"),
            ("3.8.0", "3.8"),
            ("3.8.1", "3.8a"),
            ("3.8.2", "3.8b"),
            ("3.8.26", "3.8z"),
            ("3.8.1-rc.1", "3.8a-rc1"),
            ("3.10.0", "3.10"),
            ("3.8.27", "3.8.27"),
            ("3.8", "3.8"),
            ("x.8.0", "x.8.0"),
        ] {
            assert_eq!(display_from_cargo(cargo), shown, "{cargo}");
        }
    }

    #[test]
    fn this_build_is_a_tmux_release() {
        // The display version starts with the tmux release we track.
        assert!(display().starts_with(TMUX_VERSION), "{}", display());
    }
}
