//! The scoped state machine (PLAN §1.4.3, ADR-0005, F4.4): rule in, plain
//! match coordinates out.
//!
//! **`Under` is a hard scope.** The driver visits every node the entry mode
//! selects and runs the program **only over that node's (named,
//! trivia-filtered per ADR-0010) child list** — no instruction can escape
//! the scope, so the state space per pivot is `O(children × states)`.
//!
//! **Linear via memoized failure.** Within a scope, matching is an NFA
//! simulation recorded as a `(child_index, pc)` dynamic program: `accepts`
//! (can the suffix of the program from `pc` match starting exactly at child
//! `i`?) and `consume` (the longest accepting consumption from there).
//! A state that failed at child *i* is never re-explored — each `(i, pc)`
//! is settled exactly once, which is the mechanical no-explosion guarantee
//! KP-7 makes falsifiable. Nested `Under` instructions evaluate their
//! pivot's scope through the same DP, **cached by `(node id, entry pc)`**
//! — a scope is computed once no matter how many outer paths reach it, so
//! global cost is `O(Σ scopes × children × states)`, linear in tree size.
//!
//! **Match set and captures (ADR-0016).** Per pivot, each start index that
//! accepts yields exactly one match: the *local maximum* — the longest
//! accepting run (alternation can offer shorter ones). Captures bind along
//! the canonical accepting path: alternation branches in program order
//! (Pike-VM-style thread priority — the earliest-position binding wins).
//! Reports are plain data (Rule T3): no `Node`, no borrows, no kind names
//! (ids only — names resolve at serialization time, §1.4.2).
//!
//! **Allocation discipline (KP-8, re-anchored here):** steady-state
//! matching allocates nothing per visited node — child lists, DP tables,
//! capture slots, cursors, and the traversal node buffer are pooled and
//! reused (`tests/alloc.rs` fences this). The only growing allocations are
//! the *result vectors* (reports and their captures), which is what the
//! fence's allowance encodes.

use std::collections::HashMap;

use smol_str::SmolStr;

use crate::ast::compile::{Entry, Instr, Program, TextFilter};
use crate::ast::ffi_shim::{Node, Point, Tree, TreeCursor};
use crate::ast::matchset;
use crate::ast::walk::CursorPool;
use crate::error::MatchError;

/// Pathological scope nesting refuses loudly instead of overflowing the
/// stack (scope recursion is one level per `Under` per tree level). 256 is
/// an order of magnitude above real code's nesting (rust bodies nest in the
/// tens) and an order below rayon's 2 MiB worker stacks even with
/// unoptimized frames.
const MAX_SCOPE_DEPTH: usize = 256;

/// `consume` sentinel: no accepting completion from this state.
const DEAD: u32 = u32::MAX;

/// A point in `{row, column}` form; `column` is in BYTES (KP-3 / A.3.4 —
/// the same units `Node::start_position` reports).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PointData {
    pub row: u32,
    pub column: u32,
}

impl From<Point> for PointData {
    fn from(p: Point) -> Self {
        Self {
            row: p.row as u32,
            column: p.column as u32,
        }
    }
}

/// One captured span (the symbol and the nodes it covered).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureReport {
    pub symbol: SmolStr,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointData,
    pub end_point: PointData,
    pub kind_id: u16,
}

/// A match, as plain data (Rule T3): copied values, never a `Node` handle.
/// `overlap` and `rule_id` carry ADR-0016's match-set annotations (set by
/// [`matchset`] / the scan layer).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MatchReport {
    pub path: SmolStr,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: PointData,
    pub end_point: PointData,
    /// The **pivot's** kind id (the scope container, not the first child).
    pub kind_id: u16,
    pub captures: Vec<CaptureReport>,
    /// ADR-0016: span-intersects another reported match in the same file.
    pub overlap: bool,
    pub rule_id: u32,
}

/// Where a scope's child list comes from: the pivot's own (filtered)
/// children, or — for [`Entry::Unit`] — the single node being tested
/// (unfiltered: a unit pattern may legitimately target a comment kind).
enum Children<'tree> {
    Enumerate(Node<'tree>),
    Only(Node<'tree>),
}

/// Per-depth DP state, pooled and reused across scopes and runs (the
/// "memo is a reused bitset" of the KP-8 re-anchor). `prepare` clears in
/// place; buffers only grow, and only until the tree's width/depth maximum
/// has been seen.
struct ScopeFrame<'tree> {
    children: Vec<Node<'tree>>,
    /// `accepts[i][pc]` bitset over `(child_index, pc)`.
    accepts: Vec<u64>,
    /// `under_ok[i][pc]` bitset: the `Under` test at `pc` passed on child i.
    under: Vec<u64>,
    /// `consume[i][pc]`: longest accepting consumption, or [`DEAD`].
    consume: Vec<u32>,
    cap_start: Vec<i64>,
    cap_end: Vec<i64>,
    width: usize,
    rows: usize,
}

impl<'tree> ScopeFrame<'tree> {
    fn new() -> Self {
        Self {
            children: Vec::new(),
            accepts: Vec::new(),
            under: Vec::new(),
            consume: Vec::new(),
            cap_start: Vec::new(),
            cap_end: Vec::new(),
            width: 0,
            rows: 0,
        }
    }

    /// Reset for an `(n children × w pcs)` run with `nsyms` capture slots.
    fn prepare(&mut self, n: usize, w: usize, nsyms: usize) {
        self.width = w;
        self.rows = n + 1;
        let words = (self.rows * w).div_ceil(64);
        self.accepts.resize(words, 0);
        self.accepts.fill(0);
        self.under.resize(words, 0);
        self.under.fill(0);
        self.consume.resize(self.rows * w, DEAD);
        self.consume.fill(DEAD);
        self.cap_start.clear();
        self.cap_start.resize(nsyms, -1);
        self.cap_end.clear();
        self.cap_end.resize(nsyms, -1);
    }

    #[inline]
    fn accepts_get(&self, i: usize, pc: usize) -> bool {
        let bit = i * self.width + pc;
        self.accepts[bit / 64] & (1_u64 << (bit % 64)) != 0
    }

    #[inline]
    fn accepts_set(&mut self, i: usize, pc: usize) {
        let bit = i * self.width + pc;
        self.accepts[bit / 64] |= 1_u64 << (bit % 64);
    }

    #[inline]
    fn under_get(&self, i: usize, pc: usize) -> bool {
        let bit = i * self.width + pc;
        self.under[bit / 64] & (1_u64 << (bit % 64)) != 0
    }

    #[inline]
    fn under_set(&mut self, i: usize, pc: usize) {
        let bit = i * self.width + pc;
        self.under[bit / 64] |= 1_u64 << (bit % 64);
    }

    #[inline]
    fn consume_get(&self, i: usize, pc: usize) -> u32 {
        self.consume[i * self.width + pc]
    }

    #[inline]
    fn consume_set(&mut self, i: usize, pc: usize, len: u32) {
        self.consume[i * self.width + pc] = len;
    }
}

/// One matcher over one compiled program and one tree. Create per file
/// (the scan work item's exact ownership shape: owned tree reference,
/// pooled buffers, plain data out); [`Matcher::matches`] may be called
/// repeatedly (the KP-8 fence does exactly that).
pub struct Matcher<'prog, 'tree> {
    program: &'prog Program,
    tree: &'tree Tree,
    rule_id: u32,
    /// Pivot discovery: the pooled-cursor preorder (Rule T5; walk.rs).
    pool: CursorPool<'tree>,
    /// The preorder node buffer, refilled per `matches` call.
    nodes: Vec<Node<'tree>>,
    /// Depth-pooled DP frames (never shrink; recycled by `prepare`).
    frames: Vec<ScopeFrame<'tree>>,
    /// Current scope recursion depth — the index into `frames`. Tracked
    /// separately from `frames.len()` (which only grows to the max depth
    /// ever seen) so sibling scopes at one depth reuse one frame.
    depth: usize,
    /// Pooled cursors for flat child enumeration (`node.children(&mut c)`).
    cursors: Vec<TreeCursor<'tree>>,
    /// `(node id, entry pc) → accepts-any` — the cross-scope memo that
    /// makes re-entered scopes O(1) (the O(Σ scopes) bound's linchpin).
    scope_cache: HashMap<(usize, u32), bool>,
}

impl<'prog, 'tree> Matcher<'prog, 'tree> {
    pub fn new(program: &'prog Program, tree: &'tree Tree, rule_id: u32) -> Self {
        Self {
            program,
            tree,
            rule_id,
            pool: CursorPool::new(),
            nodes: Vec::new(),
            frames: Vec::new(),
            depth: 0,
            cursors: Vec::new(),
            scope_cache: HashMap::new(),
        }
    }

    /// Run the program over the whole tree, appending every match (in
    /// canonical `(start_byte, end_byte, rule_id)` order per
    /// [`matchset::sort_matches`]) to `out`.
    pub fn matches(
        &mut self,
        path: &SmolStr,
        out: &mut Vec<MatchReport>,
    ) -> Result<(), MatchError> {
        let program = self.program;
        // Preorder pivot discovery through the pooled dfs (deterministic;
        // the buffer is reused, never reallocated after the first fill).
        self.nodes.clear();
        for node in self.pool.dfs(self.tree.root_node()) {
            self.nodes.push(node);
        }

        let mut i = 0;
        while i < self.nodes.len() {
            let node = self.nodes[i];
            let enter = match program.entry {
                Entry::Pivot { kind: Some(k) } => node.kind_id() == k,
                Entry::Pivot { kind: None } | Entry::EveryScope | Entry::Unit => true,
            };
            if enter {
                let depth = self.depth; // 0 at top level
                let children = match program.entry {
                    Entry::Unit => Children::Only(node),
                    _ => Children::Enumerate(node),
                };
                self.scope_dp(0, children)?;
                self.emit_from_frame(depth, node, 0, path, out);
            }
            i += 1;
        }
        Ok(())
    }

    /// The `(node id, entry)`-keyed scope memo. Computed once, reused by
    /// every outer path that reaches the same pivot — the property a naive
    /// backtracking matcher lacks and KP-7's baseline documents.
    fn scope_accepts(&mut self, node: Node<'tree>, entry: u32) -> Result<bool, MatchError> {
        let key = (node.id(), entry);
        if let Some(hit) = self.scope_cache.get(&key) {
            return Ok(*hit);
        }
        let accepts = self.scope_dp(entry, Children::Enumerate(node))?;
        self.scope_cache.insert(key, accepts);
        Ok(accepts)
    }

    /// Compute one scope's DP over the pivot's child list (or the
    /// single-node list for [`Children::Only`]), leaving the tables in
    /// `frames[depth]`. Returns whether any start index accepts.
    fn scope_dp(&mut self, entry: u32, children: Children<'tree>) -> Result<bool, MatchError> {
        let depth = self.depth;
        if depth >= MAX_SCOPE_DEPTH {
            return Err(MatchError::Engine {
                message: format!(
                    "scope nesting exceeds {MAX_SCOPE_DEPTH} — the tree is pathologically deep"
                ),
            });
        }
        if self.frames.len() == depth {
            self.frames.push(ScopeFrame::new());
        }
        self.depth += 1;
        let result = self.scope_dp_frame(depth, entry, children);
        self.depth -= 1;
        result
    }

    /// The scope body, run with `frames[depth]` claimed (depth already
    /// incremented by the caller, so nested scopes claim deeper frames).
    fn scope_dp_frame(
        &mut self,
        depth: usize,
        entry: u32,
        children: Children<'tree>,
    ) -> Result<bool, MatchError> {
        // Fill the child list. The frame is pooled: clear the previous
        // scope's children first (the KP-8 fence's reused-buffer contract —
        // and a fresh list per scope is obviously load-bearing).
        if let Some(frame) = self.frames.get_mut(depth) {
            frame.children.clear();
        }
        // Enumerate uses a pooled cursor; Skip policy removes comment-class
        // kinds HERE, at every scope (KP-6 verdict 3).
        match children {
            Children::Enumerate(pivot) => {
                let mut cursor = match self.cursors.pop() {
                    Some(mut spare) => {
                        spare.reset(pivot);
                        spare
                    }
                    None => pivot.walk(),
                };
                let skip: &[u16] = &self.program.trivia_skip;
                for child in pivot.named_children(&mut cursor) {
                    let keep = skip.is_empty() || !skip.contains(&child.kind_id());
                    if keep && let Some(frame) = self.frames.get_mut(depth) {
                        frame.children.push(child);
                    }
                }
                self.cursors.push(cursor);
            }
            Children::Only(single) => {
                if let Some(frame) = self.frames.get_mut(depth) {
                    frame.children.push(single);
                }
            }
        }

        let program = self.program;
        let w = program.len();
        let nsyms = program.symbols.len();
        let n = self.frames.get(depth).map_or(0, |f| f.children.len());
        if let Some(frame) = self.frames.get_mut(depth) {
            frame.prepare(n, w, nsyms);
        }

        // Pre-pass: settle every `Under` test for this scope, restricted to
        // the entered region — other regions' instructions are unreachable
        // here, and testing them would make deep rules quadratic in pattern
        // depth. The tests recurse into nested scopes (cache-guarded), so
        // the frame borrow is released around each call — children are
        // `Copy` handles.
        let region_exit = program.region_exit(entry) as usize;
        let mut i = 0;
        while i < n {
            let child = match self.frames.get(depth).and_then(|f| f.children.get(i)) {
                Some(child) => *child,
                None => break,
            };
            for pc in entry as usize..=region_exit.min(w.saturating_sub(1)) {
                if let Instr::Under(kind, sub_entry, _) = program.instrs()[pc] {
                    let kind_ok = kind.is_none_or(|k| child.kind_id() == k);
                    let accepted = if kind_ok {
                        self.scope_accepts(child, sub_entry)?
                    } else {
                        false
                    };
                    if accepted && let Some(frame) = self.frames.get_mut(depth) {
                        frame.under_set(i, pc);
                    }
                }
            }
            i += 1;
        }

        // The DP: for each (i, pc) of the region — children descending, pc
        // descending within a row (epsilon targets are forward-only by
        // construction, and confined to the region). Pure table work: no
        // recursion, no allocation.
        let accepts_any;
        {
            let frame = match self.frames.get_mut(depth) {
                Some(frame) => frame,
                None => {
                    return Err(MatchError::Engine {
                        message: "scope frame vanished mid-DP".to_owned(),
                    });
                }
            };
            let instrs = program.instrs();
            for i in (0..=n).rev() {
                for pc in (entry as usize..=region_exit.min(w.saturating_sub(1))).rev() {
                    let (ok, cons) = match instrs[pc] {
                        Instr::Match => (true, 0),
                        Instr::Jmp(t) => {
                            let t = t as usize;
                            (frame.accepts_get(i, t), frame.consume_get(i, t))
                        }
                        Instr::Split(a, b) => {
                            let (a, b) = (a as usize, b as usize);
                            let ok = frame.accepts_get(i, a) || frame.accepts_get(i, b);
                            // DEAD is u32::MAX — a plain max would let a
                            // dead arm poison a live one. Max over the live
                            // arms only.
                            let (ca, cb) = (frame.consume_get(i, a), frame.consume_get(i, b));
                            let cons = if ca == DEAD {
                                cb
                            } else if cb == DEAD {
                                ca
                            } else {
                                ca.max(cb)
                            };
                            (ok, cons)
                        }
                        Instr::CapStart(_) | Instr::CapEnd(_) => {
                            (frame.accepts_get(i, pc + 1), frame.consume_get(i, pc + 1))
                        }
                        Instr::Kind(k) => {
                            let ok = i < n
                                && frame.children[i].kind_id() == k
                                && frame.accepts_get(i + 1, pc + 1);
                            let cons = if ok {
                                frame.consume_get(i + 1, pc + 1).saturating_add(1)
                            } else {
                                DEAD
                            };
                            (ok, cons)
                        }
                        Instr::Any => {
                            let ok = i < n && frame.accepts_get(i + 1, pc + 1);
                            let cons = if ok {
                                frame.consume_get(i + 1, pc + 1).saturating_add(1)
                            } else {
                                DEAD
                            };
                            (ok, cons)
                        }
                        Instr::Under(..) => {
                            let ok =
                                i < n && frame.under_get(i, pc) && frame.accepts_get(i + 1, pc + 1);
                            let cons = if ok {
                                frame.consume_get(i + 1, pc + 1).saturating_add(1)
                            } else {
                                DEAD
                            };
                            (ok, cons)
                        }
                    };
                    if ok {
                        frame.accepts_set(i, pc);
                        frame.consume_set(i, pc, cons);
                    }
                }
            }
            accepts_any = (0..n).any(|s| frame.accepts_get(s, entry as usize));
        }
        Ok(accepts_any)
    }

    /// Emit one report per accepting start in `frames[depth]` — the local
    /// maximum (longest run) per start, captures bound along the canonical
    /// priority path. No recursion: runs after the scope's DP is settled.
    fn emit_from_frame(
        &mut self,
        depth: usize,
        pivot: Node<'tree>,
        entry: u32,
        path: &SmolStr,
        out: &mut Vec<MatchReport>,
    ) {
        let program = self.program;
        let instrs = program.instrs();
        let Some(frame) = self.frames.get_mut(depth) else {
            return;
        };
        let n = frame.children.len();
        for s in 0..n {
            if !frame.accepts_get(s, entry as usize) {
                continue;
            }
            let len = frame.consume_get(s, entry as usize);
            if len == DEAD || len == 0 {
                continue; // unreachable for v0.1 patterns (every path consumes >= 1)
            }

            // The canonical path: at each fork, the first (program-order)
            // branch that preserves the maximal length — Pike-VM thread
            // priority, earliest-position captures.
            frame.cap_start.fill(-1);
            frame.cap_end.fill(-1);
            let mut pc = entry as usize;
            let mut ci = s;
            while pc < instrs.len() && !matches!(instrs[pc], Instr::Match) {
                match instrs[pc] {
                    Instr::Jmp(t) => pc = t as usize,
                    Instr::Split(a, b) => {
                        let need = frame.consume_get(ci, pc);
                        let a = a as usize;
                        let via_a =
                            frame.consume_get(ci, a) != DEAD && frame.consume_get(ci, a) == need;
                        pc = if via_a { a } else { b as usize };
                    }
                    Instr::CapStart(sid) => {
                        frame.cap_start[sid as usize] = ci as i64;
                        pc += 1;
                    }
                    Instr::CapEnd(sid) => {
                        frame.cap_end[sid as usize] = ci as i64;
                        pc += 1;
                    }
                    // Consuming instructions on an accepting path always
                    // match the child under them — advance both cursors.
                    Instr::Kind(_) | Instr::Any | Instr::Under(..) => {
                        ci += 1;
                        pc += 1;
                    }
                    Instr::Match => break,
                }
            }

            let Some(first) = frame.children.get(s).copied() else {
                continue;
            };
            let Some(last) = frame.children.get(s + len as usize - 1).copied() else {
                continue;
            };

            let mut captures = Vec::new();
            for (sid, symbol) in program.symbols.iter().enumerate() {
                let (start, end) = (
                    frame.cap_start.get(sid).copied().unwrap_or(-1),
                    frame.cap_end.get(sid).copied().unwrap_or(-1),
                );
                if start < 0 || end < 0 || end <= start {
                    continue; // symbol unbound on this path (untaken branch)
                }
                let (Some(from), Some(to)) = (
                    frame.children.get(start as usize).copied(),
                    frame.children.get(end as usize - 1).copied(),
                ) else {
                    continue;
                };
                captures.push(CaptureReport {
                    symbol: symbol.clone(),
                    start_byte: from.start_byte(),
                    end_byte: to.end_byte(),
                    start_point: from.start_position().into(),
                    end_point: to.end_position().into(),
                    kind_id: from.kind_id(),
                });
            }
            captures.sort_by(|a, b| {
                a.start_byte
                    .cmp(&b.start_byte)
                    .then_with(|| a.symbol.cmp(&b.symbol))
            });

            out.push(MatchReport {
                path: path.clone(),
                start_byte: first.start_byte(),
                end_byte: last.end_byte(),
                start_point: first.start_position().into(),
                end_point: last.end_position().into(),
                kind_id: pivot.kind_id(),
                captures,
                overlap: false,
                rule_id: self.rule_id,
            });
        }
    }
}

/// Convenience for single-file evaluation (tests, the bench baseline
/// comparisons): parse-free entry over an already-parsed tree.
pub fn match_tree(
    program: &Program,
    tree: &Tree,
    rule_id: u32,
    path: &SmolStr,
) -> Result<Vec<MatchReport>, MatchError> {
    let mut out = Vec::new();
    let mut matcher = Matcher::new(program, tree, rule_id);
    matcher.matches(path, &mut out)?;
    matchset::sort_matches(&mut out);
    Ok(out)
}

/// The v0.3 B1 post-match leaf-text filters: every clause must hold on the
/// match's captures (AND), and a failing clause kills **that match only** —
/// the NFA run, its DP, and the capture binding are untouched (the
/// linear-time promise never includes the filter pass; it is
/// `O(matches × clauses)` with a bounded root-to-leaf walk per tested
/// capture, so it composes with the matcher's `O(tree)` the way unify's
/// fingerprint pass does).
///
/// A clause's **leaf text** is the source bytes of the capture's leftmost
/// **named leaf**: descend through named children only, and the first node
/// with none carries the text. That is the node itself for leaf captures
/// (an `identifier`, a `field_identifier` — and a comment, whose text
/// token is anonymous in some grammars, so the comment node IS the leaf),
/// and the name token for name-carrying subtrees (`macro_invocation` →
/// `panic`, `call_expression` → the callee's first token). A symbol
/// unbound on a particular match (an untaken alternation branch) filters
/// that match, like any failing predicate — same rule as `unify`.
pub fn filter_text(program: &Program, tree: &Tree, bytes: &[u8], reports: &mut Vec<MatchReport>) {
    if program.filters.is_empty() {
        return;
    }
    reports.retain(|report| {
        program.filters.iter().all(|filter| {
            let Some(name) = program.symbols.get(filter_sym(filter) as usize) else {
                return false;
            };
            let Some(cap) = report.captures.iter().find(|c| c.symbol == *name) else {
                return false; // symbol unbound on this path: filtered
            };
            let text = leftmost_leaf_text(tree, bytes, cap.start_byte, cap.end_byte);
            match filter {
                TextFilter::Text { needle, .. } => text == needle.as_bytes(),
                TextFilter::Glob { glob, .. } => glob.is_match(text),
            }
        })
    });
}

/// The interned symbol id a compiled filter predicates.
fn filter_sym(filter: &TextFilter) -> u32 {
    match filter {
        TextFilter::Text { sym, .. } | TextFilter::Glob { sym, .. } => *sym,
    }
}

/// The leftmost named leaf inside `[start, end)`: descend from the root
/// through NAMED children (captures bind named children, and named nodes
/// nest under named nodes), at each level entering the first one whose
/// span intersects the capture, until a node with no named children — then
/// its source bytes. Named children are ordered and non-overlapping, so
/// the first intersector is the child containing `start` (found by binary
/// search, never a scan) or the first child starting at/after it; the walk
/// is `O(depth × log fanout)` per tested capture and allocates nothing —
/// the shape the `b1/filter-text` bench family pins against regressing
/// into a fanout scan (which would make the filter pass quadratic in wide
/// scopes).
fn leftmost_leaf_text<'a>(tree: &'a Tree, bytes: &'a [u8], start: usize, end: usize) -> &'a [u8] {
    let mut node = tree.root_node();
    loop {
        if node.named_child_count() == 0 {
            return &bytes[node.start_byte()..node.end_byte()];
        }
        // Lower bound over the ordered, disjoint child spans: the first
        // child whose start is >= `start`. The intersector is that child
        // (if it starts inside the capture) or its predecessor (the only
        // child that can contain `start`).
        let count = node.named_child_count() as u32;
        let mut lo = 0_u32;
        let mut hi = count;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if node
                .named_child(mid)
                .is_some_and(|c| c.start_byte() < start)
            {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        let next = node
            .named_child(lo)
            .filter(|c| c.start_byte() < end)
            .or_else(|| {
                lo.checked_sub(1)
                    .and_then(|prev| node.named_child(prev))
                    .filter(|c| c.end_byte() > start)
            });
        let Some(child) = next else {
            return &bytes[..0]; // unreachable for capture-born spans
        };
        node = child;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::compile::{TriviaPolicy, compile_rule};
    use crate::ast::dsl::parse_rule;
    use crate::ast::lang::LangRegistry;
    use crate::ast::parse::parse_snapshot;

    #[cfg(feature = "lang-rust")]
    fn find(rule: &str, src: &str, policy: TriviaPolicy) -> Vec<(usize, usize)> {
        let registry = LangRegistry::new();
        let lang = registry.for_extension("rs").expect("lang-rust default");
        let parsed = parse_rule(rule).expect("test rule parses");
        let program = compile_rule(&parsed, &registry, lang, policy).expect("test rule compiles");
        let mut parser = registry.parser(lang).expect("parser");
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(src.as_bytes());
        let tree = parse_snapshot(&mut parser, &bytes).expect("test source parses");
        let reports = match_tree(&program, &tree, 0, &SmolStr::from("t.rs")).expect("match runs");
        reports.iter().map(|r| (r.start_byte, r.end_byte)).collect()
    }

    /// KP-6 pattern 1 on its curated snippet: strict 1, skip 2 (the
    /// comment-separated `(c,d)` pair is the whole point of ADR-0010).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn let_adjacency_strict_vs_skip() {
        let src = "fn curated_let(v: u32) -> u32 {\n    let a = v;\n    let b = a;\n    b;\n    let c = 1;\n    // comment between lets\n    let d = 2;\n    d\n}\n";
        let rule = "(under block (seq (kind let_declaration) (kind let_declaration)))";
        assert_eq!(
            find(rule, src, TriviaPolicy::Strict).len(),
            1,
            "strict: only the clean (a,b) pair"
        );
        let skip = find(rule, src, TriviaPolicy::Skip);
        assert_eq!(skip.len(), 2, "skip: the comment-separated (c,d) pair too");
        // Spans are the matched runs, byte-exact against the source.
        let a = src.find("let a = v;").unwrap_or_default();
        let b_end = src.find("let b = a;").unwrap_or_default() + "let b = a;".len();
        assert_eq!(skip[0], (a, b_end));
        let c = src.find("let c = 1;").unwrap_or_default();
        let d_end = src.find("let d = 2;").unwrap_or_default() + "let d = 2;".len();
        assert_eq!(skip[1], (c, d_end));
    }

    /// Every start position reports once (local maxima per pivot): three
    /// consecutive lets yield two overlapping-in-siblings matches, both
    /// reported — same pivot, adjacent starts.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn every_start_position_reports_its_local_maximum() {
        let src = "fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    a\n}\n";
        let rule = "(under block (seq (kind let_declaration) (kind let_declaration)))";
        let matches = find(rule, src, TriviaPolicy::Skip);
        assert_eq!(matches.len(), 2, "starts 0 and 1");
    }

    /// Captures bind byte-exact spans of what they wrapped, and the match
    /// span is the whole run. (The rust grammar's `function_item` children
    /// are `identifier, parameters, [return type], block` — the doc
    /// example's shape against the real adjacency.)
    #[test]
    #[cfg(feature = "lang-rust")]
    fn captures_bind_the_covered_span() {
        let src = "fn alpha(x: u32) -> u32 { x }\n";
        let rule = "(under function_item (seq (capture @name identifier) (kind parameters)))";
        let registry = LangRegistry::new();
        let lang = registry.for_extension("rs").unwrap();
        let parsed = parse_rule(rule).unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
        let mut parser = registry.parser(lang).unwrap();
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(src.as_bytes());
        let tree = parse_snapshot(&mut parser, &bytes).unwrap();
        let reports = match_tree(&program, &tree, 0, &SmolStr::from("t.rs")).unwrap();

        assert_eq!(reports.len(), 1);
        let report = &reports[0];
        let name_at = src.find("alpha").unwrap_or_default();
        let params_at = src.find("(x: u32)").unwrap_or_default();
        assert_eq!(report.start_byte, name_at, "the run starts at the fn name");
        assert_eq!(
            report.end_byte,
            params_at + "(x: u32)".len(),
            "the run ends at the parameters (strict adjacency)"
        );
        let fn_kind = registry.resolve_kind(lang, "function_item").unwrap();
        assert_eq!(report.kind_id, fn_kind, "the report carries the PIVOT kind");
        assert_eq!(report.captures.len(), 1);
        let cap = &report.captures[0];
        assert_eq!(cap.symbol.as_str(), "name");
        assert_eq!(cap.start_byte, name_at);
        assert_eq!(cap.end_byte, name_at + "alpha".len());
    }

    /// Nested `Under` descends one scope: the element consumes a child of
    /// the pivot kind whose own children must match the inner body. (In the
    /// rust grammar a *statement-position* `if` is wrapped in an
    /// `expression_statement`, so the outer pivot here is `_` — the
    /// `if_expression` is a direct child of that wrapper.)
    #[test]
    #[cfg(feature = "lang-rust")]
    fn nested_under_tests_the_childs_own_scope() {
        let with_else = "fn a(x: bool) -> u32 {\n    if x { 1 } else { 2 }\n}\n";
        let without_else = "fn b(x: bool) -> u32 {\n    if x { 1 }\n}\n";
        let rule = "(under _ (seq (under if_expression (seq _ (kind block) (kind else_clause)))))";
        assert_eq!(find(rule, with_else, TriviaPolicy::Skip).len(), 1);
        assert!(
            find(rule, without_else, TriviaPolicy::Skip).is_empty(),
            "the inner scope must reject an if without else"
        );
    }

    /// Alternation resolves to the local maximum: `(any-of a (seq b b))`
    /// starting at a `b b` run must consume two children, not one.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn any_of_picks_the_longest_accepting_run() {
        let src = "fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n";
        // (any-of let_declaration (seq let_declaration let_declaration)):
        // at start 0 the two-element branch accepts → length 2; start 1
        // only the single branch → length 1.
        let rule = "(under block (seq (any-of (kind let_declaration) (seq (kind let_declaration) (kind let_declaration)))))";
        let matches = find(rule, src, TriviaPolicy::Skip);
        assert_eq!(matches.len(), 2);
        let first_run = src.find("let a").unwrap_or_default();
        let second_end = src.find("let b = 2;").unwrap_or_default() + "let b = 2;".len();
        assert_eq!(matches[0], (first_run, second_end), "local maximum: 2 lets");
        let b = src.find("let b").unwrap_or_default();
        assert_eq!(matches[1], (b, second_end), "only the single branch fits");
    }

    /// Top-level shapes: a bare kind matches every node (including root);
    /// a top-level seq runs in every scope (docs/DSL.md §3).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn top_level_shapes_unit_and_every_scope() {
        let src = "fn a() {}\nfn b() {}\n";
        // Unit: every function_item node — 2.
        assert_eq!(find("function_item", src, TriviaPolicy::Skip).len(), 2);
        // Unit wildcard: every node in the tree (root included) — more
        // than the function count, deterministic.
        assert!(find("_", src, TriviaPolicy::Skip).len() > 2);
        // EveryScope: a top-level seq of 2 matches among every node's
        // children — here the source_file's two fns.
        let seq = find("(seq function_item function_item)", src, TriviaPolicy::Skip);
        assert_eq!(
            seq.len(),
            1,
            "exactly one adjacent fn pair (the root scope)"
        );
    }

    /// Wildcard pivot scopes every node: the pair matches exactly once, in
    /// its actual container (a `_` pivot adds scopes, not matches — only
    /// the block holds both lets as direct children).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn wildcard_pivot_scopes_every_node() {
        let src = "fn f() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n";
        let rule = "(under _ (seq (kind let_declaration) (kind let_declaration)))";
        assert_eq!(find(rule, src, TriviaPolicy::Skip).len(), 1);
    }

    /// v0.3 B1: `filter_text` predicates a capture's LEAF TEXT — the node
    /// itself for leaf captures, the leftmost leaf (the name token) for
    /// subtree captures. A failing clause kills that match only; an unbound
    /// symbol filters the match; text and glob clauses AND.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn text_filters_predicate_leaf_text() {
        let src = concat!(
            "fn demo() {\n",
            "    panic!(\"boom\");\n",
            "    panic!();\n",
            "    format!(\"{}\", 1);\n",
            "    assert!(true);\n",
            "    let x = vec![1];\n",
            "    x\n",
            "}\n",
        );
        let registry = LangRegistry::new();
        let lang = registry.for_extension("rs").unwrap();
        let mut parser = registry.parser(lang).unwrap();
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(src.as_bytes());
        let tree = parse_snapshot(&mut parser, &bytes).unwrap();

        let run = |rule: &str| -> Vec<String> {
            let parsed = parse_rule(rule).unwrap();
            let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
            let mut reports =
                match_tree(&program, &tree, 0, &SmolStr::from("t.rs")).expect("match runs");
            filter_text(&program, &tree, &bytes, &mut reports);
            reports
                .iter()
                .map(|r| src[r.start_byte..r.end_byte].to_string())
                .collect()
        };

        // The acceptance shape: subtree captures predicate their NAME
        // token — `(text @m "panic")` keeps the panic! sites and only
        // those (format!/assert!/vec! drop out; the bare `panic!()` still
        // counts, its macro name is the same leaf).
        let kept =
            run("(under _ (seq (capture @m macro_invocation)) (filters (text @m \"panic\")))");
        assert_eq!(
            kept,
            vec!["panic!(\"boom\")".to_owned(), "panic!()".to_owned()],
            "exactly the panic! sites, name-token predicated"
        );

        // A glob over the same capture predicates the same leaf text —
        // here: the name with a byte-class member.
        let globbed =
            run("(under _ (seq (capture @m macro_invocation)) (filters (glob @m \"p[a-z]nic\")))");
        assert_eq!(
            globbed,
            vec!["panic!(\"boom\")".to_owned(), "panic!()".to_owned()],
            "the glob predicates the leaf text, not the covered span"
        );

        // Leaf captures compare the node itself — method names.
        let src2 = "fn z() -> u32 { let v = z(1); v.unwrap() + v.clone() }\n";
        let bytes2: std::sync::Arc<[u8]> = std::sync::Arc::from(src2.as_bytes());
        let tree2 = parse_snapshot(&mut parser, &bytes2).unwrap();
        let parsed = parse_rule(
            "(under field_expression (seq _ (capture @m field_identifier)) \
             (filters (text @m \"unwrap\")))",
        )
        .unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
        let mut reports = match_tree(&program, &tree2, 0, &SmolStr::from("t.rs")).unwrap();
        filter_text(&program, &tree2, &bytes2, &mut reports);
        assert_eq!(reports.len(), 1, "clone drops out, unwrap stays");

        // A comment's leaf text is the WHOLE comment: its text token is
        // anonymous in the rust grammar, so the comment node is the
        // leftmost named leaf itself (strict trivia — comments are skip-
        // mode trivia and never siblings otherwise).
        let src3 = "// TODO: strict mode sees comments\nfn z2() -> u32 { 1 }\n";
        let bytes3: std::sync::Arc<[u8]> = std::sync::Arc::from(src3.as_bytes());
        let tree3 = parse_snapshot(&mut parser, &bytes3).unwrap();
        let parsed =
            parse_rule("(under _ (seq (capture @c line_comment)) (filters (glob @c \"*TODO*\")))")
                .unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Strict).unwrap();
        let mut reports = match_tree(&program, &tree3, 0, &SmolStr::from("t.rs")).unwrap();
        filter_text(&program, &tree3, &bytes3, &mut reports);
        assert_eq!(reports.len(), 1, "the TODO comment survives the glob");
        let parsed =
            parse_rule("(under _ (seq (capture @c line_comment)) (filters (glob @c \"*FIXME*\")))")
                .unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Strict).unwrap();
        let mut reports = match_tree(&program, &tree3, 0, &SmolStr::from("t.rs")).unwrap();
        filter_text(&program, &tree3, &bytes3, &mut reports);
        assert!(reports.is_empty(), "no FIXME, no match");

        // Multiple clauses AND: text "unwrap" AND glob "un*" both hold.
        let parsed = parse_rule(
            "(under field_expression (seq _ (capture @m field_identifier)) \
             (filters (text @m \"unwrap\") (glob @m \"un*\")))",
        )
        .unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
        let mut reports = match_tree(&program, &tree2, 0, &SmolStr::from("t.rs")).unwrap();
        filter_text(&program, &tree2, &bytes2, &mut reports);
        assert_eq!(reports.len(), 1);

        // An unbound symbol (untaken any-of branch) filters the match —
        // the same rule as unify.
        let parsed = parse_rule(
            "(under field_expression (seq (any-of _ (capture @m field_identifier))) \
             (filters (text @m \"unwrap\")))",
        )
        .unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
        let mut reports = match_tree(&program, &tree2, 0, &SmolStr::from("t.rs")).unwrap();
        filter_text(&program, &tree2, &bytes2, &mut reports);
        assert!(
            reports
                .iter()
                .all(|r| r.captures.iter().any(|c| c.symbol.as_str() == "m")),
            "only matches that actually bound @m survive"
        );
    }

    /// Pathological nesting refuses with the typed engine error instead of
    /// a stack overflow (scan surfaces it as E_MATCH). The chain: nested
    /// braces give a ~3000-level tree, and a 1100-deep wildcard-under rule
    /// walks one scope per level until the guard trips.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn deep_nesting_is_a_typed_error_not_a_crash() {
        let mut src = String::from("fn f() {\n");
        for _ in 0..1500 {
            src.push_str("{\n");
        }
        for _ in 0..1500 {
            src.push_str("}\n");
        }
        src.push_str("}\n");
        let mut rule = String::new();
        for _ in 0..400 {
            rule.push_str("(under _ ");
        }
        rule.push('_');
        for _ in 0..400 {
            rule.push(')');
        }
        let registry = LangRegistry::new();
        let lang = registry.for_extension("rs").unwrap();
        let parsed = parse_rule(&rule).unwrap();
        let program = compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).unwrap();
        let mut parser = registry.parser(lang).unwrap();
        let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(src.as_bytes());
        let tree = parse_snapshot(&mut parser, &bytes).unwrap();
        let err = match_tree(&program, &tree, 0, &SmolStr::from("deep.rs")).unwrap_err();
        assert!(matches!(err, MatchError::Engine { .. }), "got {err:?}");
    }
}
