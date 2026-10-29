//! C number handling for the `e` expression modifier and the numeric arguments of the
//! other modifiers: `strtonum`, `strtod` (C locale), `(long long)` casts and
//! `printf("%.*f")`, as the reference tmux 3.8 on x86-64 glibc does them.

/// `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// tmux's `strtonum` (compat/strtonum.c): base-10 `strtoll` of the whole string, within
/// `min..=max`. `None` is any of its errors (invalid, too small, too large).
pub(super) fn strtonum(s: &str, min: i64, max: i64) -> Option<i64> {
    let b = s.as_bytes();
    let mut i = 0;
    while b.get(i).is_some_and(|&c| is_space(c)) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let digits = &b[i..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut v: i128 = 0;
    for &d in digits {
        v = (v * 10 + i128::from(d - b'0')).min(i128::from(i64::MAX) + 2);
    }
    let v = if neg { -v } else { v };
    let v = i64::try_from(v).ok()?;
    (min..=max).contains(&v).then_some(v)
}

fn starts_with_ci(b: &[u8], word: &str) -> bool {
    b.len() >= word.len() && b[..word.len()].eq_ignore_ascii_case(word.as_bytes())
}

/// glibc `strtod` in the C locale: the value and how many bytes it consumed. Zero bytes
/// consumed means there was no conversion (the value is then 0).
#[allow(clippy::many_single_char_names)]
pub(super) fn strtod(s: &str) -> (f64, usize) {
    let b = s.as_bytes();
    let mut i = 0;
    while b.get(i).is_some_and(|&c| is_space(c)) {
        i += 1;
    }
    let neg = match b.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let signed = |v: f64| if neg { -v } else { v };
    let rest = &b[i..];

    if starts_with_ci(rest, "inf") {
        let n = if starts_with_ci(rest, "infinity") {
            8
        } else {
            3
        };
        return (signed(f64::INFINITY), i + n);
    }
    if starts_with_ci(rest, "nan") {
        let mut n = 3;
        if rest.get(3) == Some(&b'(') {
            let body = rest[4..]
                .iter()
                .take_while(|c| c.is_ascii_alphanumeric() || **c == b'_')
                .count();
            if rest.get(4 + body) == Some(&b')') {
                n = 5 + body;
            }
        }
        return (signed(f64::from_bits(0x7ff8_0000_0000_0000)), i + n);
    }
    if rest.first() == Some(&b'0') && rest.get(1).is_some_and(|c| c.eq_ignore_ascii_case(&b'x')) {
        if let Some((v, n)) = hex_float(&rest[2..]) {
            return (signed(v), i + 2 + n);
        }
        // "0x" without hex digits converts just the "0".
        return (signed(0.0), i + 1);
    }

    let int = rest.iter().take_while(|c| c.is_ascii_digit()).count();
    let mut n = int;
    let mut frac = 0;
    if rest.get(n) == Some(&b'.') {
        frac = rest[n + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        if int + frac > 0 {
            n += 1 + frac;
        }
    }
    if int + frac == 0 {
        return (0.0, 0);
    }
    if rest.get(n).is_some_and(|c| c.eq_ignore_ascii_case(&b'e')) {
        let mut e = n + 1;
        if matches!(rest.get(e), Some(b'+' | b'-')) {
            e += 1;
        }
        let ed = rest[e..].iter().take_while(|c| c.is_ascii_digit()).count();
        if ed > 0 {
            n = e + ed;
        }
    }
    // The text is ASCII digits, '.', 'e' and a sign: Rust's parser accepts exactly that.
    let v = std::str::from_utf8(&rest[..n])
        .ok()
        .and_then(|t| t.parse::<f64>().ok())
        .unwrap_or(0.0);
    (signed(v), i + n)
}

/// The part of a hex float after `0x`: the value and the bytes used, or `None` when
/// there is no hex digit.
fn hex_float(b: &[u8]) -> Option<(f64, usize)> {
    let hex = |c: u8| char::from(c).to_digit(16);
    let mut n = 0;
    let mut mant = 0.0_f64;
    let mut digits = 0;
    while let Some(d) = b.get(n).and_then(|&c| hex(c)) {
        mant = mant * 16.0 + f64::from(d);
        digits += 1;
        n += 1;
    }
    let mut exp: i32 = 0;
    if b.get(n) == Some(&b'.') {
        let mut m = n + 1;
        let mut fd = 0;
        while let Some(d) = b.get(m).and_then(|&c| hex(c)) {
            mant = mant * 16.0 + f64::from(d);
            exp -= 4;
            fd += 1;
            m += 1;
        }
        if digits + fd > 0 {
            digits += fd;
            n = m;
        }
    }
    if digits == 0 {
        return None;
    }
    if b.get(n).is_some_and(|c| c.eq_ignore_ascii_case(&b'p')) {
        let mut e = n + 1;
        let neg = b.get(e) == Some(&b'-');
        if matches!(b.get(e), Some(b'+' | b'-')) {
            e += 1;
        }
        let ed = b[e..].iter().take_while(|c| c.is_ascii_digit()).count();
        if ed > 0 {
            let v: i32 = std::str::from_utf8(&b[e..e + ed])
                .ok()
                .and_then(|t| t.parse().ok())
                .unwrap_or(i32::MAX);
            exp = exp.saturating_add(if neg { -v } else { v });
            n = e + ed;
        }
    }
    Some((mant * 2f64.powi(exp.clamp(-3000, 3000)), n))
}

/// C `(long long)x` on x86-64: out of range and NaN give `LLONG_MIN`.
#[allow(clippy::cast_possible_truncation)]
pub(super) fn to_long_long(x: f64) -> i64 {
    if x.is_nan() || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&x) {
        i64::MIN
    } else {
        x as i64
    }
}

/// `printf("%.*f", prec, x)`; a negative precision means the default, 6.
pub(super) fn fixed(x: f64, prec: i32) -> String {
    if x.is_nan() {
        return String::from(if x.is_sign_negative() { "-nan" } else { "nan" });
    }
    let prec = usize::try_from(prec).unwrap_or(6);
    format!("{x:.prec$}")
}

/// The operators of `e|op|` (format.c:5822-5846).
#[derive(Clone, Copy)]
enum Op {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

/// `format_replace_expression` (format.c:5811) once the operands are expanded: `args` is
/// the modifier's argument list (operator, flags, precision) and `None` is tmux's failure
/// (the caller turns it into an empty value).
pub(super) fn expression(args: &[String], left: &str, right: &str) -> Option<String> {
    let op = match args.first()?.as_str() {
        "+" => Op::Add,
        "-" => Op::Sub,
        "*" => Op::Mul,
        "/" => Op::Div,
        "%" | "m" => Op::Mod,
        "==" => Op::Eq,
        "!=" => Op::Ne,
        ">" => Op::Gt,
        "<" => Op::Lt,
        ">=" => Op::Ge,
        "<=" => Op::Le,
        _ => return None,
    };
    let use_fp = args.get(1).is_some_and(|f| f.contains('f'));
    let mut prec: i32 = if use_fp { 2 } else { 0 };
    if let Some(p) = args.get(2) {
        // A negative precision reaches printf as such, which means "default".
        prec = i32::try_from(strtonum(p, -100, 100)?).ok()?;
    }
    let operand = |s: &str| {
        let (v, used) = strtod(s);
        (used == s.len()).then_some(v)
    };
    let (mut l, mut r) = (operand(left)?, operand(right)?);
    if !use_fp {
        #[allow(clippy::cast_precision_loss)]
        {
            l = to_long_long(l) as f64;
            r = to_long_long(r) as f64;
        }
    }
    let mut result = match op {
        Op::Add => l + r,
        Op::Sub => l - r,
        Op::Mul => l * r,
        Op::Div => l / r,
        Op::Mod => l % r,
        Op::Eq => f64::from(u8::from((l - r).abs() < 1e-9)),
        Op::Ne => f64::from(u8::from((l - r).abs() > 1e-9)),
        Op::Gt => f64::from(u8::from(l > r)),
        Op::Ge => f64::from(u8::from(l >= r)),
        Op::Lt => f64::from(u8::from(l < r)),
        Op::Le => f64::from(u8::from(l <= r)),
    };
    // An invalid operation gives x86's default NaN, which has the sign bit set
    // ("-nan" in glibc's printf); a NaN operand propagates as it is.
    if result.is_nan() && !l.is_nan() && !r.is_nan() {
        result = f64::from_bits(0xfff8_0000_0000_0000);
    }
    #[allow(clippy::cast_precision_loss)]
    let shown = if use_fp {
        result
    } else {
        to_long_long(result) as f64
    };
    Some(fixed(shown, prec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strtonum_follows_strtoll() {
        assert_eq!(strtonum("42", 0, 100), Some(42));
        assert_eq!(strtonum(" +7", 0, 100), Some(7));
        assert_eq!(strtonum("-3", -10, 10), Some(-3));
        assert_eq!(strtonum("101", 0, 100), None);
        assert_eq!(strtonum("", 0, 100), None);
        assert_eq!(strtonum("5x", 0, 100), None);
        assert_eq!(strtonum("99999999999999999999", 0, i64::MAX), None);
    }

    #[test]
    fn strtod_consumes_what_glibc_does() {
        assert_eq!(strtod("1.5x"), (1.5, 3));
        assert_eq!(strtod("0x10"), (16.0, 4));
        assert_eq!(strtod("0x"), (0.0, 1));
        assert_eq!(strtod("1e"), (1.0, 1));
        assert_eq!(strtod(" 5"), (5.0, 2));
        assert_eq!(strtod(" "), (0.0, 0));
        assert_eq!(strtod("-inf"), (f64::NEG_INFINITY, 4));
        assert_eq!(strtod("nan(a_1)").1, 8);
        assert_eq!(strtod(".").1, 0);
    }

    #[test]
    fn long_long_cast_is_x86() {
        assert_eq!(to_long_long(1.9), 1);
        assert_eq!(to_long_long(-1.9), -1);
        assert_eq!(to_long_long(1e30), i64::MIN);
        assert_eq!(to_long_long(f64::NAN), i64::MIN);
    }
}
