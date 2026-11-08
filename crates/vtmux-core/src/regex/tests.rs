use super::*;
type Captures = Vec<Option<Range<usize>>>;
type Row = (
    &'static str,
    &'static str,
    bool,
    Result<Option<Captures>, Error>,
);
include!("golden.in");

#[test]
fn regex_glibc_goldens() {
    let mut failures = Vec::new();
    for (pattern, text, ignore_case, expected) in rows() {
        let result = Regex::new(pattern, ignore_case)
            .and_then(|r| r.find(text))
            .map(|m| {
                m.map(|m| {
                    let mut captures = m.captures;
                    captures.resize(10, None);
                    captures.truncate(10);
                    captures
                })
            });
        if result != expected {
            failures.push(format!(
                "{pattern:?} {text:?} i={ignore_case}: {result:?} != {expected:?}"
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn regex_resource_limits_are_errors_not_panics() {
    for pattern in [
        "a".to_owned() + &"*".repeat(50_000),
        "a".to_owned() + &"{1}".repeat(50_000),
        "()".repeat(40_000),
        "(a?)".repeat(1000),
        "(".repeat(64) + "a" + &"*".repeat(64) + &")*".repeat(65),
    ] {
        assert!(
            matches!(Regex::new(&pattern, false), Err(Error::Limit)),
            "{pattern:.30}"
        );
    }
    let text = "a".repeat(2_000_000) + "b";
    assert_eq!(
        Regex::new("b", false)
            .unwrap()
            .find(&text)
            .unwrap()
            .unwrap()
            .captures[0],
        Some(2_000_000..2_000_001)
    );
    assert!(
        Regex::new("x", false)
            .unwrap()
            .find(&text)
            .unwrap()
            .is_none()
    );
}

#[test]
fn regex_random_patterns_do_not_panic() {
    let mut seed = 0x1234_5678_u64;
    let alphabet = b"ab.[]()|*+?{}^$\\:0123,-";
    for _ in 0..10_000 {
        let mut pattern = String::new();
        for _ in 0..(seed % 32) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            pattern.push(char::from(
                alphabet[usize::try_from(seed % alphabet.len() as u64).unwrap()],
            ));
        }
        if let Ok(r) = Regex::new(&pattern, seed.is_multiple_of(2)) {
            let _ = r.find("aaabé\nbbb");
        }
        seed = seed.wrapping_add(1);
    }
}

/// Review repros (`high_regex_work_budget_silently_turns`); expected spans from
/// glibc 2.43 `regexec` in C.UTF-8 (python3 ctypes, as `tests/regex_cases.sh`).
#[test]
fn regex_backreferences_on_long_text_match_like_glibc() {
    // review_fmt_regex_2: a shared cross-start budget made these fail with
    // Limit after ~1.9k chars. glibc 2.43 regexec spans (via ctypes). The
    // backtracker is quadratic here (D-002), so the second text is short.
    let cases: [(&str, String, std::ops::Range<usize>); 2] = [
        ("(\\w+) \\1", "w".repeat(1900) + " foo foo", 1901..1908),
        ("([0-9]+)-\\1", "1".repeat(600) + " 12-12", 601..606),
    ];
    for (pattern, text, expected) in cases {
        let found = Regex::new(pattern, false).unwrap().find(&text).unwrap();
        assert_eq!(
            found.and_then(|m| m.captures[0].clone()),
            Some(expected),
            "{pattern}"
        );
    }
}

#[test]
fn regex_long_texts_match_like_glibc() {
    let ones = "1".repeat(710) + " 5%";
    let cases: [(&str, String, Option<std::ops::Range<usize>>); 7] = [
        ("[0-9]+%", ones.clone(), Some(711..713)),
        ("[a-z]+:", "a".repeat(710) + " b:", Some(711..713)),
        ("\\w+@", "a".repeat(710) + " b@", Some(711..713)),
        ("[^ ]+ x", "y".repeat(1000) + " x", Some(0..1002)),
        ("(a|b)*c", "ab".repeat(168) + " c", Some(337..338)),
        (".*z|q", "y".repeat(1000) + "q", Some(1000..1001)),
        ("[0-9]+%", "1".repeat(710), None),
    ];
    for (pattern, text, expected) in cases {
        let found = Regex::new(pattern, false).unwrap().find(&text).unwrap();
        assert_eq!(
            found.map(|m| m.captures[0].clone().unwrap()),
            expected,
            "{pattern}"
        );
    }
}

/// 100k-character texts: each search must finish in well under a second
/// even in a debug build (no work cap applies to these patterns).
#[test]
fn regex_without_backreferences_is_linear() {
    let digits = "1".repeat(100_000);
    let pairs = "x".repeat(50_000) + "," + &"y".repeat(50_000);
    let cases: [(&str, String, Option<Captures>); 5] = [
        (
            "[0-9]+%",
            digits.clone() + " 5%",
            Some(vec![Some(100_001..100_003)]),
        ),
        ("[0-9]+%", digits, None),
        (
            "(a|b)*c",
            "ab".repeat(50_000) + " c",
            Some(vec![Some(100_001..100_002), None]),
        ),
        (
            "(.*),(.*)",
            pairs,
            Some(vec![
                Some(0..100_001),
                Some(0..50_000),
                Some(50_001..100_001),
            ]),
        ),
        ("(x+x+)+y", "x".repeat(100_000), None),
    ];
    for (pattern, text, expected) in cases {
        let regex = Regex::new(pattern, false).unwrap();
        let started = std::time::Instant::now();
        let found = regex.find(&text).unwrap().map(|m| m.captures);
        let elapsed = started.elapsed();
        assert_eq!(found, expected, "{pattern}");
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "{pattern}: {elapsed:?}"
        );
        let started = std::time::Instant::now();
        assert_eq!(regex.is_match(&text), Ok(expected.is_some()), "{pattern}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "{pattern}"
        );
    }
}

/// `REG_NOTBOL`: `^` never matches at the start of the text, in the NFA and
/// in the backtracker; `$` still does at its end.
#[test]
fn not_bol() {
    for pattern in ["^ab", "^(a)\\1"] {
        let text = if pattern.contains('\\') { "aa" } else { "ab" };
        let r = Regex::new(pattern, false).unwrap();
        assert!(r.find_with(text, false).unwrap().is_some(), "{pattern}");
        assert!(r.find_with(text, true).unwrap().is_none(), "{pattern}");
    }
    let r = Regex::new("b$", false).unwrap();
    assert!(r.find_with("ab", true).unwrap().is_some());
}

/// A pattern every branch of which starts with `^` is tried at the text's
/// start only; one with a branch that does not is tried everywhere.
#[test]
fn regex_anchored_patterns_start_only_at_zero() {
    let span = |p: &str, t: &str, not_bol: bool| {
        Regex::new(p, false)
            .unwrap()
            .find_with(t, not_bol)
            .unwrap()
            .and_then(|m| m.captures[0].clone())
    };
    assert_eq!(span("^5", "15 some", false), None);
    assert_eq!(span("^5", "5x", false), Some(0..1));
    assert_eq!(span("^5", "5x", true), None);
    assert_eq!(span("^5.*0.some", "50 some", false), Some(0..7));
    assert_eq!(span("(^a|^b)c", "xbc", false), None);
    assert_eq!(span("(^a|^b)c", "bc", false), Some(0..2));
    assert_eq!(span("a|^b", "xb", false), None);
    assert_eq!(span("a|^b", "xa", false), Some(1..2));
    assert_eq!(span("(^a)|b", "xb", false), Some(1..2));
    let long = "a".repeat(100_000) + "b";
    assert_eq!(span("^b", &long, false), None);
    assert_eq!(span("^a*b", &long, false), Some(0..100_001));
}

/// The skip to the first byte a match can begin with changes no answer: the
/// same span with and without it, over patterns that can and cannot match
/// the empty string, anchors and word assertions, case, non-ASCII text and a
/// byte that is not UTF-8.
#[test]
fn skipping_to_a_first_byte_changes_no_span() {
    let patterns = [
        "a",
        "été",
        "é",
        "\u{4e2d}",
        "f[a-z]+r",
        "[0-9]+",
        "x+y",
        "^5",
        "5$",
        "\\<fi",
        "\\bfiller",
        "\\Bil",
        "(a|b)c",
        "a*",
        "a?b",
        "x*|y",
        "(^|,)b",
        "[^a-z]+",
        ".",
        ".+z",
        "[[:digit:]]{2}",
        "[\u{e9}a]x",
        "K",
        "s+",
        "\u{212a}",
        "a{0}b",
    ];
    let texts = [
        "",
        "123 some filler text here   ",
        "abcabc, bcd",
        "un \u{e9}t\u{e9} chaud",
        "\u{c9}T\u{c9} 5",
        "\u{4e2d}\u{6587} 55",
        "xxy zzz",
        "kK\u{212a}sS\u{17f}",
        "a\0b",
        "ab\u{10ff01}cd",
        "   ",
    ];
    let bytes = crate::rawstr::from_bytes(b"ab\xffcd 5");
    for pattern in patterns {
        for ignore_case in [false, true] {
            let re = Regex::new(pattern, ignore_case).unwrap();
            let Some(nfa) = &re.nfa else { continue };
            for text in texts.iter().copied().chain(std::iter::once(bytes.as_str())) {
                for not_bol in [false, true] {
                    assert_eq!(
                        nfa.span_skipping(text, not_bol, true),
                        nfa.span_skipping(text, not_bol, false),
                        "{pattern:?} {text:?} i={ignore_case} not_bol={not_bol}"
                    );
                }
            }
        }
    }
}

/// `span_with` gives capture 0 of `find_with` for every pattern, with
/// groups or not.
#[test]
fn span_with_is_capture_zero() {
    for pattern in [
        "a+", "(a|b)+c", "^a", "(^a|b)+", "x*", "(a*)\\1b", "été", "[0-9]+",
    ] {
        for text in ["", "aab", "abab", "baabc", "123", "un été", "aab"] {
            let re = Regex::new(pattern, false).unwrap();
            for not_bol in [false, true] {
                let whole = re.find_with(text, not_bol).unwrap();
                let span = re.span_with(text, not_bol).unwrap();
                assert_eq!(
                    span,
                    whole.and_then(|m| m.captures[0].clone()),
                    "{pattern:?} {text:?} not_bol={not_bol}"
                );
            }
        }
    }
}
