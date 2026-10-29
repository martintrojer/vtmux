//! The evaluator: `format_expand1` (format.c:6708) and `format_replace` (6019) over the
//! typed AST.

use super::text;
use super::{
    CmpOp, EnvScope, Eval, Format, FormatContext, LOOP_LIMIT, Node, Post, SearchFlags, Value,
    VarMods, colour, is_true, num, parse,
};
use std::fmt::Write as _;

/// `FORMAT_MAX_WIDTH`, `FORMAT_MAX_REPEAT` and `FORMAT_MAX_REPEAT_SIZE` (format.c:89-95).
const MAX_WIDTH: i64 = 10_000;
const MAX_REPEAT: i64 = 10_000;
const MAX_REPEAT_SIZE: usize = 65_536;

/// The context of one loop iteration: extra variables over the parent context.
struct Overlay<'a> {
    parent: &'a dyn FormatContext,
    vars: Vec<(&'static str, String)>,
}

impl FormatContext for Overlay<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| self.parent.get(name))
    }

    fn environ(&self, scope: EnvScope) -> Option<Vec<super::EnvVar>> {
        self.parent.environ(scope)
    }

    fn window_name_exists(&self, name: &str) -> Option<bool> {
        self.parent.window_name_exists(name)
    }

    fn session_name_exists(&self, name: &str) -> bool {
        self.parent.session_name_exists(name)
    }

    fn search_pane(&self, pattern: &str, flags: SearchFlags) -> Option<u32> {
        self.parent.search_pane(pattern, flags)
    }
}

fn flag01(b: bool) -> String {
    String::from(if b { "1" } else { "0" })
}

impl Eval<'_> {
    /// `format_expand1`. `depth` is tmux's `es->loop` on entry; every nested
    /// expansion (branches, operands, `E`) enters one deeper (format.c:6720-6724).
    /// Empty input returns before any time or depth accounting (format.c:6717),
    /// so no-op expansions are free.
    pub(super) fn expand(&mut self, fmt: &Format, depth: usize) -> String {
        if fmt.nodes.is_empty() || !self.in_time() || depth >= LOOP_LIMIT {
            return String::new();
        }
        let mut out = String::new();
        for node in &fmt.nodes {
            match node {
                Node::Text(t) => out.push_str(t),
                Node::Deferred(_) => {}
                Node::Abort => break,
                Node::Replace { value, post } => match self.replace(value, post, depth) {
                    Some(v) => out.push_str(&v),
                    // `format_replace` failed: tmux ends this expansion here.
                    None => break,
                },
            }
        }
        out
    }

    /// `format_check_time` (format.c:4750). tmux also polls it every
    /// `FORMAT_TIME_LOOP_CHECK` characters inside its scanning loops; parsing
    /// here is linear in the template, so only expansions check.
    fn in_time(&self) -> bool {
        self.clock.now_ms().saturating_sub(self.start) < super::TIME_LIMIT_MS
    }

    /// One `#{...}`: find the value, then the steps after the `done:` label
    /// (format.c:6602-6682). `None` is tmux's `goto fail`.
    fn replace(&mut self, value: &Value, post: &Post, depth: usize) -> Option<String> {
        let inner = depth + 1;
        let mut v = self.value(value, inner)?;
        if post.reexpand {
            // format.c:6606: expand the result again, one level down.
            let ast = parse::template(&v, inner, &mut Vec::new());
            v = self.expand(&ast, inner);
        }
        if let Some(limit) = &post.limit {
            let limit = self.number(limit, inner);
            let marker = post.marker.as_ref().map(|m| self.expand(m, inner));
            v = trim(&v, limit, marker.as_deref());
        }
        if let Some(pad) = &post.pad {
            let width = self.number(pad, inner);
            v = if width > 0 {
                text::pad_right(&v, usize::try_from(width).unwrap_or(0))
            } else {
                text::pad_left(&v, usize::try_from(-width).unwrap_or(0))
            };
        }
        if post.length {
            v = v.len().to_string();
        }
        if post.width {
            v = text::width(&v).to_string();
        }
        Some(v)
    }

    /// An argument read with `strtonum(-10000, 10000)`; an error is 0 (format.c:6083).
    fn number(&mut self, arg: &Format, depth: usize) -> i64 {
        let s = self.expand(arg, depth);
        num::strtonum(&s, -MAX_WIDTH, MAX_WIDTH).unwrap_or(0)
    }

    /// `format_find`'s post-processing of a found variable (format.c:4691-4740).
    /// `format_find` (format.c:4610) followed by the modifiers it applies: `None` when
    /// the variable is not found.
    fn find(&mut self, name: &str, vm: &VarMods, depth: usize) -> Option<String> {
        if vm.time {
            // format.c:4681-4709: a time variable, or a variable whose value is a
            // positive number, as `ctime`; anything else (zero, negative, text,
            // missing) is not found. It returns before the other modifiers apply.
            let t = self.ctx.time(name).or_else(|| {
                let v = self.ctx.get(name)?;
                num::strtonum(&v, 0, i64::MAX)
            });
            return match t {
                Some(t) if t > 0 => self.ctx.local_time(t).map(|tm| tm.ctime()),
                _ => None,
            };
        }
        let v = self.ctx.get(name)?;
        Some(self.var_mods(v, vm, depth))
    }

    fn var_mods(&mut self, mut v: String, vm: &VarMods, depth: usize) -> String {
        if vm.basename {
            v = text::basename(&v);
        }
        if vm.dirname {
            v = text::dirname(&v);
        }
        // `q` flags are read from the expanded arguments (format.c:6150-6165).
        let (mut shell, mut single, mut style, mut args) = (false, false, false, false);
        for q in &vm.quote {
            let Some(q) = q else {
                shell = true;
                continue;
            };
            let a = self.expand(q, depth);
            if a.contains('s') {
                single = true;
            } else if a.contains(['e', 'h']) {
                style = true;
            } else if a.contains('a') {
                args = true;
            }
        }
        if shell {
            v = text::quote_shell(&v);
        }
        if single {
            v = text::quote_shell_single(&v);
        }
        if style {
            v = text::quote_style(&v);
        }
        if args {
            v = text::args_escape(&v);
        }
        v
    }

    #[allow(clippy::too_many_lines)]
    fn value(&mut self, value: &Value, inner: usize) -> Option<String> {
        Some(match value {
            Value::Var { name, vm } => self.find(name, vm, inner).unwrap_or_default(),
            Value::Inner(f) => self.expand(f, inner),
            Value::Literal(s) => s.clone(),
            Value::Char(body) => {
                let new = self.expand(body, inner);
                match num::strtonum(&new, 32, 126).and_then(|c| u8::try_from(c).ok()) {
                    Some(c) => String::from(char::from(c)),
                    None => String::new(),
                }
            }
            Value::Colour { flags, body } => {
                let (mut fg, mut bg) = (false, false);
                for f in flags {
                    let f = self.expand(f, inner);
                    fg |= f.contains('f');
                    bg |= f.contains('b');
                }
                let new = self.expand(body, inner);
                if fg || bg {
                    if new.eq_ignore_ascii_case("none") {
                        String::from("\x1b[0m")
                    } else {
                        colour::from_string(&new)
                            .and_then(|c| colour::to_escape(c, bg))
                            .unwrap_or_default()
                    }
                } else {
                    colour::from_string(&new)
                        .and_then(colour::to_rgb)
                        .map(|rgb| format!("{:06x}", rgb & 0x00ff_ffff))
                        .unwrap_or_default()
                }
            }
            Value::Environ { flags, body } => {
                let flags = flags.as_ref().map(|f| self.expand(f, inner));
                let scope = match flags.as_deref() {
                    None | Some("" | "s") => Some(EnvScope::Session),
                    Some("g") => Some(EnvScope::Global),
                    Some("c") => Some(EnvScope::Client),
                    Some(_) => None,
                };
                let Some(vars) = scope.and_then(|s| self.ctx.environ(s)) else {
                    return Some(String::new());
                };
                let last = vars.len().saturating_sub(1);
                let mut out = String::new();
                for (i, var) in vars.iter().enumerate() {
                    let overlay = Overlay {
                        parent: self.ctx,
                        vars: vec![
                            ("environ_name", var.name.clone()),
                            ("environ_value", var.value.clone().unwrap_or_default()),
                            ("environ_hidden", flag01(var.hidden)),
                            ("environ_removed", flag01(var.value.is_none())),
                            ("loop_last_flag", flag01(i == last)),
                            ("loop_index", i.to_string()),
                        ],
                    };
                    let mut sub = Eval {
                        ctx: &overlay,
                        clock: self.clock,
                        start: self.start,
                    };
                    out.push_str(&sub.expand(body, inner));
                }
                out
            }
            Value::Name {
                flags,
                body,
                fallback,
            } => {
                let (mut window, mut session) = (false, false);
                for f in flags {
                    match f {
                        None => window = true,
                        Some(f) => {
                            let f = self.expand(f, inner);
                            if f.contains('w') {
                                window = true;
                            } else if f.contains('s') {
                                session = true;
                            }
                        }
                    }
                }
                if window {
                    // format_window_name: no session fails (format.c:5301).
                    let name = self.expand(body, inner);
                    flag01(self.ctx.window_name_exists(&name)?)
                } else if session {
                    let name = self.expand(body, inner);
                    flag01(self.ctx.session_name_exists(&name))
                } else {
                    return self.value(fallback.as_deref()?, inner);
                }
            }
            Value::Search { flags, body } => {
                let flags = flags.as_ref().map(|f| self.expand(f, inner));
                let pattern = self.expand(body, inner);
                let flags = SearchFlags {
                    ignore_case: flags.as_deref().is_some_and(|f| f.contains('i')),
                    regex: flags.as_deref().is_some_and(|f| f.contains('r')),
                };
                self.ctx
                    .search_pane(&pattern, flags)
                    .unwrap_or(0)
                    .to_string()
            }
            Value::Repeat { left, right } => {
                let left = self.expand(left, inner);
                let right = self.expand(right, inner);
                match num::strtonum(&right, 1, MAX_REPEAT).and_then(|n| usize::try_from(n).ok()) {
                    Some(n) if left.is_empty() || n <= MAX_REPEAT_SIZE / left.len() => {
                        left.repeat(n)
                    }
                    _ => String::new(),
                }
            }
            Value::Not { negate, body } => {
                let t = is_true(&self.expand(body, inner));
                flag01(t != *negate)
            }
            // format_bool_op_n (format.c:5176): stop at the first deciding operand.
            Value::Or(parts) => flag01(parts.iter().any(|p| is_true(&self.expand(p, inner)))),
            Value::And(parts) => flag01(parts.iter().all(|p| is_true(&self.expand(p, inner)))),
            Value::Cmp { op, left, right } => {
                let l = self.expand(left, inner);
                let r = self.expand(right, inner);
                // strcmp: bytewise, which is also how `str` orders.
                flag01(match op {
                    CmpOp::Eq => l == r,
                    CmpOp::Ne => l != r,
                    CmpOp::Lt => l < r,
                    CmpOp::Gt => l > r,
                    CmpOp::Le => l <= r,
                    CmpOp::Ge => l >= r,
                })
            }
            Value::Cond { arms, default, vm } => {
                for arm in arms {
                    // format.c:6523-6544.
                    let found = if let Some(v) = self.find(&arm.raw, vm, inner) {
                        v
                    } else {
                        let e = self.expand(&arm.test, inner);
                        if e == arm.raw { String::new() } else { e }
                    };
                    if is_true(&found) {
                        return Some(self.expand(&arm.then, inner));
                    }
                }
                default
                    .as_ref()
                    .map(|d| self.expand(d, inner))
                    .unwrap_or_default()
            }
            Value::Expr { args, operands } => {
                let args: Vec<String> = args.iter().map(|a| self.expand(a, inner)).collect();
                let Some((l, r)) = operands else {
                    return Some(String::new());
                };
                let l = self.expand(l, inner);
                let r = self.expand(r, inner);
                // Any failure is an empty value, not an abort (format.c:6586).
                num::expression(&args, &l, &r).unwrap_or_default()
            }
        })
    }
}

/// The `=` modifier (format.c:6623-6648): keep `limit` columns from the left, or from
/// the right when negative; a marker marks what was cut.
fn trim(v: &str, limit: i64, marker: Option<&str>) -> String {
    let (keep, from_right) = match limit {
        0 => return v.to_owned(),
        l if l > 0 => (usize::try_from(l).unwrap_or(0), false),
        l => (usize::try_from(-l).unwrap_or(0), true),
    };
    let new = if from_right {
        text::trim_right(v, keep)
    } else {
        text::trim_left(v, keep)
    };
    match marker {
        Some(m) if new != v => {
            let mut out = String::new();
            if from_right {
                let _ = write!(out, "{m}{new}");
            } else {
                let _ = write!(out, "{new}{m}");
            }
            out
        }
        _ => new,
    }
}
