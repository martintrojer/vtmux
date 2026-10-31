//! Goldens from the reference tmux 3.8: `tests/layout_cases.sh` drives it
//! (`new -d -x W -y H`, a command sequence, then `display -p
//! '#{window_layout}'` for v2 and a `-C attach` client without new-layouts
//! for v1) and writes `golden_cases.in`. Each case is replayed here on the
//! model through a small window simulator that does what the tmux commands
//! do around the layout code (target lookup, pane order, active pane,
//! `lastlayout`), then every emitted string is parsed back.

use std::str::FromStr;

use super::{
    Dir, FloatArgs, Layout, NamedLayout, PaneStatus, ResizeRequest, SetOpts, SplitRequest,
    WindowPanes, percentage,
};
use crate::cmd::{Args, parse_argv, strtonum};
use crate::ids::{PaneId, WindowId};

struct Case {
    name: &'static str,
    sx: u32,
    sy: u32,
    ops: &'static [&'static str],
    errs: &'static str,
    v2: &'static str,
    v1: &'static str,
    /// What the reference makes of `v2` / `v1`: `display -p '#{window_layout}'`
    /// after `select-layout <string>` on the same final state, and the stderr
    /// of that select-layout. Parsing is not the identity in tmux.
    re2: &'static str,
    re2_err: &'static str,
    re1: &'static str,
    re1_err: &'static str,
}

include!("golden_cases.in");

/// A window as the commands see it.
struct Sim {
    layout: Layout,
    win: WindowPanes,
    next: u32,
    last_layout: Option<NamedLayout>,
    opts: SetOpts,
    errs: Vec<String>,
}

impl Sim {
    fn new(sx: u32, sy: u32) -> Self {
        Self {
            layout: Layout::new(sx, sy, PaneId(0)),
            win: WindowPanes::new(PaneId(0)),
            next: 1,
            last_layout: None,
            opts: SetOpts::default(),
            errs: Vec::new(),
        }
    }

    fn target(&self, args: &Args) -> PaneId {
        args.value('t')
            .map_or(self.win.active.unwrap(), |t| PaneId::from_str(t).unwrap())
    }

    fn run(&mut self, line: &str, saved: &mut (String, String)) {
        match line {
            "@save" => {
                saved.0 = self.layout.dump_v2(&self.win).unwrap();
                return;
            }
            "@save1" => {
                saved.1 = self.layout.dump_v1();
                return;
            }
            "@restore" => return self.select_layout_str(&saved.0.clone()),
            "@restore1" => return self.select_layout_str(&saved.1.clone()),
            _ => {}
        }
        let words: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
        let call = parse_argv(&words)
            .unwrap_or_else(|e| panic!("{line}: {e}"))
            .remove(0);
        let a = &call.args;
        match call.entry.name {
            "split-window" => self.split_window(a),
            "new-pane" => self.new_pane(a),
            "resize-pane" => self.resize_pane(a),
            "resize-window" => self.resize_window(a),
            "select-layout" => self.select_layout(a),
            "next-layout" => self.cycle(true),
            "previous-layout" => self.cycle(false),
            "select-pane" => {
                if a.flag('l') {
                    let last = self.win.last[0];
                    let floating = self.is_floating(last);
                    self.win.select_raising(last, floating);
                } else {
                    let t = self.target(a);
                    let floating = self.is_floating(t);
                    self.win.select_raising(t, floating);
                }
            }
            "kill-pane" => {
                let t = self.target(a);
                self.layout.close_pane(t);
                self.win.remove(t);
            }
            "set-option" => self.set_option(a),
            other => panic!("golden op {other} not simulated"),
        }
    }

    fn set_option(&mut self, a: &Args) {
        let (name, val) = (a.positional(0).unwrap(), a.positional(1).unwrap());
        match name {
            "pane-border-status" => {
                self.layout.opts.status = match val {
                    "top" => PaneStatus::Top,
                    "bottom" => PaneStatus::Bottom,
                    _ => PaneStatus::Off,
                };
            }
            "main-pane-width" => self.opts.main_pane_width = val.to_owned(),
            "main-pane-height" => self.opts.main_pane_height = val.to_owned(),
            "other-pane-width" => self.opts.other_pane_width = val.to_owned(),
            "other-pane-height" => self.opts.other_pane_height = val.to_owned(),
            "tiled-layout-max-columns" => self.opts.tiled_layout_max_columns = val.parse().unwrap(),
            other => panic!("golden option {other} not simulated"),
        }
    }

    fn is_floating(&self, p: PaneId) -> bool {
        let path = self.layout.root.find_pane(p).unwrap();
        self.layout.root.get(&path).is_floating()
    }

    /// cmd-split-window.c: a tiled split, or a split of a floating pane.
    fn split_window(&mut self, a: &Args) {
        let target = self.target(a);
        let (horizontal, before, full) = (a.flag('h'), a.flag('b'), a.flag('f'));
        let new = PaneId(self.next);
        if self.is_floating(target) {
            let g = match self.layout.split_floating(target, horizontal, before, full) {
                Ok(g) => g,
                Err(e) => return self.errs.push(e.to_string()),
            };
            self.layout.add_floating(Some(target), new, g);
            self.finish_add(new, target, a, true, before, full);
            return;
        }
        let mut size = None;
        let curval = {
            let r = if full {
                (self.layout.sx, self.layout.sy)
            } else {
                let g = self.layout.pane_rect(target).unwrap();
                (g.sx, g.sy)
            };
            i64::from(if horizontal { r.0 } else { r.1 })
        };
        if let Some(l) = a.value('l') {
            match percentage(l, 0, i64::from(i32::MAX), curval) {
                Ok(v) => size = Some(v),
                Err(e) => return self.errs.push(format!("invalid tiled geometry {e}")),
            }
        } else if let Some(p) = a.value('p') {
            match strtonum(p, 0, 100) {
                Ok(v) => size = Some(curval * v / 100),
                Err(e) => return self.errs.push(format!("invalid tiled geometry {e}")),
            }
        }
        let req = SplitRequest {
            horizontal,
            before,
            full,
            size,
        };
        match self.layout.split(target, new, &req) {
            Ok(()) => self.finish_add(new, target, a, false, before, full),
            Err(e) => self.errs.push(e.to_string()),
        }
    }

    fn finish_add(
        &mut self,
        new: PaneId,
        other: PaneId,
        a: &Args,
        floating: bool,
        before: bool,
        full: bool,
    ) {
        self.next += 1;
        self.win.add(
            new,
            Some(other),
            super::AddFlags {
                before,
                full,
                floating,
            },
        );
        if !a.flag('d') {
            self.win.select(new);
        }
    }

    /// cmd-split-window.c as `new-pane` (floating).
    fn new_pane(&mut self, a: &Args) {
        let target = self.target(a);
        let fa = FloatArgs {
            width: a.value('x').map(str::to_owned),
            height: a.value('y').map(str::to_owned),
            x: a.value('X').map(str::to_owned),
            y: a.value('Y').map(str::to_owned),
        };
        match self.layout.floating_geometry(&fa) {
            Ok(g) => {
                let new = PaneId(self.next);
                self.layout.add_floating(Some(target), new, g);
                self.finish_add(new, target, a, true, false, false);
            }
            Err(e) => self.errs.push(e),
        }
    }

    fn resize_pane(&mut self, a: &Args) {
        let target = self.target(a);
        let mut req = ResizeRequest {
            x: a.value('x').map(str::to_owned),
            y: a.value('y').map(str::to_owned),
            adjust: Vec::new(),
        };
        for flag in ['U', 'D', 'L', 'R'] {
            if !a.flag(flag) {
                continue;
            }
            let v = a.value(flag).or(a.positional(0)).unwrap_or("1");
            match strtonum(v, i64::from(i32::MIN), i64::from(i32::MAX)) {
                Ok(n) => req.adjust.push((flag, n as i32)),
                Err(e) => return self.errs.push(format!("adjustment {e}")),
            }
        }
        if let Err(e) = self.layout.resize_pane_cmd(target, &req, &mut 0) {
            self.errs.push(e);
        }
    }

    fn resize_window(&mut self, a: &Args) {
        let mut sx = i64::from(self.layout.sx);
        let mut sy = i64::from(self.layout.sy);
        if a.flag('x') {
            sx = a.number('x', 1, 10_000).unwrap();
        }
        if a.flag('y') {
            sy = a.number('y', 1, 10_000).unwrap();
        }
        self.layout.resize_window(sx as u32, sy as u32);
    }

    fn cycle(&mut self, next: bool) {
        let l = if next {
            NamedLayout::next(self.last_layout)
        } else {
            NamedLayout::previous(self.last_layout)
        };
        self.apply_named(l);
    }

    fn apply_named(&mut self, l: NamedLayout) {
        let panes = self.win.panes.clone();
        self.layout.select_named(l, &panes, &self.opts);
        self.last_layout = Some(l);
    }

    fn select_layout(&mut self, a: &Args) {
        if a.flag('n') {
            return self.cycle(true);
        }
        if a.flag('p') {
            return self.cycle(false);
        }
        if a.flag('E') {
            let t = self.target(a);
            self.layout.spread_out(t);
            return;
        }
        match a.positional(0) {
            None => {
                if let Some(l) = self.last_layout {
                    self.apply_named(l);
                }
            }
            Some(name) => {
                if let Some(l) = NamedLayout::lookup(name) {
                    self.apply_named(l);
                } else {
                    self.select_layout_str(name);
                }
            }
        }
    }

    fn select_layout_str(&mut self, s: &str) {
        if let Err(cause) = self.layout.select_custom(WindowId(0), &mut self.win, s) {
            self.errs.push(format!("{cause}: {s}"));
        }
    }
}

fn replay(case: &Case) -> Sim {
    let mut sim = Sim::new(case.sx, case.sy);
    let mut saved = (String::new(), String::new());
    for op in case.ops {
        sim.run(op, &mut saved);
    }
    sim
}

#[test]
fn layout_goldens_match_the_reference() {
    let mut failed = Vec::new();
    let cases = cases();
    assert!(cases.len() >= 30);
    for case in &cases {
        let sim = replay(case);
        let v2 = sim.layout.dump_v2(&sim.win).unwrap();
        let v1 = sim.layout.dump_v1();
        let errs = sim.errs.join("\n");
        if v2 != case.v2 || v1 != case.v1 || errs != case.errs {
            failed.push(format!(
                "{} ({}x{}) {:?}\n  v2 want {}\n  v2 got  {}\n  v1 want {}\n  v1 got  {}\n  errs want {:?}\n  errs got  {:?}",
                case.name, case.sx, case.sy, case.ops, case.v2, v2, case.v1, v1, case.errs, errs
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} of {} cases differ:\n{}",
        failed.len(),
        cases.len(),
        failed.join("\n")
    );
}

/// Re-select `string` on the final state of `case` the way
/// `select-layout <string>` does; the new v2 string and the error text.
fn reselect(case: &Case, string: &str) -> (String, String) {
    let mut sim = replay(case);
    sim.errs.clear();
    sim.select_layout_str(string);
    let err = sim.errs.join("\n");
    (sim.layout.dump_v2(&sim.win).unwrap(), err)
}

/// Parsing an emitted string is not the identity in tmux (`fix_offsets`
/// rebases a lone node, the last-used stack is rebuilt from the `"l"` values
/// present, collapsed nodes lose offsets), so the property is: the model
/// parsing the v2 string ends where the reference parsing it ends.
#[test]
fn v2_strings_parse_like_the_reference() {
    let mut failed = Vec::new();
    for case in &cases() {
        let (got, err) = reselect(case, case.v2);
        // select_layout_str appends `: <string>` to a cause, as the command does.
        let err = err.strip_suffix(&format!(": {}", case.v2)).unwrap_or(&err);
        let want_err = case
            .re2_err
            .strip_suffix(&format!(": {}", case.v2))
            .unwrap_or(case.re2_err);
        if got != case.re2 || err != want_err {
            failed.push(format!(
                "{} {:?}\n  want {}\n  got  {}\n  err want {want_err:?} got {err:?}",
                case.name, case.ops, case.re2, got
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

/// The same for the v1 string (what a control client without new-layouts
/// sees): the reference's result is a v2 string, with panes assigned in
/// window order and floating cells gone.
#[test]
fn v1_strings_parse_like_the_reference() {
    let mut failed = Vec::new();
    for case in &cases() {
        let (got, err) = reselect(case, case.v1);
        let err = err.strip_suffix(&format!(": {}", case.v1)).unwrap_or(&err);
        let want_err = case
            .re1_err
            .strip_suffix(&format!(": {}", case.v1))
            .unwrap_or(case.re1_err);
        if got != case.re1 || err != want_err {
            failed.push(format!(
                "{} {:?}\n  want {}\n  got  {}\n  err want {want_err:?} got {err:?}",
                case.name, case.ops, case.re1, got
            ));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n"));
}

#[test]
fn named_layout_names_and_cycling() {
    assert_eq!(NamedLayout::lookup("tiled"), Some(NamedLayout::Tiled));
    assert_eq!(
        NamedLayout::lookup("even-h"),
        Some(NamedLayout::EvenHorizontal)
    );
    assert_eq!(NamedLayout::lookup("main"), None);
    assert_eq!(
        NamedLayout::lookup("main-horizontal"),
        Some(NamedLayout::MainHorizontal)
    );
    assert_eq!(NamedLayout::lookup("nope"), None);
    assert_eq!(NamedLayout::next(None), NamedLayout::EvenHorizontal);
    assert_eq!(
        NamedLayout::next(Some(NamedLayout::Tiled)),
        NamedLayout::EvenHorizontal
    );
    assert_eq!(NamedLayout::previous(None), NamedLayout::Tiled);
    assert_eq!(
        NamedLayout::previous(Some(NamedLayout::EvenHorizontal)),
        NamedLayout::Tiled
    );
    assert_eq!(Dir::LeftRight, Dir::LeftRight);
}
