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

/// `FORMAT_TIME_LIMIT` (format.c:140), in milliseconds: how long one
/// [`expand`] may run before every further nested expansion yields the empty
/// string. It stops runaway re-expansion such as `@a='#{E:@a}#{E:@a}'`
/// (format.c:4750, `format_check_time`). Time, not an operation count, so a
/// finite template is never truncated while it finishes within the limit.
pub(crate) const TIME_LIMIT_MS: u64 = 100;

/// A monotonic millisecond clock, injected so tests can drive the time limit
/// deterministically.
pub trait Clock {
    /// Milliseconds since some fixed, arbitrary point; never goes backwards.
    fn now_ms(&self) -> u64;
}

/// The real clock: milliseconds elapsed since it was created.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock(std::time::Instant);

impl SystemClock {
    #[must_use]
    pub fn new() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        u64::try_from(self.0.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

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

/// Expand a parsed template against the real clock.
#[must_use]
pub fn expand(ast: &Format, ctx: &dyn FormatContext) -> String {
    expand_with_clock(ast, ctx, &SystemClock::new())
}

/// Expand a parsed template; `clock` drives the [`TIME_LIMIT_MS`] cut-off.
#[must_use]
pub fn expand_with_clock(ast: &Format, ctx: &dyn FormatContext, clock: &dyn Clock) -> String {
    Eval {
        ctx,
        clock,
        start: clock.now_ms(),
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
    clock: &'a dyn Clock,
    /// When this expansion started (`es->start_time`, format.c:6899).
    start: u64,
}

impl Eval<'_> {
    /// `format_expand1`. `depth` is tmux's `es->loop` on entry; every nested
    /// expansion (branches, operands, `E`) enters one deeper (format.c:6720-6724).
    /// Empty input returns before any time or depth accounting (format.c:6717),
    /// so no-op expansions are free.
    fn expand(&mut self, fmt: &Format, depth: usize) -> String {
        if fmt.nodes.is_empty() || !self.in_time() || depth >= LOOP_LIMIT {
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
                        let ast = parse::template(&v, depth + 1, &mut Vec::new());
                        v = self.expand(&ast, depth + 1);
                    }
                    out.push_str(&v);
                }
            }
        }
        out
    }

    /// `format_check_time` (format.c:4750). tmux also polls it every
    /// `FORMAT_TIME_LOOP_CHECK` characters inside its scanning loops; parsing
    /// here is linear in the template, so only expansions check.
    fn in_time(&self) -> bool {
        self.clock.now_ms().saturating_sub(self.start) < TIME_LIMIT_MS
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

    /// Advances `step` ms on every reading.
    struct FakeClock {
        now: std::cell::Cell<u64>,
        step: u64,
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            let t = self.now.get();
            self.now.set(t + self.step);
            t
        }
    }

    fn ticking(step: u64) -> FakeClock {
        FakeClock {
            now: std::cell::Cell::new(0),
            step,
        }
    }

    #[test]
    fn time_limit_cuts_off_later_expansions() {
        // Readings: start (0), the top-level expansion (1), then one per `E`.
        // Elapsed must stay under TIME_LIMIT_MS, so the top level and 98 `E`s
        // run and the rest are empty.
        let ctx = map(&[("@e", "y")]);
        let ast = parse(&"#{E:@e}".repeat(300)).ast;
        assert_eq!(
            expand_with_clock(&ast, &ctx, &ticking(1)),
            "y".repeat(usize::try_from(TIME_LIMIT_MS - 2).unwrap())
        );
        // A clock that never advances never cuts anything off.
        assert_eq!(expand_with_clock(&ast, &ctx, &ticking(0)), "y".repeat(300));
        // Surrounding text outside the cut-off expansions survives.
        let ast = parse("a#{E:@e}b").ast;
        assert_eq!(expand_with_clock(&ast, &ctx, &ticking(60)), "ab");
        assert_eq!(expand_with_clock(&ast, &ctx, &ticking(40)), "ayb");
    }

    #[test]
    fn empty_expansions_never_read_the_clock() {
        // format.c:6717: empty input returns before format_check_time.
        let ctx = map(&[("@e", "")]);
        let clock = ticking(1);
        let ast = parse("#{E:@e}").ast;
        assert_eq!(expand_with_clock(&ast, &ctx, &clock), "");
        // start + the top-level template; the empty re-expansion adds none.
        assert_eq!(clock.now.get(), 2);
    }

    #[test]
    fn runaway_reexpansion_terminates() {
        // Exponential if unchecked: tmux stops it with a time limit (format.c:140).
        let ctx = map(&[("@a", "#{E:@a}#{E:@a}x")]);
        let start = std::time::Instant::now();
        let _ = expand_str("#{E:@a}", &ctx);
        // The limit is 100 ms; allow generous slack for a loaded CI host.
        assert!(start.elapsed() < std::time::Duration::from_millis(500));
    }

    // The 110k-element cases take 50-90 ms in a debug build, close to the
    // 100 ms limit (tmux itself needs ~70 ms), so they run on a frozen clock;
    // what they pin is that no count-based budget truncates finite output.
    // The real clock gets a small case.

    #[test]
    fn large_flat_templates_are_not_truncated() {
        let n = 110_000;
        let ctx = map(&[("@x", "1"), ("@e", "y")]);
        let frozen = ticking(0);
        let ast = parse(&"#{?@x,y}".repeat(n)).ast;
        assert_eq!(expand_with_clock(&ast, &ctx, &frozen), "y".repeat(n));
        let ast = parse(&"#{E:@e}".repeat(n)).ast;
        assert_eq!(expand_with_clock(&ast, &ctx, &frozen), "y".repeat(n));
        assert_eq!(expand_str(&"#{?@x,y}".repeat(1000), &ctx), "y".repeat(1000));
    }

    #[test]
    fn nested_empty_expansions_do_not_discard_later_output() {
        let b = format!("{}#{{E:@y}}", "#{E:}".repeat(100_000));
        let ctx = map(&[("@y", "y"), ("@b", &b)]);
        let ast = parse("#{E:@b}").ast;
        assert_eq!(expand_with_clock(&ast, &ctx, &ticking(0)), "y");
        let b = format!("{}#{{E:@y}}", "#{E:}".repeat(1000));
        let ctx = map(&[("@y", "y"), ("@b", &b)]);
        assert_eq!(expand_str("#{E:@b}", &ctx), "y");
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
