use super::*;
type Row = (
    &'static str,
    &'static str,
    bool,
    Result<Option<Vec<Option<Range<usize>>>>, Error>,
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
