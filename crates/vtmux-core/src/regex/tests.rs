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
