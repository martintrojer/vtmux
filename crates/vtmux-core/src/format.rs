//! tmux 3.8 format (#{...}) parsing and expansion.
//!
//! A template is parsed once into a typed AST ([`parse`]) and evaluated against a
//! [`FormatContext`] ([`expand`]). Behaviour is tmux 3.8's `format.c`, read as a
//! spec; the structure is not. In tmux, parsing and evaluation are one string
//! rewriting pass (`format_expand1`), so the places where evaluation can stop
//! early (an unterminated `#{`, a `#{==:a}` without a comma) are decided at
//! parse time here and show up as [`Node::Abort`].
//!
//! Supports variables, aliases, conditionals, boolean and string comparisons,
//! arithmetic (`e`), glob/ERE/fuzzy matching (`m`), ERE substitution (`s`),
//! literals/characters (`l`, `a`), paths (`b`, `d`), length/width (`n`, `w`),
//! trimming/padding (`=`, `p`), colours/quoting (`c`, `q`), repeats (`R`),
//! environment loops (`V`), session/window/pane/client/option loops (`S`, `W`, `P`,
//! `L`, `O`, see [`loops`]), name tests (`N`) and pane-search hooks (`C`).
//! `E` re-expands; `T` re-expands after a strftime pass.
//! Bare `t` formats a time variable as `ctime` text. [`expand_time`] runs
//! strftime over the template first. Escapes and `#[style]` pass through.
//! Unsupported modifiers become [`Node::Deferred`] and expand to nothing;
//! [`Parsed::deferred`] lists them so the server can log them.
//! `#(cmd)` is [`Node::Job`]: the command is expanded (no jobs, no cycles, no strftime),
//! handed to [`FormatContext::job`], and the last line of what that returns is expanded
//! again. Running and caching jobs is the server's business (D9); the default hook
//! returns nothing. [`expand_nojobs`] is `FORMAT_EXPAND_NOJOBS`: `#()` is always empty.

mod colour;
mod colour_names;
mod eval;
mod fuzzy;
mod loops;
mod matching;
mod mods;
mod num;
mod parse;
mod text;
mod time;
/// Width and truncation of expanded text that may still hold `#[style]` runs and `##`
/// escapes (`format_width`, `format_trim_left/right`, format-draw.c:1100-1270), for the
/// status line.
pub mod trim {
    pub use super::text::{trim_left, trim_right, width};
}
mod wcwidth;
mod wcwidth_tables;

pub use crate::time::{FixedWallClock, SystemWallClock, Tm, WallClock};

#[cfg(test)]
mod golden;
#[cfg(test)]
mod loop_golden;

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
/// options, environment and the target session/window/pane. Only [`get`] is
/// required; the hooks default to "nothing there".
///
/// [`get`]: FormatContext::get
pub trait FormatContext {
    /// The value of `name`, or `None` when no such variable exists.
    fn get(&self, name: &str) -> Option<String>;

    /// The seconds-since-epoch value of a time variable (`session_created`
    /// ...), for `#{t:name}`. `None` when `name` is not a time.
    fn time(&self, _name: &str) -> Option<i64> {
        None
    }

    /// [`get`](FormatContext::get) without the environment fallback. `format_find`
    /// consults the environment only for non-time lookups (format.c:4666), so
    /// `#{t:TS}` must not see an environment variable `TS`. Contexts that do not
    /// fall back to the environment need not override it.
    fn get_without_environ(&self, name: &str) -> Option<String> {
        self.get(name)
    }

    /// `#{V:...}`: the environment of `scope` in name order, or `None` when there is
    /// none (no session, no client).
    fn environ(&self, _scope: EnvScope) -> Option<Vec<EnvVar>> {
        None
    }

    /// `#{N:name}`: whether the target session has a window called `name`. `None` when
    /// there is no session, which ends the whole expansion (format.c:5301).
    fn window_name_exists(&self, _name: &str) -> Option<bool> {
        None
    }

    /// `#{N/s:name}`: whether a session called `name` exists.
    fn session_name_exists(&self, _name: &str) -> bool {
        false
    }

    /// `#{C:pattern}`: the 1-based number of the first line of the target pane that
    /// matches, `None` (shown as 0) when nothing matches or there is no pane
    /// (`window_pane_search`, window.c:2123).
    fn search_pane(&self, _pattern: &str, _flags: SearchFlags) -> Option<u32> {
        None
    }

    /// `#{S:...}`: every session in the server's own order (by name), each as the
    /// context `format_defaults(c, s, NULL, NULL)` builds (format.c:5258).
    fn sessions(&self) -> Vec<LoopItem<'_>> {
        Vec::new()
    }

    /// `#{W:...}`: the target session's windows in index order; `None` when there is no
    /// session, which ends the whole expansion (format.c:5366).
    fn windows(&self) -> Option<Vec<LoopItem<'_>>> {
        None
    }

    /// `#{P:...}`: the target window's panes in window order; `None` when there is no
    /// window, which ends the whole expansion (format.c:5448).
    fn panes(&self) -> Option<Vec<LoopItem<'_>>> {
        None
    }

    /// `#{L:...}`: the attached clients in the order they connected (format.c:5779).
    fn clients(&self) -> Vec<LoopItem<'_>> {
        Vec::new()
    }

    /// `#{O:...}`: the entries of one options tree in name order, or `None` when there
    /// is no such tree (no session, window or pane for the scope).
    fn options(&self, _scope: OptionScope) -> Option<OptionsLoop<'_>> {
        None
    }

    /// `#(cmd)` (`format_job_get`, format.c:394-460). `raw` is the command as written
    /// (tmux keys its job cache by it and names it in `<'raw' not ready>`); `expanded`
    /// is what to run. Returns the job's output so far, of which only the last line is
    /// used and which is expanded once more; `""` while there is none. A context that
    /// runs no jobs keeps this.
    fn job(&self, _raw: &str, _expanded: &str) -> String {
        String::new()
    }
}

pub use loops::{LoopItem, LoopKeys, LoopOption, OptionScope, OptionsLoop};

/// A borrowed context is a context, so a loop child can be `Box::new(&ctx)`.
impl<T: FormatContext + ?Sized> FormatContext for &T {
    fn get(&self, name: &str) -> Option<String> {
        (**self).get(name)
    }
    fn time(&self, name: &str) -> Option<i64> {
        (**self).time(name)
    }
    fn get_without_environ(&self, name: &str) -> Option<String> {
        (**self).get_without_environ(name)
    }
    fn environ(&self, scope: EnvScope) -> Option<Vec<EnvVar>> {
        (**self).environ(scope)
    }
    fn window_name_exists(&self, name: &str) -> Option<bool> {
        (**self).window_name_exists(name)
    }
    fn session_name_exists(&self, name: &str) -> bool {
        (**self).session_name_exists(name)
    }
    fn search_pane(&self, pattern: &str, flags: SearchFlags) -> Option<u32> {
        (**self).search_pane(pattern, flags)
    }
    fn sessions(&self) -> Vec<LoopItem<'_>> {
        (**self).sessions()
    }
    fn windows(&self) -> Option<Vec<LoopItem<'_>>> {
        (**self).windows()
    }
    fn panes(&self) -> Option<Vec<LoopItem<'_>>> {
        (**self).panes()
    }
    fn clients(&self) -> Vec<LoopItem<'_>> {
        (**self).clients()
    }
    fn options(&self, scope: OptionScope) -> Option<OptionsLoop<'_>> {
        (**self).options(scope)
    }
    fn job(&self, raw: &str, expanded: &str) -> String {
        (**self).job(raw, expanded)
    }
}

/// Which environment `#{V/x:...}` loops over (format.c:5692).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvScope {
    /// The target session's (no flag, or `s`).
    Session,
    /// The global environment (`g`).
    Global,
    /// The client's (`c`).
    Client,
}

/// One environment entry as the `#{V:...}` loop sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvVar {
    pub name: String,
    /// `None` for an entry marked removed (`set-environment -r`).
    pub value: Option<String>,
    pub hidden: bool,
}

/// The flags of `#{C/ir:...}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchFlags {
    /// `i`: ignore case.
    pub ignore_case: bool,
    /// `r`: the pattern is a regular expression, not a glob.
    pub regex: bool,
}

/// A parsed template.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Format {
    nodes: Vec<Node>,
    /// The text this was parsed from, kept when it holds a `%`: a time-aware
    /// expansion runs strftime over every nested template before parsing it
    /// (format.c:6728), so the text is needed again at expansion time.
    src: Option<Box<str>>,
}

/// The result of [`parse`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub ast: Format,
    /// Constructs that parsed but are not evaluated yet: each modifier name of
    /// every unsupported modifier list.
    pub deferred: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Text(String),
    Replace {
        value: Value,
        post: Box<Post>,
    },
    /// An unsupported modifier list (first modifier named); expands to nothing.
    Deferred(String),
    /// `#(cmd)`: the command as written, and as a template.
    Job(String, Format),
    /// tmux stops expanding here and returns what it has so far.
    Abort,
}

/// How a value is expanded again (format.c:6602-6611).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Reexpand {
    #[default]
    No,
    /// `E`: like any nested expansion; strftime only inside a time-aware expansion.
    E,
    /// `T`: strftime first, whatever the expansion. `E` wins when both are given.
    T,
}

/// What the modifiers do to a value once it is found (format.c:6602-6682).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Post {
    substitutions: Vec<matching::Substitute>,
    /// `E` or `T`: expand the value again.
    reexpand: Reexpand,
    /// `=N`: trim to this many columns; negative trims from the left.
    limit: Option<Format>,
    /// `=/N/marker`: appended (or prepended when `limit` is negative) if trimmed.
    marker: Option<Format>,
    /// `p`: pad to this many columns; negative pads on the left.
    pad: Option<Format>,
    /// `n`: replace with the length in bytes.
    length: bool,
    /// `w`: replace with the display width.
    width: bool,
}

/// The modifiers `format_find` applies to a variable it found (format.c:4691-4740).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct VarMods {
    /// One entry per `t` modifier (format.c:4681-4709); empty when there is none. A
    /// time variable renders as ctime text, `t/p`, `t/r`, `t/d` or `t/f/<fmt>`, and
    /// returns before the other modifiers apply.
    time: Vec<time::TimeMod>,
    basename: bool,
    dirname: bool,
    /// One entry per `q` modifier: its argument (`s`, `e`, `h`, `a`), if any.
    quote: Vec<Option<Format>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Value {
    Match(matching::PatternMatch),
    /// Look the (raw) name up.
    Var {
        name: String,
        vm: VarMods,
    },
    /// The body itself contains `#{`: expand it as a template (format.c:6575).
    Inner(Format),
    /// `l`: the body, unescaped but not expanded.
    Literal(String),
    /// `a`: the character with this code.
    Char(Format),
    /// `c`: a colour as hex or, with `f`/`b` flags, as an SGR sequence.
    Colour {
        flags: Vec<Format>,
        body: Format,
    },
    /// `S`, `W`, `P`, `L`: the body once per session, window, pane or client;
    /// `active` (the second of `all,active`) for the current one.
    Loop {
        kind: loops::LoopKind,
        sort: loops::SortMod,
        all: Format,
        active: Option<Format>,
    },
    /// `O`: the body once per option (or array item) of one tree.
    Options {
        flags: Option<Format>,
        body: Format,
    },
    /// `V`: the body once per environment entry.
    Environ {
        flags: Option<Format>,
        body: Format,
    },
    /// `N`: does a window (or, with `s`, a session) of this name exist? Without a
    /// recognised flag the value is that of `fallback` (`None`: it would have failed).
    Name {
        flags: Vec<Option<Format>>,
        body: Format,
        fallback: Option<Box<Value>>,
    },
    /// `C`: search the pane.
    Search {
        flags: Option<Format>,
        body: Format,
    },
    /// `R`: repeat `left` `right` times.
    Repeat {
        left: Format,
        right: Format,
    },
    /// `!` (negate) and `!!`: truthiness of the body.
    Not {
        negate: bool,
        body: Format,
    },
    /// `||` (any) and `&&` (all), n-ary and short-circuiting.
    Or(Vec<Format>),
    And(Vec<Format>),
    Cmp {
        op: CmpOp,
        left: Format,
        right: Format,
    },
    Cond {
        arms: Vec<CondArm>,
        default: Option<Format>,
        vm: VarMods,
    },
    /// `e|op|flags|precision`: arithmetic. `operands` is `None` without a comma.
    Expr {
        args: Vec<Format>,
        operands: Option<(Format, Format)>,
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
    let template = crate::rawstr::c_str(template);
    let ast = parse::template(template, 0, &mut deferred);
    Parsed { ast, deferred }
}

/// Expand a parsed template against the real clocks.
#[must_use]
pub fn expand(ast: &Format, ctx: &dyn FormatContext) -> String {
    expand_with(ast, ctx, &SystemClock::new(), &SystemWallClock)
}

/// Expand a parsed template with `FORMAT_EXPAND_NOJOBS` (format.c:150): `#()` expands
/// to nothing and its hook is never called. For `%if`, hooks, `wait-for`, styles and
/// other places where tmux refuses to start jobs.
#[must_use]
pub fn expand_nojobs(ast: &Format, ctx: &dyn FormatContext) -> String {
    let clock = SystemClock::new();
    let mut eval = Eval::new(ctx, &clock, &SystemWallClock, false);
    eval.nojobs = true;
    eval.expand(ast, 0)
}

/// [`expand_nojobs`] on template text.
#[must_use]
pub fn expand_str_nojobs(template: &str, ctx: &dyn FormatContext) -> String {
    expand_nojobs(&parse(template).ast, ctx)
}

/// Expand a parsed template; `clock` drives the [`TIME_LIMIT_MS`] cut-off.
#[must_use]
pub fn expand_with_clock(ast: &Format, ctx: &dyn FormatContext, clock: &dyn Clock) -> String {
    expand_with(ast, ctx, clock, &SystemWallClock)
}

/// Expand a parsed template; `wall` says what time it is and which zone is local
/// (`#{t:...}`, and `T` and `t/f` when they run strftime).
#[must_use]
pub fn expand_at(ast: &Format, ctx: &dyn FormatContext, wall: &dyn WallClock) -> String {
    expand_with(ast, ctx, &SystemClock::new(), wall)
}

/// [`expand`] with both clocks injected.
#[must_use]
pub fn expand_with(
    ast: &Format,
    ctx: &dyn FormatContext,
    clock: &dyn Clock,
    wall: &dyn WallClock,
) -> String {
    Eval::new(ctx, clock, wall, false).expand(ast, 0)
}

/// `format_expand_time` (format.c:6892, 6728-6741): every template in the expansion,
/// nested ones included, is run through `strftime` with the current local time
/// before it is parsed. `display-message` uses this; `list-*` formats do not.
#[must_use]
pub fn expand_time(template: &str, ctx: &dyn FormatContext) -> String {
    expand_time_with(template, ctx, &SystemClock::new(), &SystemWallClock)
}

/// [`expand_time`] with `wall` as the clock and zone.
#[must_use]
pub fn expand_time_at(template: &str, ctx: &dyn FormatContext, wall: &dyn WallClock) -> String {
    expand_time_with(template, ctx, &SystemClock::new(), wall)
}

/// [`expand_time`] with both clocks injected.
#[must_use]
pub fn expand_time_with(
    template: &str,
    ctx: &dyn FormatContext,
    clock: &dyn Clock,
    wall: &dyn WallClock,
) -> String {
    Eval::new(ctx, clock, wall, true).expand(&parse(template).ast, 0)
}

/// Parse and expand in one go.
#[must_use]
pub fn expand_str(template: &str, ctx: &dyn FormatContext) -> String {
    expand(&parse(template).ast, ctx)
}

/// tmux's `format_true` (format.c:4893): neither empty nor `0`.
#[must_use]
pub fn is_true(s: &str) -> bool {
    !s.is_empty() && s != "0"
}

struct Eval<'a> {
    ctx: &'a dyn FormatContext,
    clock: &'a dyn Clock,
    /// When this expansion started (`es->start_time`, format.c:6899).
    start: u64,
    wall: &'a dyn WallClock,
    /// Seconds since the epoch when the expansion started (`es->time`).
    now: i64,
    /// `FORMAT_EXPAND_TIME` (format.c:149): templates pass through strftime first.
    time_mode: bool,
    /// `FORMAT_EXPAND_NOJOBS` (format.c:150): `#()` is empty.
    nojobs: bool,
    /// `FORMAT_EXPAND_NOCYCLE` (format.c:151): no `A:` animation frames inside a
    /// job's command or output.
    #[allow(dead_code)] // read by the `A:` modifier, which is not implemented yet
    nocycle: bool,
}

impl<'a> Eval<'a> {
    fn new(
        ctx: &'a dyn FormatContext,
        clock: &'a dyn Clock,
        wall: &'a dyn WallClock,
        time_mode: bool,
    ) -> Self {
        Self {
            ctx,
            clock,
            start: clock.now_ms(),
            wall,
            now: wall.now(),
            time_mode,
            nojobs: false,
            nocycle: false,
        }
    }
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
        let p = parse("a#{l:x}b#{=3:@n}#{s/a/b/;E:@n}#(true)#{@v}#{m:a,b}#{t/p:@n}");
        assert_eq!(p.deferred, Vec::<String>::new());
        assert_eq!(expand(&p.ast, &map(&[("@v", "v")])), "axbv0");
        assert_eq!(
            parse("#{?@x,a,b}#{E:@y}#{||:1,0}#S").deferred,
            Vec::<String>::new()
        );
    }

    /// Reference (TZ=UTC, `set -gF @out FMT; show -gv @out`): `#{t:@t}` with
    /// @t=86400 is `Fri Jan  2 00:00:00 1970`; @t of -5, 0, `abc` or unset,
    /// and a non-time variable like `session_name`, give nothing; `#{t:#{@t}}`
    /// is an inner format, so plain `86400`.
    #[test]
    fn time_modifier() {
        struct Times(Map);
        impl FormatContext for Times {
            fn get(&self, name: &str) -> Option<String> {
                self.0.get(name)
            }
            fn time(&self, name: &str) -> Option<i64> {
                (name == "session_created").then_some(1_000_000_000)
            }
        }
        let ctx = Times(map(&[
            ("@t", "86400"),
            ("@neg", "-5"),
            ("@zero", "0"),
            ("@bad", "abc"),
            ("session_name", "s"),
        ]));
        let wall = FixedWallClock::new(1_793_369_398, "UTC").expect("zoneinfo");
        let e = |f: &str| expand_at(&parse(f).ast, &ctx, &wall);
        assert_eq!(e("#{t:@t}"), "Fri Jan  2 00:00:00 1970");
        assert_eq!(e("a#{t:@t}b"), "aFri Jan  2 00:00:00 1970b");
        assert_eq!(e("#{t:session_created}"), "Sun Sep  9 01:46:40 2001");
        for empty in [
            "#{t:@neg}",
            "#{t:@zero}",
            "#{t:@bad}",
            "#{t:@nosuch}",
            "#{t:session_name}",
        ] {
            assert_eq!(e(empty), "", "{empty}");
        }
        assert_eq!(e("#{t:#{@t}}"), "86400");
        assert_eq!(parse("#{t:@t}").deferred, Vec::<String>::new());
    }

    /// Reference: `display -p '%H %%'` at 20:xx prints `20 %`; a template
    /// without `%` is not touched by strftime.
    #[test]
    fn expand_time_runs_strftime_first() {
        struct At(Map);
        impl FormatContext for At {
            fn get(&self, name: &str) -> Option<String> {
                self.0.get(name)
            }
        }
        let ctx = At(map(&[("session_name", "a")]));
        let wall = FixedWallClock::new(1_793_305_246, "UTC").expect("zoneinfo");
        assert_eq!(expand_time_at("%H %% #S", &ctx, &wall), "20 % a");
        assert_eq!(expand_time_at("#S", &ctx, &wall), "a");
    }

    /// A fake job runner: `echo X` returns X (after the shell's quote removal, which
    /// is all these probes need), `printf` returns its argument with `\n` unescaped,
    /// anything else nothing. Records `(raw, expanded)` of every call.
    struct Jobs {
        vars: Map,
        calls: std::cell::RefCell<Vec<(String, String)>>,
    }

    impl Jobs {
        fn new(pairs: &[(&str, &str)]) -> Self {
            Self {
                vars: map(pairs),
                calls: std::cell::RefCell::default(),
            }
        }
        fn calls(&self) -> Vec<(String, String)> {
            self.calls.borrow().clone()
        }
    }

    impl FormatContext for Jobs {
        fn get(&self, name: &str) -> Option<String> {
            self.vars.get(name)
        }
        fn job(&self, raw: &str, expanded: &str) -> String {
            self.calls
                .borrow_mut()
                .push((raw.to_owned(), expanded.to_owned()));
            if let Some(rest) = expanded.strip_prefix("echo ") {
                rest.trim_matches('"').to_owned()
            } else if let Some(rest) = expanded.strip_prefix("printf ") {
                rest.trim_matches('"').replace("\\n", "\n")
            } else {
                String::new()
            }
        }
    }

    /// Reference (`-L`, isolated; each `#()` run chained on one command line with a
    /// `run -d1` between, as a fresh client never sees job output):
    /// `display -p 'a[#(echo hi)]'` then, a second later, `b[hi]`.
    #[test]
    fn job_runs_through_the_hook() {
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_str("a[#(echo hi)]b", &ctx), "a[hi]b");
        assert_eq!(ctx.calls(), [("echo hi".to_owned(), "echo hi".to_owned())]);
        // The default hook is empty; the template parses without deferrals.
        assert_eq!(expand_str("a[#(echo hi)]b", &map(&[])), "a[]b");
        assert_eq!(parse("#(echo hi)").deferred, Vec::<String>::new());
    }

    /// Reference: `#(echo "##{session_name}")` gives `s`: the command expands to
    /// `echo "#{session_name}"`, and the output is expanded once more.
    #[test]
    fn job_command_and_output_are_both_expanded() {
        let ctx = Jobs::new(&[("session_name", "s"), ("@x", "X")]);
        assert_eq!(expand_str("#(echo \"##{session_name}\")", &ctx), "s");
        assert_eq!(
            ctx.calls(),
            [(
                "echo \"##{session_name}\"".to_owned(),
                "echo \"#{session_name}\"".to_owned()
            )]
        );
        // A variable in the command is substituted before the job runs.
        let ctx = Jobs::new(&[("@x", "X")]);
        assert_eq!(expand_str("#(echo #{@x})", &ctx), "X");
        assert_eq!(
            ctx.calls()[0],
            ("echo #{@x}".to_owned(), "echo X".to_owned())
        );
        // A nested `#()` is empty in the command (NOJOBS) and in the output.
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_str("#(echo a#(echo b)c)", &ctx), "ac");
        // Reference: `#(echo "x#(echo y)")` gives `x`.
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_str("#(echo \"x#(echo y)\")", &ctx), "x");
        assert_eq!(ctx.calls().len(), 1);
        // ... and so does a job whose output holds one.
        let ctx = Jobs::new(&[("@o", "z#(echo y)")]);
        assert_eq!(expand_str("#(echo #{@o})", &ctx), "z");
    }

    /// Reference: `printf "a\nb\nc"` gives `c`; a trailing newline does not make
    /// the last line empty (`evbuffer_readline`).
    #[test]
    fn job_keeps_only_the_last_line() {
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_str("[#(printf \"a\\nb\\nc\")]", &ctx), "[c]");
        assert_eq!(expand_str("[#(printf \"a\\nb\\n\")]", &ctx), "[b]");
        assert_eq!(expand_str("[#(printf \"a\\n\\n\")]", &ctx), "[a]");
    }

    /// NOJOBS (format.c:6776): `#()` is empty and the hook is never called, also
    /// inside conditionals and `E`.
    #[test]
    fn nojobs_disables_jobs() {
        let ctx = Jobs::new(&[("@j", "#(echo hi)")]);
        assert_eq!(expand_str_nojobs("[#(echo hi)]", &ctx), "[]");
        assert_eq!(expand_str_nojobs("#{?#(echo 1),a,b}", &ctx), "b");
        assert_eq!(expand_str_nojobs("[#{E:@j}]", &ctx), "[]");
        assert_eq!(ctx.calls(), Vec::<(String, String)>::new());
        // Jobs run through `E` and conditionals otherwise.
        assert_eq!(expand_str("[#{E:@j}]", &ctx), "[hi]");
        assert_eq!(expand_str("#{?#(echo 1),a,b}", &ctx), "a");
    }

    /// Reference (17:xx local): `@k='#(echo %H)'` with `#{T:@k}` gives `17` (`T` runs
    /// strftime on the value before the job starts). `display -p '#(echo %%H)'` gives
    /// `%H`: `display`'s own pass turns `%%` into `%`, and neither the command nor the
    /// output gets another. `set -gF @o '#(echo %%H)'` (no pass) gives `%%H`.
    #[test]
    fn job_command_gets_strftime_only_from_t_or_the_caller() {
        let wall = FixedWallClock::new(1_793_305_246, "UTC").expect("zoneinfo");
        let ctx = Jobs::new(&[("@k", "#(echo %H)")]);
        // Plain expansion never runs strftime.
        assert_eq!(expand_at(&parse("#{@k}").ast, &ctx, &wall), "#(echo %H)");
        assert_eq!(expand_at(&parse("#{T:@k}").ast, &ctx, &wall), "20");
        assert_eq!(ctx.calls(), [("echo 20".to_owned(), "echo 20".to_owned())]);
        // A time-aware expansion: the caller's pass runs on the whole template, and
        // the job's command and output get none more.
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_time_at("#(echo %%H)", &ctx, &wall), "%H");
        assert_eq!(ctx.calls(), [("echo %H".to_owned(), "echo %H".to_owned())]);
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_time_at("#(echo %H)", &ctx, &wall), "20");
        // Not time-aware: `%H` stays literal in the command.
        let ctx = Jobs::new(&[]);
        assert_eq!(expand_at(&parse("#(echo %H)").ast, &ctx, &wall), "%H");
    }

    /// Jobs seen through a loop child reach the parent's hook.
    #[test]
    fn job_hook_survives_loops() {
        struct Loop(Jobs);
        impl FormatContext for Loop {
            fn get(&self, name: &str) -> Option<String> {
                self.0.get(name)
            }
            fn job(&self, raw: &str, expanded: &str) -> String {
                self.0.job(raw, expanded)
            }
            fn sessions(&self) -> Vec<LoopItem<'_>> {
                vec![LoopItem {
                    ctx: Box::new(&self.0),
                    keys: LoopKeys::default(),
                }]
            }
        }
        let ctx = Loop(Jobs::new(&[("session_name", "s")]));
        assert_eq!(expand_str("#{S:#(echo #S)}", &ctx), "s");
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
        let alphabet = b"#{}[](),:;?=!|&<>/@ESTlsa-0123xyepcqRVNCbdnwf+*%.\0\xff\xc3";
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
