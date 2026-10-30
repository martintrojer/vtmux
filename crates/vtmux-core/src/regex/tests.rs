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
