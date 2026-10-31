//! Linear-time matching for EREs without backreferences.
//!
//! The node graph reproduces glibc's compiled tree (regcomp.c: `parse_dup_op`,
//! `optimize_subexps`, `lower_subexp`, `calc_first`, `calc_next`,
//! `link_nfa_nodes`), because glibc's capture walk prefers the epsilon
//! destination with the lower node index. The whole match is one Thompson
//! simulation over the text that keeps the earliest start for each node: the
//! leftmost start, then the longest end. Captures replay glibc's regexec.c on
//! that span only: forward state log, backward sift (`prune_impossible_nodes`),
//! then the greedy `set_regs` / `proceed_next_node` / `update_regs` walk.
//! Each pass costs O(text × nodes), so these patterns need no work budget.
use super::{Anchor, Bracket, Class, Error, Expr, equal};
use std::collections::HashMap;
use std::ops::Range;

type Captures = Vec<Option<Range<usize>>>;

/// Compiled tree nodes; `(a{1000}){1000}` would need about two million.
const MAX_TREE_NODES: usize = 1 << 20;

#[derive(Debug, Clone, Copy)]
enum Kind {
    Literal(char),
    Any,
    Bracket(usize),
    Open(usize),
    Close(usize, bool),
    Assert(Assert),
    /// `OP_ALT` / `OP_DUP_ASTERISK`.
    Split,
    End,
}

#[derive(Debug, Clone, Copy)]
enum Assert {
    Start,
    End,
    WordFirst,
    WordLast,
    InsideWord,
    InsideNotWord,
}

#[derive(Debug, Clone)]
struct Node {
    kind: Kind,
    /// Consuming nodes: the next node. Epsilon nodes: unused.
    next: usize,
    /// Epsilon destinations in glibc's preference order (ascending node index).
    edests: Vec<usize>,
}

#[derive(Debug, Clone)]
pub(super) struct Nfa {
    nodes: Vec<Node>,
    rev: Vec<Vec<usize>>,
    brackets: Vec<Bracket>,
    init: usize,
    groups: usize,
    /// `subexp_map`: `((x))` reports the outer group for the inner one.
    group_map: Vec<usize>,
    ignore_case: bool,
}

// ---- compilation -------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum TKind {
    Leaf(Kind),
    Concat,
    Alt,
    Star,
}

struct TNode {
    kind: TKind,
    left: Option<usize>,
    right: Option<usize>,
    first: usize,
    next: Option<usize>,
    idx: usize,
}

struct Builder {
    tree: Vec<TNode>,
    brackets: Vec<Bracket>,
    /// Repetition copies share one bracket table entry, keyed by address.
    bracket_ids: HashMap<*const Bracket, usize>,
    group_map: Vec<usize>,
}

impl Builder {
    fn add(
        &mut self,
        kind: TKind,
        left: Option<usize>,
        right: Option<usize>,
    ) -> Result<usize, Error> {
        if self.tree.len() >= MAX_TREE_NODES {
            return Err(Error::Limit);
        }
        self.tree.push(TNode {
            kind,
            left,
            right,
            first: 0,
            next: None,
            idx: 0,
        });
        Ok(self.tree.len() - 1)
    }
    fn leaf(&mut self, kind: Kind) -> Result<Option<usize>, Error> {
        self.add(TKind::Leaf(kind), None, None).map(Some)
    }
    fn concat(&mut self, a: Option<usize>, b: Option<usize>) -> Result<Option<usize>, Error> {
        Ok(match (a, b) {
            (Some(a), Some(b)) => Some(self.add(TKind::Concat, Some(a), Some(b))?),
            (a, b) => a.or(b),
        })
    }
    fn bracket(&mut self, b: &Bracket) -> usize {
        let next = self.brackets.len();
        let id = *self
            .bracket_ids
            .entry(std::ptr::from_ref(b))
            .or_insert(next);
        if id == next {
            self.brackets.push(b.clone());
        }
        id
    }

    /// One `parse_expression` result. `opt` marks this group `opt_subexp`;
    /// `dup` is a `duplicate_tree` copy, whose tokens have every `opt_subexp`
    /// cleared by `create_token_tree` (regcomp.c).
    fn build(&mut self, expr: &Expr, opt: bool, dup: bool) -> Result<Option<usize>, Error> {
        match expr {
            Expr::Empty => Ok(None),
            Expr::Literal(c) => self.leaf(Kind::Literal(*c)),
            Expr::Any => self.leaf(Kind::Any),
            Expr::Bracket(b) => {
                let id = self.bracket(b);
                self.leaf(Kind::Bracket(id))
            }
            Expr::Anchor(a) => {
                let pair = |x, y| (Kind::Assert(x), Kind::Assert(y));
                // parse_expression splits \b and \B into an OP_ALT of two anchors.
                let (x, y) = match a {
                    Anchor::Start => return self.leaf(Kind::Assert(Assert::Start)),
                    Anchor::End => return self.leaf(Kind::Assert(Assert::End)),
                    Anchor::WordStart => return self.leaf(Kind::Assert(Assert::WordFirst)),
                    Anchor::WordEnd => return self.leaf(Kind::Assert(Assert::WordLast)),
                    Anchor::Boundary => pair(Assert::WordFirst, Assert::WordLast),
                    Anchor::NotBoundary => pair(Assert::InsideWord, Assert::InsideNotWord),
                };
                let x = self.leaf(x)?;
                let y = self.leaf(y)?;
                self.add(TKind::Alt, x, y).map(Some)
            }
            Expr::Backref(_) => unreachable!("backreference patterns use the backtracker"),
            Expr::Seq(parts) => {
                let mut tree = None;
                for part in parts {
                    let e = self.build(part, false, dup)?;
                    tree = self.concat(tree, e)?;
                }
                Ok(tree)
            }
            Expr::Alt(parts) => {
                let mut tree = self.build(&parts[0], false, dup)?;
                for part in &parts[1..] {
                    let e = self.build(part, false, dup)?;
                    tree = Some(self.add(TKind::Alt, tree, e)?);
                }
                Ok(tree)
            }
            Expr::Group(n, body) => {
                // optimize_subexps: a group whose whole body is a group merges with it.
                let mut body: &Expr = body;
                if let Expr::Group(m, inner) = single(body) {
                    self.group_map[*m] = self.group_map[*n];
                    body = inner;
                }
                let open = self.leaf(Kind::Open(*n))?;
                let body = self.build(body, false, dup)?;
                let close = self.leaf(Kind::Close(*n, opt))?;
                let tail = self.concat(body, close)?;
                self.concat(open, tail)
            }
            Expr::Repeat { expr, min, max } => self.repeat(expr, *min, *max, dup),
        }
    }

    /// regcomp.c:`parse_dup_op`: `e{n,m}` becomes `e…e` then `(e(e…)?)?` or `e*`.
    /// Only the copy marked after duplication keeps `opt_subexp`.
    fn repeat(
        &mut self,
        expr: &Expr,
        min: usize,
        max: Option<usize>,
        dup: bool,
    ) -> Result<Option<usize>, Error> {
        if max == Some(0) {
            return Ok(None);
        }
        let opt = matches!(expr, Expr::Group(..)) && !dup;
        let mut old = None;
        let mut star_dup = dup;
        if min > 0 {
            let Some(mut tree) = self.build(expr, false, dup)? else {
                return Ok(None);
            };
            for _ in 2..=min {
                let e = self.build(expr, false, true)?;
                tree = self.concat(Some(tree), e)?.unwrap();
            }
            if max == Some(min) {
                return Ok(Some(tree));
            }
            old = Some(tree);
            star_dup = true;
        }
        let Some(e) = self.build(expr, opt, star_dup)? else {
            return Ok(None);
        };
        let mut tree = if max.is_none() {
            self.add(TKind::Star, Some(e), None)?
        } else {
            self.add(TKind::Alt, Some(e), None)?
        };
        if let Some(max) = max {
            for _ in min + 2..=max {
                let e = self.build(expr, false, true)?;
                let c = self.concat(Some(tree), e)?;
                tree = self.add(TKind::Alt, c, None)?;
            }
        }
        self.concat(old, Some(tree))
    }
}

fn single(expr: &Expr) -> &Expr {
    match expr {
        Expr::Seq(parts) if parts.len() == 1 => &parts[0],
        e => e,
    }
}

impl Nfa {
    pub(super) fn compile(expr: &Expr, groups: usize, ignore_case: bool) -> Result<Self, Error> {
        let mut b = Builder {
            tree: Vec::new(),
            brackets: Vec::new(),
            bracket_ids: HashMap::new(),
            group_map: (0..=groups).collect(),
        };
        let body = b.build(expr, false, false)?;
        let end = b.add(TKind::Leaf(Kind::End), None, None)?;
        let root = b.concat(body, Some(end))?.unwrap();
        let mut tree = b.tree;

        // calc_first, postorder: non-CONCAT nodes get consecutive indices.
        let mut count = 0;
        let mut stack = vec![(root, false)];
        while let Some((t, done)) = stack.pop() {
            if done {
                if matches!(tree[t].kind, TKind::Concat) {
                    tree[t].first = tree[tree[t].left.unwrap()].first;
                } else {
                    tree[t].first = t;
                    tree[t].idx = count;
                    count += 1;
                }
                continue;
            }
            stack.push((t, true));
            if let Some(r) = tree[t].right {
                stack.push((r, false));
            }
            if let Some(l) = tree[t].left {
                stack.push((l, false));
            }
        }
        // calc_next, preorder.
        let mut stack = vec![root];
        while let Some(t) = stack.pop() {
            let (left, right, next) = (tree[t].left, tree[t].right, tree[t].next);
            match tree[t].kind {
                TKind::Star => tree[left.unwrap()].next = Some(t),
                TKind::Concat => {
                    tree[left.unwrap()].next = Some(tree[right.unwrap()].first);
                    tree[right.unwrap()].next = next;
                }
                _ => {
                    for c in [left, right].into_iter().flatten() {
                        tree[c].next = next;
                    }
                }
            }
            stack.extend([left, right].into_iter().flatten());
        }
        // link_nfa_nodes. A CONCAT's node_idx is its first child's.
        let idx = |t: usize| tree[tree[t].first].idx;
        let mut nodes = vec![
            Node {
                kind: Kind::End,
                next: 0,
                edests: Vec::new(),
            };
            count
        ];
        for n in &tree {
            let kind = match n.kind {
                TKind::Concat => continue,
                TKind::Leaf(kind) => kind,
                TKind::Alt | TKind::Star => Kind::Split,
            };
            let next = n.next.map(idx);
            let node = &mut nodes[n.idx];
            node.kind = kind;
            match kind {
                Kind::End => {}
                Kind::Literal(_) | Kind::Any | Kind::Bracket(_) => node.next = next.unwrap(),
                Kind::Open(..) | Kind::Close(..) | Kind::Assert(_) => {
                    node.edests = vec![next.unwrap()];
                }
                Kind::Split => {
                    let left = n.left.map_or_else(|| next.unwrap(), idx);
                    let right = n.right.map_or_else(|| next.unwrap(), idx);
                    // re_node_set_init_2 sorts and dedups.
                    node.edests = if left == right {
                        vec![left]
                    } else {
                        vec![left.min(right), left.max(right)]
                    };
                }
            }
        }
        let mut rev = vec![Vec::new(); count];
        for (i, n) in nodes.iter().enumerate() {
            for &d in &n.edests {
                rev[d].push(i);
            }
        }
        Ok(Self {
            init: idx(root),
            nodes,
            rev,
            brackets: b.brackets,
            groups,
            group_map: b.group_map,
            ignore_case,
        })
    }

    // ---- matching ------------------------------------------------------

    fn accepts(&self, node: usize, c: char) -> bool {
        match self.nodes[node].kind {
            Kind::Literal(l) => equal(l, c, self.ignore_case),
            Kind::Any => true,
            Kind::Bracket(b) => self.brackets[b].matches(c, self.ignore_case),
            _ => false,
        }
    }

    fn consuming(&self, node: usize) -> bool {
        matches!(
            self.nodes[node].kind,
            Kind::Literal(_) | Kind::Any | Kind::Bracket(_)
        )
    }

    /// Whether an epsilon node may be crossed at `pos` (anchors only).
    fn passable(&self, node: usize, text: &str, pos: usize, not_bol: bool) -> bool {
        let Kind::Assert(a) = self.nodes[node].kind else {
            return true;
        };
        let word = |c: char| c == '_' || Class::Alnum.contains(c);
        let before = text[..pos].chars().next_back().is_some_and(word);
        let after = text[pos..].chars().next().is_some_and(word);
        match a {
            Assert::Start => pos == 0 && !not_bol,
            Assert::End => pos == text.len(),
            Assert::WordFirst => !before && after,
            Assert::WordLast => before && !after,
            Assert::InsideWord => before && after,
            Assert::InsideNotWord => !before && !after,
        }
    }

    /// Leftmost start, then longest end: one pass, earliest start wins per node.
    pub(super) fn span(&self, text: &str, not_bol: bool) -> Option<(usize, usize)> {
        let mut mark = vec![usize::MAX; self.nodes.len()];
        let mut seeds: Vec<(usize, usize)> = Vec::new();
        let mut cur: Vec<(usize, usize)> = Vec::new();
        let mut stack = Vec::new();
        let mut best: Option<(usize, usize)> = None;
        let mut pos = 0;
        loop {
            cur.clear();
            let fresh = best.is_none().then_some((self.init, pos));
            for (node, start) in seeds.iter().copied().chain(fresh) {
                if best.is_some_and(|(s, _)| start > s) {
                    continue;
                }
                stack.push(node);
                while let Some(n) = stack.pop() {
                    if mark[n] == pos {
                        continue;
                    }
                    mark[n] = pos;
                    match self.nodes[n].kind {
                        Kind::End => {
                            if best.is_none_or(|(s, e)| start < s || (start == s && pos > e)) {
                                best = Some((start, pos));
                            }
                        }
                        Kind::Literal(_) | Kind::Any | Kind::Bracket(_) => cur.push((n, start)),
                        _ => {
                            if self.passable(n, text, pos, not_bol) {
                                stack.extend(self.nodes[n].edests.iter().rev());
                            }
                        }
                    }
                }
            }
            let Some(c) = text[pos..].chars().next() else {
                break;
            };
            seeds.clear();
            seeds.extend(
                cur.iter()
                    .filter(|&&(n, start)| {
                        best.is_none_or(|(s, _)| start <= s) && self.accepts(n, c)
                    })
                    .map(|&(n, start)| (self.nodes[n].next, start)),
            );
            if seeds.is_empty() && best.is_some() {
                break;
            }
            pos += c.len_utf8();
        }
        best
    }

    /// Captures for the match `start..end`, as glibc's `set_regs` reports them.
    /// `None` when glibc's walk dead-ends (it then returns `REG_NOMATCH`).
    pub(super) fn captures(
        &self,
        text: &str,
        start: usize,
        end: usize,
        not_bol: bool,
    ) -> Option<Captures> {
        if self.groups == 0 {
            return Some(vec![Some(start..end)]);
        }
        let positions: Vec<usize> = text[start..end]
            .char_indices()
            .map(|(i, _)| start + i)
            .chain(std::iter::once(end))
            .collect();
        let log = self.state_log(text, &positions, not_bol);
        let sifted = self.sift(text, &positions, &log);
        let mut captures = self.set_regs(text, &positions, &sifted)?;
        for g in 1..=self.groups {
            if self.group_map[g] != g {
                captures[g] = captures[self.group_map[g]].clone();
            }
        }
        Some(captures)
    }

    /// Forward state log anchored at `positions[0]`; failing anchors are left out.
    fn state_log(&self, text: &str, positions: &[usize], not_bol: bool) -> Vec<Vec<usize>> {
        let end = *positions.last().unwrap();
        let mut log = Vec::with_capacity(positions.len());
        let mut mark = vec![usize::MAX; self.nodes.len()];
        let mut seeds = vec![self.init];
        let mut stack = Vec::new();
        for (k, &pos) in positions.iter().enumerate() {
            let mut state = Vec::new();
            stack.append(&mut seeds);
            while let Some(x) = stack.pop() {
                if mark[x] == k || (!self.consuming(x) && !self.passable(x, text, pos, not_bol)) {
                    continue;
                }
                mark[x] = k;
                state.push(x);
                stack.extend(self.nodes[x].edests.iter().copied());
            }
            if let Some(c) = text[pos..end].chars().next() {
                seeds.extend(
                    state
                        .iter()
                        .filter(|&&x| self.consuming(x) && self.accepts(x, c))
                        .map(|&x| self.nodes[x].next),
                );
            }
            log.push(state);
        }
        log
    }

    /// regexec.c:`sift_states_backward`: keep nodes that still reach END at the end.
    fn sift(&self, text: &str, positions: &[usize], log: &[Vec<usize>]) -> Vec<Vec<usize>> {
        let n = self.nodes.len();
        let last = positions.len() - 1;
        let mut sifted: Vec<Vec<usize>> = vec![Vec::new(); positions.len()];
        let mut in_state = vec![usize::MAX; n];
        // Position at which each node was last added to a sifted state.
        let mut here = vec![usize::MAX; n];
        for k in (0..=last).rev() {
            for &x in &log[k] {
                in_state[x] = k;
            }
            let mut out: Vec<usize> = if k == last {
                log[k]
                    .iter()
                    .copied()
                    .filter(|&x| matches!(self.nodes[x].kind, Kind::End))
                    .collect()
            } else {
                let c = text[positions[k]..].chars().next().unwrap();
                log[k]
                    .iter()
                    .copied()
                    .filter(|&x| {
                        self.consuming(x) && self.accepts(x, c) && here[self.nodes[x].next] == k + 1
                    })
                    .collect()
            };
            for &x in &out {
                here[x] = k;
            }
            let mut i = 0;
            while i < out.len() {
                let y = out[i];
                i += 1;
                for &x in &self.rev[y] {
                    if in_state[x] == k && here[x] != k {
                        here[x] = k;
                        out.push(x);
                    }
                }
            }
            sifted[k] = out;
        }
        sifted
    }

    /// regexec.c:`set_regs` without a fail stack (glibc backtracks only with
    /// backreferences): greedy walk preferring the lower-index destination.
    fn set_regs(&self, text: &str, positions: &[usize], sifted: &[Vec<usize>]) -> Option<Captures> {
        let n = self.nodes.len();
        let last = positions.len() - 1;
        let to_i = |p: usize| isize::try_from(p).unwrap();
        let mut regs: Vec<(isize, isize)> = vec![(-1, -1); self.groups + 1];
        regs[0] = (to_i(positions[0]), to_i(positions[last]));
        let mut prev = regs.clone();
        let mut member = vec![usize::MAX; n];
        let mut eps_via = vec![usize::MAX; n];
        let mut via_epoch = 0usize;
        let mut k = 0;
        for &x in &sifted[0] {
            member[x] = 0;
        }
        let mut cur = self.init;
        // An epsilon walk visits each node at most twice per position.
        let mut steps = 0usize;
        let limit = (n + 1) * (positions.len() + 1) * 2;
        loop {
            steps += 1;
            if steps > limit {
                return None;
            }
            let pos = positions[k];
            Self::update_regs(self.nodes[cur].kind, &mut regs, &mut prev, to_i(pos));
            if matches!(self.nodes[cur].kind, Kind::End) {
                if k == last {
                    break;
                }
                return None;
            }
            if self.consuming(cur) {
                // Without a fail stack glibc checks only that the node accepts.
                if k == last || !self.accepts(cur, text[pos..].chars().next()?) {
                    return None;
                }
                cur = self.nodes[cur].next;
                k += 1;
                for &x in &sifted[k] {
                    member[x] = k;
                }
                via_epoch += 1;
                continue;
            }
            eps_via[cur] = via_epoch;
            let mut dest = None;
            for &d in &self.nodes[cur].edests {
                if member[d] != k {
                    continue;
                }
                match dest {
                    None => dest = Some(d),
                    // proceed_next_node: take the second branch if the first loops.
                    Some(first) if eps_via[first] == via_epoch => dest = Some(d),
                    Some(_) => {}
                }
            }
            cur = dest?;
        }
        Some(
            regs.iter()
                .map(|&(so, eo)| {
                    (so >= 0 && eo >= 0)
                        .then(|| usize::try_from(so).unwrap()..usize::try_from(eo).unwrap())
                })
                .collect(),
        )
    }

    /// regexec.c:`update_regs`.
    fn update_regs(
        kind: Kind,
        regs: &mut [(isize, isize)],
        prev: &mut [(isize, isize)],
        pos: isize,
    ) {
        match kind {
            Kind::Open(g) => regs[g] = (pos, -1),
            Kind::Close(g, opt) => {
                if regs[g].0 < pos {
                    regs[g].1 = pos;
                    prev.copy_from_slice(regs);
                } else if opt && prev[g].0 != -1 {
                    regs.copy_from_slice(prev);
                } else {
                    regs[g].1 = pos;
                }
            }
            _ => {}
        }
    }
}
