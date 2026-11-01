//! The loop modifiers: `S` (sessions), `W` (windows), `P` (panes), `L` (clients) and
//! `O` (options), format.c:5230-5800.
//!
//! The context hands back one child context per item ([`LoopItem`]) in the server's
//! own order, with the keys tmux sorts by; this module sorts them (sort.c), picks the
//! template (`all,active` for `S`, `W` and `P`) and adds the per-item variables:
//! `loop_index`, `loop_last_flag`, and for windows `window_before_active`,
//! `window_after_active` and the `next_*`/`prev_*` neighbour variables.
//!
//! Every child is a fresh context: like tmux's `format_create` plus `format_defaults`
//! for the item, it does not see the parent's extra variables (an outer
//! `loop_index`, `line`).

use std::cmp::Ordering;

use super::{EnvScope, EnvVar, Eval, Format, FormatContext, SearchFlags};

/// One session, window, pane or client a loop visits.
pub struct LoopItem<'a> {
    /// What `format_defaults` would build for the item.
    pub ctx: Box<dyn FormatContext + 'a>,
    pub keys: LoopKeys,
}

/// What a loop sorts and selects by. Which fields matter depends on the loop; unused
/// ones stay at their defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoopKeys {
    /// Expand the second template of `all,active`: the session of the context's
    /// client, the session's current window, the window's active pane.
    pub active: bool,
    /// The ID (`$N`, `@N`, `%N`, or a client's connection number): creation order,
    /// and `#{S/i:...}` order.
    pub id: u32,
    /// A window's index in the session; a pane's `pane_index`.
    pub index: u32,
    /// Session name, window name, pane title or client name: `/n` order and the tie
    /// breaker of every sort.
    pub name: String,
    /// Higher is more recent: session, window and client activity; a pane's
    /// `active_point`.
    pub activity: u64,
    /// A pane's z index (`window_pane_zindex`, window.c:1293).
    pub z: u32,
    /// A window's own `@options` (not inherited ones), name and value, for the
    /// neighbours' `next_@x` and `prev_@x` (format.c:5323-5347).
    pub user_options: Vec<(String, String)>,
}

/// Which options tree `#{O/flags:...}` walks (format.c:5631-5656).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptionScope {
    /// `v`: the server options.
    Server,
    /// `g`, `gs`: the global session options.
    GlobalSession,
    /// `gw`: the global window options.
    GlobalWindow,
    /// No flags, or `s`: the target session's own tree.
    Session,
    /// `w`: the target window's own tree.
    Window,
    /// `p`: the target pane's own tree.
    Pane,
}

/// One entry of an options tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoopOption {
    pub name: String,
    /// `options_to_string(o, NULL, 0)`: flags as `on`/`off`, arrays joined by spaces.
    pub value: String,
    /// An array option's items, key and value, in key order; `None` when not an array.
    pub array: Option<Vec<(String, String)>>,
    pub hook: bool,
    /// A user `@option`.
    pub user: bool,
}

/// What `#{O:...}` walks: the tree's entries in name order and the context every
/// child starts from (`format_defaults(c, s, wl, wp)` of the parent's target).
pub struct OptionsLoop<'a> {
    pub ctx: Box<dyn FormatContext + 'a>,
    pub entries: Vec<LoopOption>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LoopKind {
    Sessions,
    Windows,
    Panes,
    Clients,
}

impl LoopKind {
    pub(super) fn from_modifier(name: &str) -> Option<Self> {
        Some(match name {
            "S" => Self::Sessions,
            "W" => Self::Windows,
            "P" => Self::Panes,
            "L" => Self::Clients,
            _ => return None,
        })
    }
}

/// The sort a loop uses: set by the last `S`, `W`, `P` or `L` modifier, whichever
/// loop runs (format.c:6191-6286; `#{S;W:...}` loops sessions in `W`'s order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SortMod {
    pub kind: LoopKind,
    /// The modifier's first argument: `i`, `n`, `t`, `z` and `r` letters.
    pub arg: Option<Format>,
}

/// `enum sort_order`, as far as the loops reach it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Order {
    /// The list as the server keeps it; `r` reverses it.
    Listed,
    Index,
    Name,
    Activity,
    Creation,
    Z,
}

/// format.c:6191-6286: what the modifier and its letters select.
fn order_of(kind: LoopKind, arg: Option<&str>) -> (Order, bool) {
    let Some(a) = arg else {
        return (order_of(kind, Some("")).0, false);
    };
    let default = match kind {
        LoopKind::Sessions => Order::Index,
        LoopKind::Panes => Order::Creation,
        LoopKind::Windows | LoopKind::Clients => Order::Listed,
    };
    let order = if kind == LoopKind::Panes {
        // Panes know `i` and `z` only.
        if a.contains('i') {
            Order::Index
        } else if a.contains('z') {
            Order::Z
        } else {
            default
        }
    } else if a.contains('i') {
        default
    } else if a.contains('n') {
        Order::Name
    } else if a.contains('t') {
        Order::Activity
    } else {
        default
    };
    (order, a.contains('r'))
}

/// `sort_session_cmp`, `sort_winlink_cmp`, `sort_pane_cmp` and `sort_client_cmp`
/// (sort.c): the primary key, then the name; `reversed` negates the whole result.
fn compare(kind: LoopKind, order: Order, a: &LoopKeys, b: &LoopKeys) -> Ordering {
    let primary = match (order, kind) {
        (Order::Index, LoopKind::Sessions) | (Order::Creation, _) => a.id.cmp(&b.id),
        (Order::Index, LoopKind::Windows | LoopKind::Panes) => a.index.cmp(&b.index),
        // A pane's active point sorts ascending; activity times newest first.
        (Order::Activity, LoopKind::Panes) => a.activity.cmp(&b.activity),
        (Order::Activity, _) => b.activity.cmp(&a.activity),
        (Order::Name, _) => a.name.cmp(&b.name),
        (Order::Z, LoopKind::Panes) => a.z.cmp(&b.z),
        _ => Ordering::Equal,
    };
    primary.then_with(|| a.name.cmp(&b.name))
}

/// `sort_qsort` (sort.c:39-61).
fn sort(kind: LoopKind, order: Order, reversed: bool, items: &mut [LoopItem<'_>]) {
    if order == Order::Listed {
        if reversed {
            items.reverse();
        }
        return;
    }
    items.sort_by(|a, b| {
        let o = compare(kind, order, &a.keys, &b.keys);
        if reversed { o.reverse() } else { o }
    });
}

/// `format_loop_options` (format.c:5624-5656): `None` is a scope with no tree.
fn option_scope(flags: &str) -> Option<OptionScope> {
    let flags = if flags.is_empty() { "s" } else { flags };
    if flags.contains('v') {
        return Some(OptionScope::Server);
    }
    let global = flags.contains('g');
    if flags.contains('w') {
        Some(if global {
            OptionScope::GlobalWindow
        } else {
            OptionScope::Window
        })
    } else if flags.contains('s') {
        Some(if global {
            OptionScope::GlobalSession
        } else {
            OptionScope::Session
        })
    } else if flags.contains('p') {
        (!global).then_some(OptionScope::Pane)
    } else {
        global.then_some(OptionScope::GlobalSession)
    }
}

fn flag(b: bool) -> String {
    String::from(if b { "1" } else { "0" })
}

/// A loop child: the item's context with the loop's variables in front.
pub(super) struct Overlay<'a> {
    pub parent: &'a dyn FormatContext,
    pub vars: Vec<(String, String)>,
}

impl Overlay<'_> {
    fn own(&self, name: &str) -> Option<String> {
        self.vars
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    }
}

impl FormatContext for Overlay<'_> {
    fn get(&self, name: &str) -> Option<String> {
        self.own(name).or_else(|| self.parent.get(name))
    }

    fn get_without_environ(&self, name: &str) -> Option<String> {
        self.own(name)
            .or_else(|| self.parent.get_without_environ(name))
    }

    fn time(&self, name: &str) -> Option<i64> {
        self.parent.time(name)
    }

    fn environ(&self, scope: EnvScope) -> Option<Vec<EnvVar>> {
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

    fn sessions(&self) -> Vec<LoopItem<'_>> {
        self.parent.sessions()
    }

    fn windows(&self) -> Option<Vec<LoopItem<'_>>> {
        self.parent.windows()
    }

    fn panes(&self) -> Option<Vec<LoopItem<'_>>> {
        self.parent.panes()
    }

    fn clients(&self) -> Vec<LoopItem<'_>> {
        self.parent.clients()
    }

    fn options(&self, scope: OptionScope) -> Option<OptionsLoop<'_>> {
        self.parent.options(scope)
    }

    fn job(&self, raw: &str, expanded: &str) -> String {
        self.parent.job(raw, expanded)
    }
}

/// `format_add_window_neighbour` (format.c:5323-5347).
fn neighbour(vars: &mut Vec<(String, String)>, keys: &LoopKeys, prefix: &str) {
    vars.push((format!("{prefix}_window_index"), keys.index.to_string()));
    vars.push((format!("{prefix}_window_active"), flag(keys.active)));
    for (name, value) in &keys.user_options {
        vars.push((format!("{prefix}_{name}"), value.clone()));
    }
}

impl Eval<'_> {
    /// Expand `body` against `ctx` with this expansion's clocks and limits
    /// (`format_copy_state`, format.c:315).
    pub(super) fn expand_in(&self, ctx: &dyn FormatContext, body: &Format, depth: usize) -> String {
        Eval {
            ctx,
            clock: self.clock,
            start: self.start,
            wall: self.wall,
            now: self.now,
            time_mode: self.time_mode,
            nojobs: self.nojobs,
            nocycle: self.nocycle,
        }
        .expand(body, depth)
    }

    /// `format_loop_sessions`, `_windows`, `_panes` and `_clients`. `None` is the
    /// "no session" / "no window" failure that ends the whole expansion.
    pub(super) fn run_loop(
        &mut self,
        kind: LoopKind,
        sort_mod: &SortMod,
        all: &Format,
        active: Option<&Format>,
        depth: usize,
    ) -> Option<String> {
        let arg = sort_mod.arg.as_ref().map(|a| self.expand(a, depth));
        let (order, reversed) = order_of(sort_mod.kind, arg.as_deref());
        let ctx = self.ctx;
        let mut items = match kind {
            LoopKind::Sessions => ctx.sessions(),
            LoopKind::Windows => ctx.windows()?,
            LoopKind::Panes => ctx.panes()?,
            LoopKind::Clients => ctx.clients(),
        };
        sort(kind, order, reversed, &mut items);
        let n = items.len();
        let mut out = String::new();
        for (i, item) in items.iter().enumerate() {
            let template = match active {
                Some(a) if item.keys.active => a,
                _ => all,
            };
            let mut vars = vec![
                ("loop_index".to_owned(), i.to_string()),
                ("loop_last_flag".to_owned(), flag(i + 1 == n)),
            ];
            if kind == LoopKind::Windows {
                let prev = i.checked_sub(1).map(|p| &items[p].keys);
                let next = items.get(i + 1).map(|x| &x.keys);
                vars.push((
                    "window_after_active".to_owned(),
                    flag(prev.is_some_and(|k| k.active)),
                ));
                vars.push((
                    "window_before_active".to_owned(),
                    flag(next.is_some_and(|k| k.active)),
                ));
                if let Some(k) = next {
                    neighbour(&mut vars, k, "next");
                }
                if let Some(k) = prev {
                    neighbour(&mut vars, k, "prev");
                }
            }
            let overlay = Overlay {
                parent: &*item.ctx,
                vars,
            };
            out.push_str(&self.expand_in(&overlay, template, depth));
        }
        Some(out)
    }

    /// `format_loop_options` (format.c:5603-5690): one child per entry, or per item
    /// of an array that has any.
    pub(super) fn options_loop(
        &mut self,
        flags: Option<&Format>,
        body: &Format,
        depth: usize,
    ) -> String {
        let flags = flags.map(|f| self.expand(f, depth)).unwrap_or_default();
        let ctx = self.ctx;
        let Some(tree) = option_scope(&flags).and_then(|s| ctx.options(s)) else {
            return String::new();
        };
        let mut out = String::new();
        let mut index = 0usize;
        let last_entry = tree.entries.len().saturating_sub(1);
        for (e, o) in tree.entries.iter().enumerate() {
            let common = |vars: &mut Vec<(String, String)>, count: usize| {
                vars.push(("option_name".to_owned(), o.name.clone()));
                vars.push(("option_array_count".to_owned(), count.to_string()));
                vars.push(("option_is_hook".to_owned(), flag(o.hook)));
                vars.push(("option_is_user".to_owned(), flag(o.user)));
            };
            let items = o.array.as_deref().unwrap_or_default();
            if items.is_empty() {
                // format_loop_add_option (format.c:5491-5537).
                let is_array = o.array.is_some();
                let mut vars = vec![
                    ("option_value".to_owned(), o.value.clone()),
                    ("option_is_array".to_owned(), flag(is_array)),
                    ("option_array_key".to_owned(), String::new()),
                    ("option_array_index".to_owned(), String::new()),
                    ("option_array_first".to_owned(), flag(is_array)),
                    ("option_array_last".to_owned(), flag(is_array)),
                    ("loop_last_flag".to_owned(), flag(e == last_entry)),
                    ("loop_index".to_owned(), index.to_string()),
                ];
                common(&mut vars, 0);
                let overlay = Overlay {
                    parent: &*tree.ctx,
                    vars,
                };
                out.push_str(&self.expand_in(&overlay, body, depth));
                index += 1;
                continue;
            }
            // format_loop_add_array_item (format.c:5540-5600).
            for (a, (key, value)) in items.iter().enumerate() {
                let last_item = a + 1 == items.len();
                let mut vars = vec![
                    ("option_value".to_owned(), value.clone()),
                    ("option_is_array".to_owned(), flag(true)),
                    ("option_array_key".to_owned(), key.clone()),
                    ("option_array_index".to_owned(), key.clone()),
                    ("option_array_first".to_owned(), flag(a == 0)),
                    ("option_array_last".to_owned(), flag(last_item)),
                    (
                        "loop_last_flag".to_owned(),
                        flag(last_item && e == last_entry),
                    ),
                    ("loop_index".to_owned(), index.to_string()),
                ];
                common(&mut vars, items.len());
                let overlay = Overlay {
                    parent: &*tree.ctx,
                    vars,
                };
                out.push_str(&self.expand_in(&overlay, body, depth));
                index += 1;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(id: u32, name: &str, activity: u64) -> LoopKeys {
        LoopKeys {
            id,
            name: name.to_owned(),
            activity,
            ..LoopKeys::default()
        }
    }

    #[test]
    fn sort_letters_follow_the_modifier() {
        use LoopKind::{Clients, Panes, Sessions, Windows};
        assert_eq!(order_of(Sessions, None), (Order::Index, false));
        assert_eq!(order_of(Windows, None), (Order::Listed, false));
        assert_eq!(order_of(Panes, None), (Order::Creation, false));
        assert_eq!(order_of(Clients, None), (Order::Listed, false));
        assert_eq!(order_of(Sessions, Some("x")), (Order::Index, false));
        assert_eq!(order_of(Windows, Some("i")), (Order::Listed, false));
        assert_eq!(order_of(Clients, Some("nr")), (Order::Name, true));
        // `i` beats `n` beats `t`; panes know only `i` and `z`.
        assert_eq!(order_of(Sessions, Some("tn")), (Order::Name, false));
        assert_eq!(order_of(Windows, Some("ti")), (Order::Listed, false));
        assert_eq!(order_of(Panes, Some("zi")), (Order::Index, false));
        assert_eq!(order_of(Panes, Some("t")), (Order::Creation, false));
    }

    #[test]
    fn activity_is_newest_first_and_ties_go_by_name() {
        let (a, b) = (keys(0, "a", 5), keys(1, "b", 5));
        let c = keys(2, "c", 9);
        let k = LoopKind::Sessions;
        assert_eq!(compare(k, Order::Activity, &c, &a), Ordering::Less);
        assert_eq!(compare(k, Order::Activity, &a, &b), Ordering::Less);
        // Pane active points sort the other way.
        assert_eq!(
            compare(LoopKind::Panes, Order::Activity, &c, &a),
            Ordering::Greater
        );
        // An order the loop has no key for falls through to the name.
        assert_eq!(
            compare(LoopKind::Windows, Order::Z, &c, &a),
            Ordering::Greater
        );
    }

    #[test]
    fn option_flags_pick_a_tree() {
        use OptionScope::{GlobalSession, GlobalWindow, Pane, Server, Session, Window};
        for (flags, scope) in [
            ("", Some(Session)),
            ("s", Some(Session)),
            ("w", Some(Window)),
            ("p", Some(Pane)),
            ("g", Some(GlobalSession)),
            ("gs", Some(GlobalSession)),
            ("gw", Some(GlobalWindow)),
            ("gp", None),
            ("x", None),
            ("v", Some(Server)),
            ("gwv", Some(Server)),
            ("ws", Some(Window)),
        ] {
            assert_eq!(option_scope(flags), scope, "{flags}");
        }
    }
}
