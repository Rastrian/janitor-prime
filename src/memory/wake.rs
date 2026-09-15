//! `wake(budget)` — token-budgeted tiling recall (PLAN §1.5.2, ADR-0011).
//!
//! One chronological **oldest-first** pass interleaving raw facts and
//! summaries (the OptMem recall shape, reimplemented — A.2), under a token
//! budget with the `chars4` estimator: each item's cost is
//! `ceil(bytes/4)`, declared in the payload as `"estimator": "chars4"`.
//!
//! The algorithm, in the plan's own words and in order:
//!
//! 1. **everything fits ⇒ nothing is compressed** — when all facts raw fit
//!    the budget, all facts are served raw and TREE is never consulted
//!    (so an unsettle summary can never block a wake that needs none);
//! 2. **biased tiling** — otherwise the id range is tiled greedily from
//!    the oldest end with aligned power-of-two blocks, and a block stays
//!    whole only if its size is small relative to its *age* (the number
//!    of newer facts): `size ≤ α × age` with `α =` [`PRESENT_BIAS_ALPHA`]
//!    (present-bias: detail spends on the recent, coarseness on the
//!    ancient; a block at the very end has age 0 and demotes to raw, so
//!    the newest facts are raw by construction); locked facts are
//!    force-split out — any block containing one demotes until it is raw,
//!    and locked facts are **pinned**: never compressed, never dropped by
//!    truncation, always surfaced raw at their position (§1.5.1);
//! 3. **over budget** — adjacent sibling blocks merge (oldest mergeable
//!    pair first) into their parent until the payload fits; if merging is
//!    exhausted, whole tiles are dropped **from the oldest end** (pinned
//!    locks excepted) and `truncated: true` is reported — the newest
//!    material survives longest;
//! 4. **leftover budget** splits the rightmost of the largest compressed
//!    blocks into its two halves while the budget allows ("spend what is
//!    left on the present"; a settled block's children always exist — the
//!    tree store enforces children-before-parents — so refinement never
//!    trips the gate);
//! 5. **completeness gate** — any block the layout *needs* whose summary
//!    is not yet written is the typed [`MemoryError::NapPending`] refusal
//!    carrying the pending compression request: never holes served
//!    silently. The success payload carries `"awake": true`.
//!
//! The budget governs the *memory* payload only; the global layer's
//! standing text is reported with its own estimate alongside it.

use serde::Serialize;

use crate::error::{JanitorError, MemoryError};
use crate::memory::log::{FactRecord, LogWarning, MemoryFlag};
use crate::memory::nap;
use crate::memory::pointer::SyntaxPointer;
use crate::memory::tree_store::{BlockPlan, BlockSlot, TreeStore};

/// The default budget (`--budget` overrides): OptMem budgets ~96 lines ≈
/// 8k tokens; we budget tokens directly (§1.5.2).
pub const DEFAULT_BUDGET: u64 = 8192;

/// Present-bias constant: a block stays whole only if
/// `size ≤ PRESENT_BIAS_ALPHA × age`, where age is the count of newer facts.
pub const PRESENT_BIAS_ALPHA: u64 = 8;

/// The estimator declared in every wake payload (ADR-0011).
pub const ESTIMATOR: &str = "chars4";

/// Token cost of `bytes` of payload text (`ceil(bytes/4)`, ADR-0011).
pub fn tokens(bytes: usize) -> u64 {
    (bytes as u64).div_ceil(4)
}

/// One item of the wake payload.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WakeItem {
    /// A raw fact (locked facts always arrive this way).
    Fact {
        id: u64,
        text: String,
        ptr: Option<SyntaxPointer>,
        flags: Vec<MemoryFlag>,
    },
    /// A stored summary standing in for its whole block.
    Summary {
        /// Slot notation, e.g. `"0-8"`.
        slot: String,
        /// Covered fact ids `[lo, hi)`.
        covers: (u64, u64),
        text: String,
    },
}

/// The wake payload core (the context layer wraps this with the global
/// layer, STATE.json status, and hints).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct WakeReport {
    /// Present iff the payload is complete (a gate refusal is an error, so
    /// success always reports `true` — §1.5.2).
    pub awake: bool,
    pub budget_tokens: u64,
    pub estimator: &'static str,
    /// Sum of the served items' costs (the number the budget governs).
    pub estimate: u64,
    /// True when tiles were dropped from the oldest end to fit the budget.
    pub truncated: bool,
    pub items: Vec<WakeItem>,
    pub warnings: Vec<LogWarning>,
}

/// One tile of the layout during construction.
#[derive(Clone, Debug)]
enum Tile {
    /// A raw fact (pinned when the fact is locked).
    Fact {
        id: u64,
        locked: bool,
    },
    Block {
        slot: BlockSlot,
        text: String,
    },
}

impl Tile {
    fn cost(&self, facts: &[FactRecord]) -> u64 {
        match self {
            Self::Fact { id, .. } => tokens(facts[*id as usize].text.len()),
            Self::Block { text, .. } => tokens(text.len()),
        }
    }
}

/// Run `wake`: the budgeted tiling recall over one log + tree pair. The
/// warnings collected at log load ride along unchanged.
pub fn wake(
    facts: &[FactRecord],
    store: &TreeStore,
    budget: u64,
    warnings: Vec<LogWarning>,
) -> Result<WakeReport, JanitorError> {
    if budget == 0 {
        return Err(MemoryError::BudgetInvalid {
            detail: "budget must be at least 1 token".to_owned(),
        }
        .into());
    }

    // 1. Everything fits raw ⇒ nothing is compressed, TREE untouched.
    let raw_total: u64 = facts.iter().map(|fact| tokens(fact.text.len())).sum();
    if raw_total <= budget {
        return Ok(WakeReport {
            awake: true,
            budget_tokens: budget,
            estimator: ESTIMATOR,
            estimate: raw_total,
            truncated: false,
            items: facts.iter().map(raw_item).collect(),
            warnings,
        });
    }

    // 2. Biased tiling — every block it names is validated (the gate).
    let mut tiles = tile(facts, store)?;
    let mut truncated = false;

    // 3. Over budget: merge the oldest mergeable sibling pair into its
    //    parent while the payload still does not fit.
    while total_cost(&tiles, facts) > budget {
        match merge_oldest(facts, store, &tiles)? {
            Some((range, parent)) => {
                tiles.splice(range..range + 2, std::iter::once(parent));
            }
            None => break,
        }
    }

    // Still over budget: drop whole tiles from the oldest end, pinned
    // locks excepted — the newest material survives longest.
    while total_cost(&tiles, facts) > budget {
        let Some(drop_at) = tiles.iter().position(|tile| match tile {
            Tile::Fact { locked, .. } => !*locked,
            Tile::Block { .. } => true,
        }) else {
            break; // only pinned locks remain: keep them, honestly over
        };
        tiles.remove(drop_at);
        truncated = true;
    }

    // 4. Leftover budget: split the rightmost of the largest blocks into
    //    its two halves while affordable ("spend what is left on the
    //    present"). Splits strictly shrink tile sizes, so this terminates.
    loop {
        let total = total_cost(&tiles, facts);
        let Some(at) = pick_split(&tiles) else { break };
        let Tile::Block { slot, text } = &tiles[at] else {
            unreachable!("pick_split only names blocks");
        };
        let replacement = split_children(facts, store, slot)?;
        if replacement.is_empty() {
            break; // children unavailable (rebuilt tree); refinement is optional
        }
        let added = total_cost(&replacement, facts) - tokens(text.len());
        if total + added > budget {
            break;
        }
        tiles.splice(at..at + 1, replacement.into_iter().collect::<Vec<_>>());
    }

    let estimate: u64 = tiles.iter().map(|tile| tile.cost(facts)).sum();
    let items = tiles
        .into_iter()
        .map(|tile| match tile {
            Tile::Fact { id, .. } => raw_item(&facts[id as usize]),
            Tile::Block { slot, text } => WakeItem::Summary {
                slot: slot.notation(),
                covers: (slot.lo, slot.lo + slot.size),
                text,
            },
        })
        .collect();
    Ok(WakeReport {
        awake: true,
        budget_tokens: budget,
        estimator: ESTIMATOR,
        estimate,
        truncated,
        items,
        warnings,
    })
}

/// The raw-fact payload item for one record.
fn raw_item(fact: &FactRecord) -> WakeItem {
    WakeItem::Fact {
        id: fact.id,
        text: fact.text.clone(),
        ptr: fact.ptr.clone(),
        flags: fact.flags.clone(),
    }
}

fn total_cost(tiles: &[Tile], facts: &[FactRecord]) -> u64 {
    tiles.iter().map(|tile| tile.cost(facts)).sum()
}

/// The biased tiling of `[0, n)` (step 2): greedy from the oldest end; at
/// each position the largest aligned power-of-two block that fits the
/// remaining range, demoted past locked ranges, then demoted until the
/// bias rule (`size ≤ α × age`) holds or the tile is a single raw fact.
fn tile(facts: &[FactRecord], store: &TreeStore) -> Result<Vec<Tile>, JanitorError> {
    let plan = BlockPlan::new(facts);
    let n = plan.fact_count();
    let mut tiles = Vec::new();
    let mut p = 0u64;
    while p < n {
        let fit = n - p;
        let align = p.trailing_zeros().min(62);
        let mut size: u64 = 1u64 << align;
        while size > fit {
            size /= 2;
        }
        // Lock-freedom first, then present-bias — demotion only shrinks,
        // and both invariants are preserved by subranges of a valid block.
        while size > 1 && !plan.is_compressible(facts, size, p) {
            size /= 2;
        }
        while size > 1 && size > PRESENT_BIAS_ALPHA * (n - (p + size)) {
            size /= 2;
        }
        if size == 1 {
            tiles.push(Tile::Fact {
                id: p,
                locked: facts[p as usize].is_locked(),
            });
        } else {
            let slot = BlockSlot { size, lo: p };
            let text = read_or_gate(facts, store, &slot)?;
            tiles.push(Tile::Block { slot, text });
        }
        p += size;
    }
    Ok(tiles)
}

/// A block the layout needs: its stored summary, or — the completeness
/// gate (§1.5.2) — the [`MemoryError::NapPending`] refusal carrying the
/// compression request, never a silently served hole.
fn read_or_gate(
    facts: &[FactRecord],
    store: &TreeStore,
    slot: &BlockSlot,
) -> Result<String, JanitorError> {
    match store.summary(facts, slot.size, slot.lo)? {
        Some(text) => Ok(text),
        None => Err(pending_error(facts, store, slot)),
    }
}

/// Build the E_NAP_PENDING refusal. `slot` names the block the layout
/// *needs*; the `request` payload carries the protocol's next **actionable**
/// step (`nap`'s canonical next-pending — dense prefix and
/// children-before-parents respected), which may be a descendant of
/// `slot` when the tree has not been built that deep yet. An agent that
/// follows `request` slot by slot converges: every settle is permanent
/// and the tree is finite.
fn pending_error(facts: &[FactRecord], store: &TreeStore, slot: &BlockSlot) -> JanitorError {
    let request = nap::next_pending(facts, store)
        .ok()
        .and_then(|plan| plan.request)
        .and_then(|request| serde_json::to_value(&request).ok())
        .unwrap_or_else(|| serde_json::json!({ "slot": slot.notation() }));
    MemoryError::NapPending {
        slot: slot.notation(),
        request,
    }
    .into()
}

/// The oldest mergeable adjacent sibling pair (both blocks, same size,
/// aligned parent) merged into its parent block — `(range, parent)`,
/// where the parent replaces `tiles[range..range + 2]`. The parent's
/// summary is read through the same gate (a merge needs it). `None` when
/// no pair is mergeable.
fn merge_oldest(
    facts: &[FactRecord],
    store: &TreeStore,
    tiles: &[Tile],
) -> Result<Option<(usize, Tile)>, JanitorError> {
    let total = facts.len() as u64;
    for index in 0..tiles.len().saturating_sub(1) {
        if let (Tile::Block { slot: a, .. }, Tile::Block { slot: b, .. }) =
            (&tiles[index], &tiles[index + 1])
            && a.size == b.size
            && a.lo + a.size == b.lo
            && a.lo.is_multiple_of(2 * a.size)
        {
            let parent = BlockSlot {
                size: 2 * a.size,
                lo: a.lo,
            };
            // The bias rule is a layout contract, not a tiling-time detail:
            // the parent must satisfy `size <= alpha * age` exactly as the
            // initial tiling does (found by the KP-6 property test —
            // n=17, budget=23 merged [0,8)+[8,16) into [0,16), age 1,
            // 16 > alpha*1). A parent that would violate it is skipped;
            // the over-budget path then drops from the oldest end instead.
            let age = total - (parent.lo + parent.size);
            if parent.size > PRESENT_BIAS_ALPHA * age {
                continue;
            }
            let text = read_or_gate(facts, store, &parent)?;
            return Ok(Some((index, Tile::Block { slot: parent, text })));
        }
    }
    Ok(None)
}

/// The split candidate: the rightmost tile among the largest blocks.
/// Rightmost-of-largest keeps the payload reading coarse→fine and spends
/// the leftover on the more recent end.
fn pick_split(tiles: &[Tile]) -> Option<usize> {
    let max = tiles
        .iter()
        .filter_map(|tile| match tile {
            Tile::Block { slot, .. } => Some(slot.size),
            Tile::Fact { .. } => None,
        })
        .max()?;
    tiles
        .iter()
        .rposition(|tile| matches!(tile, Tile::Block { slot, .. } if slot.size == max))
}

/// The two children of a block tile: raw facts for a size-2 block, the
/// two child summaries otherwise (present for settled blocks by the
/// store's children-before-parents rule). An empty result means "not
/// splittable" (a rebuilt tree lost the children); splitting is optional
/// refinement, so the caller simply stops — the gate never fires here.
fn split_children(
    facts: &[FactRecord],
    store: &TreeStore,
    slot: &BlockSlot,
) -> Result<Vec<Tile>, JanitorError> {
    if slot.size == 2 {
        return Ok(slot
            .children()
            .iter()
            .map(|child| Tile::Fact {
                id: child.lo,
                locked: facts[child.lo as usize].is_locked(),
            })
            .collect());
    }
    let mut out = Vec::with_capacity(2);
    for child in slot.children() {
        let Some(text) = store.summary(facts, child.size, child.lo)? else {
            return Ok(Vec::new());
        };
        out.push(Tile::Block { slot: child, text });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::nap;

    fn setup(tmp: &std::path::Path, n: u64, locked: &[u64]) -> (Vec<FactRecord>, TreeStore) {
        let root = tmp.join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts = (0..n)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}: a real decision worth {id} tokens of context"),
                flags: if locked.contains(&id) {
                    vec![MemoryFlag::ArchitecturalLock]
                } else {
                    Vec::new()
                },
            })
            .collect();
        (facts, TreeStore::new(&root))
    }

    /// Settle every pending block the way the protocol says an agent
    /// would (follow `nap`, write one deterministic line per slot).
    fn settle_all(facts: &[FactRecord], store: &TreeStore) {
        for _ in 0..4 * facts.len() as u64 + 64 {
            let plan = nap::next_pending(facts, store).expect("plan");
            let Some(request) = plan.request else { break };
            let text = format!("{}: the durable decisions of the run", request.slot);
            nap::settle(facts, store, &request.slot, &text).expect("settle");
        }
        let done = nap::next_pending(facts, store).expect("plan");
        assert!(!done.pending, "settle_all exhausted the protocol");
    }

    /// Budget 0 is the typed refusal.
    #[test]
    fn zero_budget_is_typed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 2, &[]);
        let err = wake(&facts, &store, 0, Vec::new()).expect_err("zero");
        assert_eq!(err.code(), "E_BUDGET_INVALID");
    }

    /// Everything fits ⇒ nothing is compressed: a generous budget serves
    /// every fact raw, and the gate never fires even with an empty TREE
    /// (§1.5.2's first rule).
    #[test]
    fn everything_fits_serves_everything_raw() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 6, &[2]);
        let report = wake(&facts, &store, 100_000, Vec::new()).expect("wake");
        assert!(!report.truncated);
        assert_eq!(report.items.len(), 6);
        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item, WakeItem::Fact { .. }))
        );
        assert_eq!(report.estimator, "chars4");
        assert!(report.awake);
        let WakeItem::Fact { id: 2, flags, .. } = &report.items[2] else {
            unreachable!("fact item");
        };
        assert_eq!(*flags, vec![MemoryFlag::ArchitecturalLock]);
    }

    /// The gate: compression needed, tiling needs a summary that is not
    /// written ⇒ E_NAP_PENDING carrying the request — and the protocol
    /// loop closes (nap → settle → … → wake succeeds, budget respected).
    #[test]
    fn completeness_gate_refuses_then_settles_then_wakes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 8, &[]);
        let tiny = 12u64;

        let err = wake(&facts, &store, tiny, Vec::new()).expect_err("gate fires");
        assert_eq!(err.code(), "E_NAP_PENDING");
        let details = err.details().expect("request payload");
        // The biased tiling's first block over 8 facts is [0,4) (size 4
        // with age 4 satisfies `4 ≤ α·4`) — that is the needed slot; the
        // actionable request is the protocol's next step, [0,2), because
        // blocks are built bottom-up.
        assert_eq!(details["slot"], "0-4");
        assert_eq!(details["request"]["slot"], "0-2");
        assert!(
            details["request"]["instruction"]
                .as_str()
                .is_some_and(|s| s.contains("nap --slot 0-2"))
        );

        // The agent loop: settle what each refusal's request names until
        // wake answers (every step is actionable by construction).
        let mut guard = 0;
        loop {
            guard += 1;
            assert!(guard < 64, "the loop must converge");
            match wake(&facts, &store, tiny, Vec::new()) {
                Ok(report) => {
                    assert!(report.awake);
                    assert!(report.estimate <= tiny, "budget respected after settling");
                    break;
                }
                Err(err) if err.code() == "E_NAP_PENDING" => {
                    let details = err.details().expect("request");
                    let slot = details["request"]["slot"]
                        .as_str()
                        .or(details["slot"].as_str())
                        .expect("a slot to act on")
                        .to_owned();
                    let text = format!("{slot}: compressed");
                    nap::settle(&facts, &store, &slot, &text).expect("settle");
                }
                Err(other) => unreachable!("unexpected {other:?}"),
            }
        }
    }

    /// A budget that fits the full biased layout: the oldest material is
    /// a coarsened summary, the newest fact stays raw, and the locked
    /// fact stays raw both here and under starvation below.
    #[test]
    fn biased_layout_coarsens_the_old_and_keeps_the_new_raw() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 16, &[9]);
        settle_all(&facts, &store);

        // The whole biased layout (one summary over [0,8) plus the tail)
        // costs ~84 tokens; a budget just above keeps it intact.
        let roomy = wake(&facts, &store, 85, Vec::new()).expect("wake");
        assert!(roomy.estimate <= 85, "estimate {}", roomy.estimate);
        assert!(
            matches!(roomy.items.first(), Some(WakeItem::Summary { slot, .. }) if slot == "0-8"),
            "the oldest material is the [0,8) summary: {:?}",
            roomy.items.first()
        );
        assert!(
            matches!(roomy.items.last(), Some(WakeItem::Fact { id: 15, .. })),
            "the newest fact stays raw: {:?}",
            roomy.items.last()
        );

        // Starvation: everything droppable goes, pinned locks survive.
        let starved = wake(&facts, &store, 40, Vec::new()).expect("wake");
        assert!(starved.truncated);
        assert!(starved.estimate <= 40, "estimate {}", starved.estimate);
        assert!(
            matches!(starved.items.last(), Some(WakeItem::Fact { id: 15, .. })),
            "the newest fact outlives the drops: {:?}",
            starved.items.last()
        );

        for report in [&roomy, &starved] {
            let lock_flags = report
                .items
                .iter()
                .find_map(|item| match item {
                    WakeItem::Fact { id: 9, flags, .. } => Some(flags.clone()),
                    _ => None,
                })
                .expect("the lock is present");
            assert_eq!(lock_flags, vec![MemoryFlag::ArchitecturalLock]);
            for item in &report.items {
                if let WakeItem::Summary { covers, .. } = item {
                    assert!(
                        !(covers.0..covers.1).contains(&9),
                        "no summary may cover the locked fact: {covers:?}"
                    );
                }
            }
        }
    }

    /// Deterministic layout: identical inputs, identical payloads.
    #[test]
    fn wake_is_deterministic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 12, &[5]);
        settle_all(&facts, &store);
        let a = wake(&facts, &store, 30, Vec::new()).expect("wake a");
        let b = wake(&facts, &store, 30, Vec::new()).expect("wake b");
        assert_eq!(a, b);
    }

    /// The bias invariant itself: every served summary satisfies
    /// `size ≤ PRESENT_BIAS_ALPHA × age`, blocks are aligned, and items are
    /// oldest-first with disjoint coverage.
    #[test]
    fn summaries_satisfy_the_bias_and_cover_disjointly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 20, &[3]);
        settle_all(&facts, &store);
        let report = wake(&facts, &store, 60, Vec::new()).expect("wake");
        let n = facts.len() as u64;
        let mut prev_end = None;
        for item in &report.items {
            match item {
                WakeItem::Fact { id, .. } => {
                    if let Some(end) = prev_end {
                        assert!(*id >= end, "items are oldest-first, no overlap");
                    }
                    prev_end = Some(id + 1);
                }
                WakeItem::Summary { covers, .. } => {
                    let (lo, hi) = *covers;
                    let size = hi - lo;
                    assert!(lo.is_multiple_of(size), "aligned block");
                    let age = n - hi;
                    assert!(
                        size <= PRESENT_BIAS_ALPHA * age,
                        "bias violated for [{lo},{hi}) age {age}"
                    );
                    if let Some(end) = prev_end {
                        assert!(lo >= end, "items are oldest-first, no overlap");
                    }
                    prev_end = Some(hi);
                }
            }
        }
    }

    /// The merge and split passes shape a mid-size budget: fewer, larger
    /// blocks than the pure tiling when tight; more detail once the
    /// budget loosens (same log, monotone in budget).
    #[test]
    fn budget_shaping_is_monotone_in_the_budget() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 16, &[]);
        settle_all(&facts, &store);

        let tight = wake(&facts, &store, 20, Vec::new()).expect("tight");
        let loose = wake(&facts, &store, 300, Vec::new()).expect("loose");
        let raw = wake(&facts, &store, 100_000, Vec::new()).expect("raw fits");

        assert!(tight.estimate <= 20 && loose.estimate <= 300);
        assert_eq!(raw.items.len(), 16, "loosest serves everything raw");
        assert!(
            loose.items.len() >= tight.items.len(),
            "a bigger budget never carries less material"
        );
        assert!(
            loose.estimate >= tight.estimate,
            "a bigger budget never spends less"
        );
    }

    // --- property suite (brief F6.8: tiling math) ------------------------------

    use proptest::prelude::*;

    /// The tiling contract, over randomized logs and budgets: the budget
    /// is respected (or honestly truncated to pinned-only), locked facts
    /// are always present and always raw, no summary ever covers a lock,
    /// every summary satisfies the bias rule, and coverage is disjoint
    /// and oldest-first. The tree is fully settled first, so the
    /// completeness gate never fires — these properties are about the
    /// layout, not the protocol.
    #[test]
    fn tiling_properties_hold_over_random_logs_and_budgets() {
        let config = ProptestConfig::with_cases(24);
        proptest!(config, |(n in 2u64..24, lock_bits in prop::collection::vec(any::<bool>(), 0..24), budget in 1u64..4000)| {
            let locked: Vec<u64> = (0..n)
                .filter(|id| lock_bits.get(*id as usize).copied().unwrap_or(false))
                .collect();
            let tmp = tempfile::tempdir().expect("tempdir");
            let (facts, store) = setup(tmp.path(), n, &locked);
            settle_all(&facts, &store);

            let report = wake(&facts, &store, budget, Vec::new()).expect("settled tree wakes");

            // Budget respected — or honestly truncated to pinned-only.
            let only_pinned = report
                .items
                .iter()
                .all(|item| matches!(item, WakeItem::Fact { flags, .. } if flags.contains(&MemoryFlag::ArchitecturalLock)));
            prop_assert!(report.estimate <= budget || (report.truncated && only_pinned),
                "estimate {} budget {} truncated {} only_pinned {}",
                report.estimate, budget, report.truncated, only_pinned);

            // Locks: always present, exactly once, always raw.
            for id in &locked {
                let hits = report
                    .items
                    .iter()
                    .filter(|item| matches!(item, WakeItem::Fact { id: fact, flags, .. } if fact == id
                        && flags.contains(&MemoryFlag::ArchitecturalLock)))
                    .count();
                prop_assert_eq!(hits, 1, "lock {} present exactly once, raw", id);
            }

            // No summary covers a lock; bias holds; coverage disjoint and ascending.
            let total = n;
            let mut prev_end: Option<u64> = None;
            for item in &report.items {
                match item {
                    WakeItem::Fact { id, .. } => {
                        if let Some(end) = prev_end {
                            prop_assert!(*id >= end);
                        }
                        prev_end = Some(*id + 1);
                    }
                    WakeItem::Summary { covers, .. } => {
                        let (lo, hi) = *covers;
                        for id in &locked {
                            prop_assert!(!(lo..hi).contains(id), "summary [{lo},{hi}) covers lock {id}");
                        }
                        let size = hi - lo;
                        prop_assert!(lo.is_multiple_of(size));
                        prop_assert!(size <= PRESENT_BIAS_ALPHA * (total - hi), "bias for [{lo},{hi})");
                        if let Some(end) = prev_end {
                            prop_assert!(lo >= end);
                        }
                        prev_end = Some(hi);
                    }
                }
            }
        });
    }
}
