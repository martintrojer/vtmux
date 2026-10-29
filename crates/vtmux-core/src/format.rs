//! Format (#{...}) expansion for the M0 subset.
//!
//! A template is parsed once into a typed AST ([`parse`]) and evaluated against a
//! [`FormatContext`] ([`expand`]). Behaviour is tmux 3.8's `format.c`, read as a
//! spec; the structure is not. In tmux, parsing and evaluation are one string
//! rewriting pass (`format_expand1`), so the places where evaluation can stop
//! early (an unterminated `#{`, a `#{==:a}` without a comma) are decided at
//! parse time here and show up as [`Node::Abort`].
//!
//! Supported: variables and the `#S #I ...` aliases, `#{?c,v,...}`, `#{||:...}`,
//! `#{&&:...}`, `#{==:a,b}`, `#{!=:a,b}`, `#{E:x}`, `#{T:x}` (like `E`: the
//! strftime pass is deferred), the `##` `#,` `#}` escapes and `#[style]`
//! pass-through. Every other modifier parses to [`Node::Deferred`] and expands
//! to nothing; [`Parsed::deferred`] lists them so the server can log them.
//! `#(cmd)` also expands to nothing (jobs are the server's business, D9) and is
//! reported in `deferred` as `#()`.

mod parse;

#[cfg(test)]
mod golden;

/// `FORMAT_LOOP_LIMIT` (format.c:137): the deepest allowed nesting of expansions.
pub(crate) const LOOP_LIMIT: usize = 100;

/// Budget of nested re-expansions per [`expand`]: a deterministic stand-in for
/// tmux's wall-clock `FORMAT_TIME_LIMIT` (format.c:140), which stops runaway
/// re-expansion such as `@a='#{E:@a}#{E:@a}'`. Only an `E:`/`T:` that runs
/// inside another one is charged: that is the only way to make work grow
/// beyond the size of the template, so literal text, conditionals and
/// top-level `E:` are never truncated. Once spent, further nested
/// re-expansions yield the empty string, as in tmux once the time limit is
/// reached (format.c:6713).
const REEXPAND_LIMIT: u32 = 100_000;

/// Where variable values come from. The server implements this over its
/// options, environment and the target session/window/pane.
pub trait FormatContext {
    /// The value of `name`, or `None` when no such variable exists.
    fn get(&self, name: &str) -> Option<String>;
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Format {
    nodes: Vec<Node>,
}

/// The result of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub ast: Format,
    /// Constructs that parsed but are not evaluated yet: each modifier name of
    /// every unsupported modifier list, and `#()` for shell jobs.
    pub deferred: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Replace {
        value: Value,
        reexpand: bool,
    },
    /// An unsupported modifier list (first modifier named); expands to nothing.
    Deferred(String),
    /// tmux stops expanding here and returns what it has so far.
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    /// Look the (raw) name up.
    Var(String),
    /// The body itself contains `#{`: expand it as a template (format.c:6575).
    Inner(Format),
    /// `||` (any) and `&&` (all), n-ary and short-circuiting.
    Or(Vec<Format>),
    And(Vec<Format>),
    Cmp {
        ne: bool,
        left: Format,
        right: Format,
    },
    Cond {
        arms: Vec<CondArm>,
        default: Option<Format>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CondArm {
    /// The condition as written: tried as a variable name first.
    raw: String,
    /// The same text as a template, tried when no such variable exists.
    test: Format,
    then: Format,
}

/// Parse `template`. Never fails and never panics: malformed input follows
/// tmux's lenient rules (see [`Node::Abort`]).
#[must_use]
pub fn parse(template: &str) -> Parsed {
    let mut deferred = Vec::new();
    // tmux works on C strings: a NUL ends the template.
    let template = template.split('\0').next().unwrap_or_default();
    let ast = parse::template(template, 0, &mut deferred);
    Parsed { ast, deferred }
}

/// Expand a parsed template.
#[must_use]
pub fn expand(ast: &Format, ctx: &dyn FormatContext) -> String {
    Eval {
        ctx,
        budget: REEXPAND_LIMIT,
        reexpanding: 0,
    }
    .expand(ast, 0)
}

/// Parse and expand in one go.
#[must_use]
pub fn expand_str(template: &str, ctx: &dyn FormatContext) -> String {
    expand(&parse(template).ast, ctx)
}

/// tmux's `format_true` (format.c:4893): neither empty nor `0`.
fn is_true(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

struct Eval<'a> {
    ctx: &'a dyn FormatContext,
    budget: u32,
    /// How many re-expansions are currently open.
    reexpanding: usize,
}

impl Eval<'_> {
    /// `format_expand1`. `depth` is tmux's `es->loop` on entry; every nested
    /// expansion (branches, operands, `E`) enters one deeper (format.c:6720-6724).
    fn expand(&mut self, fmt: &Format, depth: usize) -> String {
        if fmt.nodes.is_empty() || depth >= LOOP_LIMIT {
            return String::new();
        }
        let mut out = String::new();
        for node in &fmt.nodes {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Deferred(_) => {}
                Node::Abort => break,
                Node::Replace { value, reexpand } => {
                    let mut v = self.value(value, depth);
                    if *reexpand {
                        // format.c:6606: expand the result again, one level down.
                        v = self.reexpand(&v, depth + 1);
                    }
                    out.push_str(&v);
                }
            }
        }
        out
    }

    /// Expand a variable's value as a template, charging nested re-expansions.
    fn reexpand(&mut self, v: &str, depth: usize) -> String {
        if self.reexpanding > 0 {
            if self.budget == 0 {
                return String::new();
            }
            self.budget -= 1;
        }
        self.reexpanding += 1;
        let out = self.expand(&parse::template(v, depth, &mut Vec::new()), depth);
        self.reexpanding -= 1;
        out
    }

    fn value(&mut self, value: &Value, depth: usize) -> String {
        let inner = depth + 1;
        match value {
            Value::Var(name) => self.ctx.get(name).unwrap_or_default(),
            Value::Inner(f) => self.expand(f, inner),
            // format_bool_op_n (format.c:5176): stop at the first deciding operand.
            Value::Or(parts) => bool_str(parts.iter().any(|p| is_true(&self.expand(p, inner)))),
            Value::And(parts) => bool_str(parts.iter().all(|p| is_true(&self.expand(p, inner)))),
            Value::Cmp { ne, left, right } => {
                let l = self.expand(left, inner);
                let r = self.expand(right, inner);
                bool_str((l == r) != *ne)
            }
            Value::Cond { arms, default } => {
                for arm in arms {
                    // format.c:6523-6544.
                    let found = if let Some(v) = self.ctx.get(&arm.raw) {
                        v
                    } else {
                        let e = self.expand(&arm.test, inner);
                        if e == arm.raw { String::new() } else { e }
                    };
                    if is_true(&found) {
                        return self.expand(&arm.then, inner);
                    }
                }
                default
                    .as_ref()
                    .map(|d| self.expand(d, inner))
                    .unwrap_or_default()
            }
        }
    }
}

fn bool_str(b: bool) -> String {
    String::from(if b { "1" } else { "0" })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct Map(HashMap<String, String>);

    impl FormatContext for Map {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
    }

    fn map(pairs: &[(&str, &str)]) -> Map {
        Map(pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect())
    }

    #[test]
    fn deferred_modifiers_are_reported() {
        let p = parse("a#{l:x}b#{=3:@n}#{s/a/b/;E:@n}#(true)#{@v}");
        assert_eq!(p.deferred, ["l", "=", "s", "#()"]);
        assert_eq!(expand(&p.ast, &map(&[("@v", "v")])), "abv");
        assert_eq!(
            parse("#{?@x,a,b}#{E:@y}#{||:1,0}#S").deferred,
            Vec::<String>::new()
        );
    }

    #[test]
    fn expand_str_matches_parse_then_expand() {
        let ctx = map(&[("session_name", "main")]);
        assert_eq!(expand_str("[#S]", &ctx), "[main]");
        assert_eq!(expand_str("[#{session_name}]", &ctx), "[main]");
    }

    #[test]
    fn runaway_reexpansion_terminates() {
        // Exponential if unchecked: tmux stops it with a time limit (format.c:140).
        let ctx = map(&[("@a", "#{E:@a}#{E:@a}x")]);
        let out = expand_str("#{E:@a}", &ctx);
        assert!(out.len() < 1_000_000);
    }

    #[test]
    fn runaway_reexpansion_is_fast() {
        let ctx = map(&[("@a", "#{E:@a}#{E:@a}")]);
        let start = std::time::Instant::now();
        let out = expand_str("#{E:@a}", &ctx);
        assert_eq!(out, "");
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn large_flat_templates_are_not_truncated() {
        let ctx = map(&[("@x", "1")]);
        let n = 110_000;
        assert_eq!(expand_str(&"#{?@x,y}".repeat(n), &ctx), "y".repeat(n));
        let ctx = map(&[("@x", "1"), ("@e", "y")]);
        assert_eq!(expand_str(&"#{E:@e}".repeat(n), &ctx), "y".repeat(n));
    }

    #[test]
    fn parse_never_panics_on_random_input() {
        // xorshift64*, biased towards the format metacharacters.
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = move || {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            state.wrapping_mul(0x2545_F491_4F6C_DD1D)
        };
        let alphabet = b"#{}[](),:;?=!|&<>/@ESTlsa-0123xy\0\xff\xc3";
        let ctx = map(&[("@x", "1"), ("@a", "#{E:@a}#{?@x,#{E:@a},b}")]);
        for round in 0..10_000 {
            let len = usize::try_from(next() % 64).unwrap();
            let bytes: Vec<u8> = (0..len)
                .map(|_| {
                    let r = next();
                    if round % 2 == 0 {
                        u8::try_from(r >> 56).unwrap()
                    } else {
                        alphabet[usize::try_from((r >> 33) % alphabet.len() as u64).unwrap()]
                    }
                })
                .collect();
            let template = String::from_utf8_lossy(&bytes);
            let _ = expand_str(&template, &ctx);
        }
    }
}
