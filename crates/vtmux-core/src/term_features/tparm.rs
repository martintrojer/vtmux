//! `tparm` (ncurses) for the terminfo strings vtmux writes.

/// A `tparm` parameter.
#[derive(Clone, Copy, Debug)]
pub enum Param<'a> {
    Int(i64),
    Str(&'a str),
}

/// `tparm` of a terminfo string: `%p1`-`%p9`, `%d`, `%s`, `%c`, `%{n}`,
/// `%'c'`, `%l`, arithmetic and logic, `%i`, `%?` ... `%;`. Variables
/// (`%P`, `%g`) and printf flags are in no string vtmux writes and are
/// skipped; a missing parameter is 0.
#[must_use]
pub fn tparm(template: &str, params: &[Param]) -> String {
    let mut params = params.to_vec();
    let bytes = template.as_bytes();
    let mut out = Vec::new();
    let mut stack: Vec<Param> = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let Some(&op) = bytes.get(i + 1) else {
            break;
        };
        i += 2;
        match op {
            b'%' => out.push(b'%'),
            b'c' => out.push(int(stack.pop()).to_le_bytes()[0]),
            b'd' => out.extend_from_slice(int(stack.pop()).to_string().as_bytes()),
            b's' => match stack.pop() {
                Some(Param::Str(s)) => out.extend_from_slice(s.as_bytes()),
                Some(Param::Int(n)) => out.extend_from_slice(n.to_string().as_bytes()),
                None => {}
            },
            b'l' => {
                let len = match stack.pop() {
                    Some(Param::Str(s)) => i64::try_from(s.len()).unwrap_or(0),
                    _ => 0,
                };
                stack.push(Param::Int(len));
            }
            b'p' => {
                let n = bytes
                    .get(i)
                    .map_or(0, |b| usize::from(b.wrapping_sub(b'1')));
                i += 1;
                stack.push(params.get(n).copied().unwrap_or(Param::Int(0)));
            }
            b'P' | b'g' => i += 1,
            b'\'' => {
                stack.push(Param::Int(bytes.get(i).copied().map_or(0, i64::from)));
                i += 2;
            }
            b'{' => {
                let end = bytes[i..]
                    .iter()
                    .position(|&b| b == b'}')
                    .map_or(bytes.len(), |e| i + e);
                stack.push(Param::Int(template[i..end].parse().unwrap_or(0)));
                i = end + 1;
            }
            b'i' => {
                for p in params.iter_mut().take(2) {
                    if let Param::Int(n) = p {
                        *n += 1;
                    }
                }
            }
            b'!' => {
                let a = int(stack.pop());
                stack.push(Param::Int(i64::from(a == 0)));
            }
            b'~' => {
                let a = int(stack.pop());
                stack.push(Param::Int(!a));
            }
            b't' => {
                if int(stack.pop()) == 0 {
                    skip(bytes, &mut i, true);
                }
            }
            // The end of a `then` branch: skip its `else`.
            b'e' => skip(bytes, &mut i, false),
            op => {
                if let Some(f) = binary(op) {
                    let b = int(stack.pop());
                    let a = int(stack.pop());
                    stack.push(Param::Int(f(a, b)));
                }
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A stack value as a number: a string's is its length, an empty stack's 0.
fn int(v: Option<Param>) -> i64 {
    match v {
        Some(Param::Int(n)) => n,
        Some(Param::Str(s)) => i64::try_from(s.len()).unwrap_or(0),
        None => 0,
    }
}

/// The binary operator `%op`, if it is one.
fn binary(op: u8) -> Option<fn(i64, i64) -> i64> {
    Some(match op {
        b'+' => i64::wrapping_add,
        b'-' => i64::wrapping_sub,
        b'*' => i64::wrapping_mul,
        b'/' => |a, b| a.checked_div(b).unwrap_or(0),
        b'm' => |a, b| a.checked_rem(b).unwrap_or(0),
        b'&' => |a, b| a & b,
        b'|' => |a, b| a | b,
        b'^' => |a, b| a ^ b,
        b'=' => |a, b| i64::from(a == b),
        b'<' => |a, b| i64::from(a < b),
        b'>' => |a, b| i64::from(a > b),
        b'A' => |a, b| i64::from(a != 0 && b != 0),
        b'O' => |a, b| i64::from(a != 0 || b != 0),
        _ => return None,
    })
}

/// Move `i` past the `%e` (when `to_else`) or `%;` that ends the branch
/// it is in, skipping nested conditionals.
fn skip(bytes: &[u8], i: &mut usize, to_else: bool) {
    let mut depth = 0;
    while *i + 1 < bytes.len() {
        if bytes[*i] != b'%' {
            *i += 1;
            continue;
        }
        let op = bytes[*i + 1];
        *i += 2;
        match op {
            b'?' => depth += 1,
            b';' if depth == 0 => return,
            b';' => depth -= 1,
            b'e' if depth == 0 && to_else => return,
            _ => {}
        }
    }
    *i = bytes.len();
}
