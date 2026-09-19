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
//!    truncation, always surfaced raw at their position (§1.5.1). One C4
//!    exception: a bias-demoted block covering an agent-tagged `low`
//!    fact stops one split level coarser — the cheaper seat (see
//!    [`tile`]);
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
//!
//! **The ranked pass (C1, PLAN-C2C §2).** `--task "<query>"` switches
//! recall from position to relevance while keeping every rule above:
//!
//! 1. **score** — every fact by store-local TF-IDF
//!    ([`crate::memory::rank`], declared `"scorer": "tfidf-store"`);
//! 2. **serve** — score-positive facts get raw seats in serve order
//!    (score desc, ties to the higher confidence — a `low` tag demotes
//!    only — then to the newer id) while their cost fits; a fact that
//!    does not fit is dropped from raw service and counted
//!    (`dropped_scored`). C4: seating runs in two tiers, normal
//!    confidence first and `low` with the leftover — under budget
//!    pressure the low facts are the first dropped, after the score
//!    ordering. Locked facts stay **pinned** — always raw, never
//!    dropped, honestly over budget if it comes to that (the §1.5.2
//!    invariant survives ranking);
//! 3. **fill** — the leftover budget buys stored summaries over the
//!    unserved ranges through the same biased tiling, with served facts
//!    pinned exactly like locks (blocks force-split around them) and the
//!    `E_NAP_PENDING` gate untouched;
//! 4. **report** — an append-only `task` block in the payload, and
//!    `truncated: true` now means "score-positive facts were dropped
//!    for budget". Without `--task` the payload is byte-identical to
//!    the positional pass (golden regression).
//! 5. **ceiling (C1-R)** — the ranked pass runs only **below**
//!    [`TASK_BUDGET_CEILING`]. The real-store KP probe (2026-09-19,
//!    448 facts, 10 hand-labeled queries) measured the ranked pass
//!    winning 7–8/10 at budget ≤ 300 and 0/10 at budget ≥ 400: at high
//!    budget the positional layout already covers ~100% of the facts
//!    through summaries — coverage is not ranking, and ordering has
//!    nothing left to buy. At or above the ceiling the wake serves the
//!    positional payload and says so in the `task` block
//!    (`"degraded": "budget_at_or_above_ceiling"`, nothing scored)
//!    instead of pretending to rank.

use serde::Serialize;

use crate::error::{JanitorError, MemoryError};
use crate::memory::log::{Confidence, FactRecord, LogWarning, MemoryFlag};
use crate::memory::nap;
use crate::memory::pointer::SyntaxPointer;
use crate::memory::rank;
use crate::memory::tree_store::{BlockPlan, BlockSlot, LevelSnapshot, TreeStore};

/// The default budget (`--budget` overrides): OptMem budgets ~96 lines ≈
/// 8k tokens; we budget tokens directly (§1.5.2).
pub const DEFAULT_BUDGET: u64 = 8192;

/// Present-bias constant: a block stays whole only if
/// `size ≤ PRESENT_BIAS_ALPHA × age`, where age is the count of newer facts.
pub const PRESENT_BIAS_ALPHA: u64 = 8;

/// The task-budget ceiling (C1-R): `--task` runs the ranked pass only
/// **below** this budget; at or above it the wake degrades honestly to
/// the positional pass (see [`wake_degraded`]). The KP probe on the real
/// 448-fact store (2026-09-19, 10 hand-labeled queries, ranked vs
/// positional at equal budget) measured the ranked pass winning 7–8/10
/// at budget ≤ 300 and 0/10 at budget ≥ 400 — above the crossover the
/// positional layout already covers ~100% of the facts via summaries, so
/// relevance ordering has nothing left to buy. The crossover sits
/// between 300 and 400; the owner set the ceiling at 350.
pub const TASK_BUDGET_CEILING: u64 = 350;

/// The `task.degraded` reason when a `--task` wake served positionally
/// because the budget reached [`TASK_BUDGET_CEILING`] (append-only key:
/// a ranked wake carries no `degraded` key at all).
pub const TASK_DEGRADED_AT_CEILING: &str = "budget_at_or_above_ceiling";

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
        /// The agent-asserted confidence tag (C4) — an append-only key,
        /// present only on tagged facts.
        #[serde(skip_serializing_if = "Option::is_none")]
        confidence: Option<Confidence>,
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
    /// True when tiles were dropped from the oldest end to fit the budget
    /// (under `--task`: when score-positive facts were dropped for
    /// budget — the fill's shaping of score-zero material is not a
    /// truncation the agent needs to act on).
    pub truncated: bool,
    /// The ranked pass's declaration — present only under `--task`
    /// (append-only: the positional payload never carries the key).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<TaskReport>,
    pub items: Vec<WakeItem>,
    pub warnings: Vec<LogWarning>,
}

/// The `task` block of a ranked wake payload: what was asked, what scored
/// it, and the serve accounting.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct TaskReport {
    /// The query exactly as given.
    pub query: String,
    /// The scorer identity (the `"estimator": "chars4"` precedent,
    /// ADR-0011 — the limitation is auditable in-band).
    pub scorer: &'static str,
    /// Score-positive facts served raw.
    pub served_scored: u64,
    /// Score-positive facts not served raw (budget); a summary may still
    /// cover them — compressed beats absent.
    pub dropped_scored: u64,
    /// Why this `--task` wake did not run the ranked pass (C1-R,
    /// append-only): [`TASK_DEGRADED_AT_CEILING`] when the budget reached
    /// [`TASK_BUDGET_CEILING`] — the payload is the positional pass's,
    /// nothing was scored, and the block says so instead of pretending
    /// to rank. A ranked wake carries no `degraded` key at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded: Option<&'static str>,
}

/// One tile of the layout during construction.
#[derive(Clone, Debug)]
enum Tile {
    /// A raw fact. `pinned` marks the facts the layout may neither
    /// compress nor drop: locked facts always, and — under `--task` —
    /// the facts the ranked serve pass bought raw.
    Fact {
        id: u64,
        pinned: bool,
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
/// warnings collected at log load ride along unchanged. `task`, when
/// given, switches recall to the ranked pass below [`TASK_BUDGET_CEILING`]
/// and to the honest positional degradation at or above it (see the
/// module doc); `None` is the positional pass, byte-identical to pre-C1
/// payloads.
///
/// The whole run reads each TREE level file exactly once (the C3 batch
/// load): `store.load_levels` pays the I/O up front and every
/// summary/rank lookup below — tiling, merging, splitting, the gate's
/// pending request — resolves in memory.
pub fn wake(
    facts: &[FactRecord],
    store: &TreeStore,
    task: Option<&str>,
    budget: u64,
    warnings: Vec<LogWarning>,
) -> Result<WakeReport, JanitorError> {
    if budget == 0 {
        return Err(MemoryError::BudgetInvalid {
            detail: "budget must be at least 1 token".to_owned(),
        }
        .into());
    }
    match task {
        // At or above the ceiling ranking buys nothing (the KP probe's
        // 0/10 at b ≥ 400): serve positional, report the degradation.
        Some(query) if budget >= TASK_BUDGET_CEILING => {
            wake_degraded(facts, store, query, budget, warnings)
        }
        Some(query) => wake_ranked(facts, store, query, budget, warnings),
        None => wake_positional(facts, store, budget, warnings),
    }
}

/// The positional pass — steps 1–4 of the module doc, unchanged by C1
/// (the golden fixtures pin it byte for byte).
fn wake_positional(
    facts: &[FactRecord],
    store: &TreeStore,
    budget: u64,
    warnings: Vec<LogWarning>,
) -> Result<WakeReport, JanitorError> {
    // 1. Everything fits raw ⇒ nothing is compressed, TREE untouched.
    let raw_total: u64 = facts.iter().map(|fact| tokens(fact.text.len())).sum();
    if raw_total <= budget {
        return Ok(WakeReport {
            awake: true,
            budget_tokens: budget,
            estimator: ESTIMATOR,
            estimate: raw_total,
            truncated: false,
            task: None,
            items: facts.iter().map(raw_item).collect(),
            warnings,
        });
    }

    // 2. Biased tiling — every block it names is validated (the gate).
    //    One batch load pays for every lookup the passes below make.
    let levels = store.load_levels(facts)?;
    let pinned = lock_mask(facts);
    let tiles = tile(facts, &levels, &pinned)?;

    // 3+4. Merge, drop, split — the same shaping the ranked pass fills
    // with. Here, tiles dropped from the oldest end are the reported
    // truncation.
    let (tiles, dropped) = shape_to_budget(facts, &levels, &pinned, tiles, budget)?;

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
        truncated: dropped,
        task: None,
        items,
        warnings,
    })
}

/// The honest degradation at or above [`TASK_BUDGET_CEILING`] (C1-R):
/// the ranked pass does not run — [`wake_positional`] serves the payload
/// (items, estimate, `truncated` all positional), and the `task` block
/// reports the state in-band instead of pretending to rank:
/// `"degraded": "budget_at_or_above_ceiling"` ([`TASK_DEGRADED_AT_CEILING`]),
/// `served_scored`/`dropped_scored` 0 (nothing was scored), `query` and
/// `scorer` still declared — the `estimator` precedent: the limitation
/// is auditable in the payload, not discovered by surprise.
fn wake_degraded(
    facts: &[FactRecord],
    store: &TreeStore,
    query: &str,
    budget: u64,
    warnings: Vec<LogWarning>,
) -> Result<WakeReport, JanitorError> {
    let mut report = wake_positional(facts, store, budget, warnings)?;
    report.task = Some(TaskReport {
        query: query.to_owned(),
        scorer: rank::SCORER,
        served_scored: 0,
        dropped_scored: 0,
        degraded: Some(TASK_DEGRADED_AT_CEILING),
    });
    Ok(report)
}

/// The ranked pass (`--task`): score → serve → fill → report ([`rank`]
/// owns the arithmetic, the module doc the contract). Runs only below
/// [`TASK_BUDGET_CEILING`] — [`wake`] routes at-or-above budgets to
/// [`wake_degraded`].
fn wake_ranked(
    facts: &[FactRecord],
    store: &TreeStore,
    query: &str,
    budget: u64,
    warnings: Vec<LogWarning>,
) -> Result<WakeReport, JanitorError> {
    // Score: one tokenization per fact feeds both the document
    // statistics and the scoring — O(total bytes), the fence the C1
    // timing test pins.
    let tokenized: Vec<Vec<String>> = facts
        .iter()
        .map(|fact| rank::tokenize(&fact.text))
        .collect();
    let scored = rank::Query::over_token_lists(query, &tokenized);
    let scores: Vec<f64> = tokenized
        .iter()
        .map(|tokens| scored.score_tokens(tokens))
        .collect();
    let order = serve_order(facts, &scores);

    // 1. Everything fits raw ⇒ nothing is compressed, TREE untouched —
    //    the same first rule as the positional pass (an unsettled
    //    summary can never block a wake that needs none). Serve order
    //    still leads: the raw evidence, best first.
    let raw_total: u64 = facts.iter().map(|fact| tokens(fact.text.len())).sum();
    if raw_total <= budget {
        return Ok(WakeReport {
            awake: true,
            budget_tokens: budget,
            estimator: ESTIMATOR,
            estimate: raw_total,
            truncated: false,
            task: Some(task_report(query, positive_count(&scores), 0)),
            items: order
                .iter()
                .map(|id| raw_item(&facts[*id as usize]))
                .collect(),
            warnings,
        });
    }

    // 2. Serve: score-positive facts take raw seats in serve order while
    //    their cost fits; a fact that does not fit is dropped from raw
    //    service and counted. Locked facts are pre-seated (their cost
    //    spent up front) and stay pinned — never dropped, honestly over
    //    budget if it comes to that. C4 seats in two tiers, normal
    //    confidence first (then `low`): under budget pressure the low
    //    facts are the first dropped — after the score ordering, which
    //    still governs within each tier.
    let mut served = lock_mask(facts);
    let mut spent: u64 = facts
        .iter()
        .filter(|fact| fact.is_locked())
        .map(|fact| tokens(fact.text.len()))
        .sum();
    let mut served_scored = 0u64;
    for low_tier in [false, true] {
        for &id in &order {
            if scores[id as usize] <= 0.0 {
                break; // serve order is score desc: the rest cannot be positive
            }
            if facts[id as usize].is_low_confidence() != low_tier {
                continue; // the other tier's fact seats in its own pass
            }
            if served[id as usize] {
                served_scored += 1; // a pinned lock with a positive score: seated
                continue;
            }
            let cost = tokens(facts[id as usize].text.len());
            if spent + cost <= budget {
                served[id as usize] = true;
                spent += cost;
                served_scored += 1;
            }
        }
    }
    let dropped_scored = positive_count(&scores) - served_scored;

    // 3. Fill: the same biased tiling over everything the serve pass did
    //    not buy raw, with served facts pinned exactly like locks (blocks
    //    force-split around them) — the completeness gate untouched.
    let levels = store.load_levels(facts)?;
    let tiles = tile(facts, &levels, &served)?;
    let (mut tiles, _fill_dropped) = shape_to_budget(facts, &levels, &served, tiles, budget)?;

    // A score-positive fact the budget dropped from raw service must not
    // sneak back as a raw tail tile or a split product: it was counted
    // dropped. A stored summary may still cover it — compressed beats
    // absent. (Score-zero facts the bias keeps raw at the tail ride
    // along: the layout covers every fact, no silent holes.)
    tiles.retain(|tile| match tile {
        Tile::Fact { id, .. } => served[*id as usize] || scores[*id as usize] <= 0.0,
        Tile::Block { .. } => true,
    });

    // 4. Report: the raw evidence leads (serve order), then the layout's
    //    chronological backdrop — the score-zero tail raws the bias kept,
    //    and the summary blocks.
    let mut items: Vec<WakeItem> = order
        .iter()
        .filter(|id| served[**id as usize])
        .map(|id| raw_item(&facts[*id as usize]))
        .collect();
    for tile in &tiles {
        match tile {
            Tile::Fact { id, .. } if !served[*id as usize] => {
                items.push(raw_item(&facts[*id as usize]));
            }
            Tile::Block { slot, text } => items.push(WakeItem::Summary {
                slot: slot.notation(),
                covers: (slot.lo, slot.lo + slot.size),
                text: text.clone(),
            }),
            Tile::Fact { .. } => {} // already rendered in serve order
        }
    }
    let estimate: u64 = items
        .iter()
        .map(|item| match item {
            WakeItem::Fact { text, .. } | WakeItem::Summary { text, .. } => tokens(text.len()),
        })
        .sum();
    Ok(WakeReport {
        awake: true,
        budget_tokens: budget,
        estimator: ESTIMATOR,
        estimate,
        truncated: dropped_scored > 0,
        task: Some(task_report(query, served_scored, dropped_scored)),
        items,
        warnings,
    })
}

/// The pin mask of the positional pass: locked facts, and nothing else.
fn lock_mask(facts: &[FactRecord]) -> Vec<bool> {
    facts.iter().map(|fact| fact.is_locked()).collect()
}

/// Fact ids in serve order: score desc, ties to the higher confidence
/// (a `low` tag demotes — C4), then to the newer id
/// ([`rank::scored_first`]).
fn serve_order(facts: &[FactRecord], scores: &[f64]) -> Vec<u64> {
    let mut ranked: Vec<rank::Scored> = scores
        .iter()
        .enumerate()
        .map(|(id, score)| rank::Scored {
            id: id as u64,
            score: *score,
            low: facts[id].is_low_confidence(),
        })
        .collect();
    ranked.sort_by(rank::scored_first);
    ranked.into_iter().map(|entry| entry.id).collect()
}

/// How many facts carry a strictly positive score.
fn positive_count(scores: &[f64]) -> u64 {
    scores.iter().filter(|&&score| score > 0.0).count() as u64
}

fn task_report(query: &str, served_scored: u64, dropped_scored: u64) -> TaskReport {
    TaskReport {
        query: query.to_owned(),
        scorer: rank::SCORER,
        served_scored,
        dropped_scored,
        degraded: None,
    }
}

/// Steps 3–4 over an initial layout: while over budget, merge the oldest
/// mergeable sibling pair into its parent; still over, drop whole tiles
/// from the oldest end (pinned tiles survive — keep them, honestly
/// over); with leftover budget, split the rightmost of the largest
/// blocks into its halves while affordable. Returns the shaped tiles and
/// whether the drop pass fired.
fn shape_to_budget(
    facts: &[FactRecord],
    levels: &LevelSnapshot,
    pinned: &[bool],
    mut tiles: Vec<Tile>,
    budget: u64,
) -> Result<(Vec<Tile>, bool), JanitorError> {
    // Over budget: merge the oldest mergeable sibling pair into its
    // parent while the payload still does not fit.
    while total_cost(&tiles, facts) > budget {
        match merge_oldest(facts, levels, &tiles)? {
            Some((range, parent)) => {
                tiles.splice(range..range + 2, std::iter::once(parent));
            }
            None => break,
        }
    }

    // Still over budget: drop whole tiles from the oldest end, pinned
    // tiles excepted — the newest material survives longest.
    let mut dropped = false;
    while total_cost(&tiles, facts) > budget {
        let Some(drop_at) = tiles.iter().position(|tile| match tile {
            Tile::Fact { pinned, .. } => !*pinned,
            Tile::Block { .. } => true,
        }) else {
            break; // only pinned tiles remain: keep them, honestly over
        };
        tiles.remove(drop_at);
        dropped = true;
    }

    // Leftover budget: split the rightmost of the largest blocks into
    // its two halves while affordable ("spend what is left on the
    // present"). Splits strictly shrink tile sizes, so this terminates.
    loop {
        let total = total_cost(&tiles, facts);
        let Some(at) = pick_split(&tiles) else { break };
        let Tile::Block { slot, text } = &tiles[at] else {
            unreachable!("pick_split only names blocks");
        };
        let replacement = split_children(pinned, levels, slot)?;
        if replacement.is_empty() {
            break; // children unavailable (rebuilt tree); refinement is optional
        }
        // Saturating: the children may be *cheaper* than the parent's own
        // summary (two short raw facts under one long line), making the
        // delta negative — the split still fits by definition, so clamp at
        // zero and proceed (found by the C3 synthetic corpus; the raw
        // subtraction panicked debug builds on exactly that shape).
        let added = total_cost(&replacement, facts).saturating_sub(tokens(text.len()));
        if total + added > budget {
            break;
        }
        tiles.splice(at..at + 1, replacement.into_iter().collect::<Vec<_>>());
    }

    Ok((tiles, dropped))
}

/// The raw-fact payload item for one record.
fn raw_item(fact: &FactRecord) -> WakeItem {
    WakeItem::Fact {
        id: fact.id,
        text: fact.text.clone(),
        ptr: fact.ptr.clone(),
        flags: fact.flags.clone(),
        confidence: fact.confidence,
    }
}

fn total_cost(tiles: &[Tile], facts: &[FactRecord]) -> u64 {
    tiles.iter().map(|tile| tile.cost(facts)).sum()
}

/// The biased tiling of `[0, n)` (step 2): greedy from the oldest end; at
/// each position the largest aligned power-of-two block that fits the
/// remaining range, demoted past unpinnable ranges, then demoted until
/// the bias rule (`size ≤ α × age`) holds or the tile is a single raw
/// fact. `pinned` marks the facts no block may swallow — locked facts
/// always (the positional pass), plus the facts the ranked serve pass
/// bought raw (they must surface raw, exactly like locks).
///
/// C4 clemency: a block the bias *demoted* stops one split level coarser
/// when that coarser block covers an agent-tagged `low` fact — the
/// cheaper seat for demoted material (the one documented exception to the
/// bias rule). Clemency is strictly a downgrade and never widens the
/// completeness gate: it fires only when the coarser slot is itself a
/// legal block (compressible, unpinned) whose summary is already
/// settled — the split pass's "refinement never trips the gate" rule,
/// mirrored. A store with no tagged facts tiles exactly as before.
fn tile(
    facts: &[FactRecord],
    levels: &LevelSnapshot,
    pinned: &[bool],
) -> Result<Vec<Tile>, JanitorError> {
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
        // Store compressibility (locks) and the pin mask first, then
        // present-bias — demotion only shrinks, and every invariant is
        // preserved by subranges of a valid block.
        while size > 1 && (!plan.is_compressible(facts, size, p) || range_pinned(pinned, p, size)) {
            size /= 2;
        }
        let capped = size; // the bias loop below only demotes from here
        while size > 1 && size > PRESENT_BIAS_ALPHA * (n - (p + size)) {
            size /= 2;
        }
        if size < capped
            && range_low(facts, p, size * 2)
            && plan.is_compressible(facts, size * 2, p)
            && !range_pinned(pinned, p, size * 2)
            && matches!(levels.summary(size * 2, p), Ok(Some(_)))
        {
            size *= 2;
        }
        if size == 1 {
            tiles.push(Tile::Fact {
                id: p,
                pinned: pinned[p as usize],
            });
        } else {
            let slot = BlockSlot { size, lo: p };
            let text = read_or_gate(facts, levels, &slot)?;
            tiles.push(Tile::Block { slot, text });
        }
        p += size;
    }
    Ok(tiles)
}

/// Whether any fact in `[lo, lo + size)` is agent-tagged low confidence.
fn range_low(facts: &[FactRecord], lo: u64, size: u64) -> bool {
    facts[lo as usize..(lo + size) as usize]
        .iter()
        .any(|fact| fact.is_low_confidence())
}

/// Whether any fact in `[lo, lo + size)` carries the pin mask.
fn range_pinned(pinned: &[bool], lo: u64, size: u64) -> bool {
    pinned[lo as usize..(lo + size) as usize]
        .iter()
        .any(|&pin| pin)
}

/// A block the layout needs: its stored summary, or — the completeness
/// gate (§1.5.2) — the [`MemoryError::NapPending`] refusal carrying the
/// compression request, never a silently served hole.
fn read_or_gate(
    facts: &[FactRecord],
    levels: &LevelSnapshot,
    slot: &BlockSlot,
) -> Result<String, JanitorError> {
    match levels.summary(slot.size, slot.lo)? {
        Some(text) => Ok(text),
        None => Err(pending_error(facts, levels, slot)),
    }
}

/// Build the E_NAP_PENDING refusal. `slot` names the block the layout
/// *needs*; the `request` payload carries the protocol's next **actionable**
/// step (`nap`'s canonical next-pending — dense prefix and
/// children-before-parents respected), which may be a descendant of
/// `slot` when the tree has not been built that deep yet. An agent that
/// follows `request` slot by slot converges: every settle is permanent
/// and the tree is finite. The request is planned over the same batch
/// load the layout used — the refusal costs no extra reads either.
fn pending_error(facts: &[FactRecord], levels: &LevelSnapshot, slot: &BlockSlot) -> JanitorError {
    let request = nap::next_pending_over(facts, levels)
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
    levels: &LevelSnapshot,
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
            let text = read_or_gate(facts, levels, &parent)?;
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
    pinned: &[bool],
    levels: &LevelSnapshot,
    slot: &BlockSlot,
) -> Result<Vec<Tile>, JanitorError> {
    if slot.size == 2 {
        return Ok(slot
            .children()
            .iter()
            .map(|child| Tile::Fact {
                id: child.lo,
                pinned: pinned[child.lo as usize],
            })
            .collect());
    }
    let mut out = Vec::with_capacity(2);
    for child in slot.children() {
        let Some(text) = levels.summary(child.size, child.lo)? else {
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
                confidence: None,
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
        let err = wake(&facts, &store, None, 0, Vec::new()).expect_err("zero");
        assert_eq!(err.code(), "E_BUDGET_INVALID");
    }

    /// Everything fits ⇒ nothing is compressed: a generous budget serves
    /// every fact raw, and the gate never fires even with an empty TREE
    /// (§1.5.2's first rule).
    #[test]
    fn everything_fits_serves_everything_raw() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 6, &[2]);
        let report = wake(&facts, &store, None, 100_000, Vec::new()).expect("wake");
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

        let err = wake(&facts, &store, None, tiny, Vec::new()).expect_err("gate fires");
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
            match wake(&facts, &store, None, tiny, Vec::new()) {
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
        let roomy = wake(&facts, &store, None, 85, Vec::new()).expect("wake");
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
        let starved = wake(&facts, &store, None, 40, Vec::new()).expect("wake");
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
        let a = wake(&facts, &store, None, 30, Vec::new()).expect("wake a");
        let b = wake(&facts, &store, None, 30, Vec::new()).expect("wake b");
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
        let report = wake(&facts, &store, None, 60, Vec::new()).expect("wake");
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

        let tight = wake(&facts, &store, None, 20, Vec::new()).expect("tight");
        let loose = wake(&facts, &store, None, 300, Vec::new()).expect("loose");
        let raw = wake(&facts, &store, None, 100_000, Vec::new()).expect("raw fits");

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

    /// The C3 synthetic corpus found the split pass's cost delta
    /// underflowing: a block whose two children are *cheaper* than its own
    /// stored summary (two very short facts under one standard-length
    /// line) made `children - parent` negative and panicked debug builds.
    /// The shape needs a long-fact head (raw total over budget, so the
    /// tiling and split passes run) and a short-fact tail (pairs cheaper
    /// than a summary): uniform lengths never reach a child-cheaper
    /// size-2 split before the everything-fits return.
    #[test]
    fn split_survives_children_cheaper_than_the_parent_summary() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts: Vec<FactRecord> = (0..64)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: if id < 32 {
                    format!("fact {id}: the overlay invariants hold and every long head fact costs a real slice of the token budget")
                } else {
                    format!("f{id}")
                },
                flags: Vec::new(),
                confidence: None,
            })
            .collect();
        let store = TreeStore::new(&root);
        settle_all(&facts, &store);

        let report = wake(&facts, &store, None, 800, Vec::new()).expect("wake");
        assert!(report.estimate <= 800, "estimate {}", report.estimate);
    }

    // --- C3: the read-path batch load -----------------------------------------

    /// One wake reads each layout level exactly once — the batch load
    /// replaces the per-lookup full-file read. This is the in-process
    /// twin of the audit's strace count (713 TREE opens for one 5k-fact
    /// wake on the lane's synthetic generator, pre-C3).
    #[test]
    fn one_wake_reads_each_present_level_exactly_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 300, &[]);
        settle_all(&facts, &store);

        let layout = BlockPlan::new(&facts).levels().len(); // 2..256: eight
        let on_disk = std::fs::read_dir(store.dir()).expect("TREE dir").count();
        assert_eq!(on_disk, layout, "settled: one file per level");
        assert!(layout <= 300u64.next_power_of_two().trailing_zeros() as usize);

        let before = crate::memory::tree_store::LEVEL_READS.with(|reads| reads.get());
        let report = wake(&facts, &store, None, 1024, Vec::new()).expect("wake");
        let reads = crate::memory::tree_store::LEVEL_READS.with(|reads| reads.get()) - before;
        assert_eq!(reads, layout, "one read per level — no per-lookup reloads");
        assert!(!report.items.is_empty());
    }

    /// The gate refusal (unsettled tree) reads each level once too — the
    /// E_NAP_PENDING request is planned over the same batch load, so the
    /// refusal costs no second pass over the level files.
    #[test]
    fn gate_refusal_reuses_the_batch_load() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 300, &[]); // nothing settled

        let before = crate::memory::tree_store::LEVEL_READS.with(|reads| reads.get());
        let err = wake(&facts, &store, None, 1024, Vec::new()).expect_err("gate fires");
        let reads = crate::memory::tree_store::LEVEL_READS.with(|reads| reads.get()) - before;
        assert_eq!(err.code(), "E_NAP_PENDING");
        assert_eq!(
            reads,
            BlockPlan::new(&facts).levels().len(),
            "the refusal plans over the snapshot it already loaded"
        );
    }

    // --- C1: the ranked pass ---------------------------------------------------

    /// The retrieval fixture at unit scale, C1-R-sized: the texts are
    /// calibrated for [`TASK_BUDGET_CEILING`] — the ten cluster facts fit
    /// raw inside budget 300 (below the ceiling, where the ranked pass
    /// runs) while the 40-fact raw total stays well above it, so the
    /// positional pass must compress. The `cluster` ids (callers put
    /// them in the oldest half) carry the "daemon socket errors"
    /// vocabulary and everything else is filler sharing none of it —
    /// the query terms are discriminative by construction, and the
    /// zero-padded id words keep no two texts tokenizing identically.
    fn cluster_setup(
        tmp: &std::path::Path,
        n: u64,
        cluster: &[u64],
        locked: &[u64],
    ) -> (Vec<FactRecord>, TreeStore) {
        let root = tmp.join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let filler = "overlay txn flush atomic rename fsync budget tiling summary lock gate \
                      nap wake log tree cache rank token bytes";
        let facts = (0..n)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-18T00:00:00Z".to_owned(),
                ptr: None,
                text: if cluster.contains(&id) {
                    format!("fact {id:02}: daemon socket errors under load, backoff retry {id:02}")
                } else {
                    format!("fact {id:02}: {filler} — note {id:02}")
                },
                flags: if locked.contains(&id) {
                    vec![MemoryFlag::ArchitecturalLock]
                } else {
                    Vec::new()
                },
                confidence: None,
            })
            .collect();
        (facts, TreeStore::new(&root))
    }

    /// The ids arriving as raw fact items.
    fn raw_ids(report: &WakeReport) -> Vec<u64> {
        report
            .items
            .iter()
            .filter_map(|item| match item {
                WakeItem::Fact { id, .. } => Some(*id),
                _ => None,
            })
            .collect()
    }

    /// The retrieval claim (C1 acceptance, unit scale): at equal budget
    /// the ranked pass serves the cluster raw while the positional pass
    /// buys summaries of the old blocks — and says so in-band. C1-R: the
    /// budget must sit below [`TASK_BUDGET_CEILING`], where the ranked
    /// pass runs — this is also the ceiling's low-budget teeth (a
    /// ceiling that blocks tight budgets fails here).
    #[test]
    fn ranked_pass_serves_the_cluster_that_positional_compresses() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect(); // 0..18: oldest half
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[]);
        settle_all(&facts, &store);
        let budget = 300u64; // < TASK_BUDGET_CEILING: the ranked pass is live

        let ranked = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            budget,
            Vec::new(),
        )
        .expect("ranked");
        let positional = wake(&facts, &store, None, budget, Vec::new()).expect("positional");

        let served = cluster
            .iter()
            .filter(|id| raw_ids(&ranked).contains(id))
            .count();
        let bought = cluster
            .iter()
            .filter(|id| raw_ids(&positional).contains(id))
            .count();
        assert!(served >= 9, "ranked served {served}/10 of the cluster");
        assert!(bought <= 3, "positional served {bought}/10 of the cluster");

        // Only cluster facts are score-positive here, so the counts say
        // exactly what the items say.
        let task = ranked.task.as_ref().expect("task block");
        assert_eq!(task.query, "daemon socket errors");
        assert_eq!(task.scorer, "tfidf-store");
        assert_eq!(task.served_scored, served as u64);
        assert_eq!(task.dropped_scored, 10 - served as u64);
        assert_eq!(ranked.truncated, task.dropped_scored > 0);

        // Append-only: the positional payload carries no task key at all.
        assert!(positional.task.is_none());
        let json = serde_json::to_value(&positional).expect("positional json");
        assert!(json.get("task").is_none(), "the key must not appear");
    }

    /// Deterministic: identical inputs, byte-identical ranked payloads.
    #[test]
    fn ranked_wake_is_deterministic() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[5]);
        settle_all(&facts, &store);
        let a = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            300,
            Vec::new(),
        )
        .expect("a");
        let b = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            300,
            Vec::new(),
        )
        .expect("b");
        assert_eq!(a, b);
        assert_eq!(
            serde_json::to_string(&a).expect("json a"),
            serde_json::to_string(&b).expect("json b")
        );
    }

    /// Locks survive ranking: always raw, exactly once, never under a
    /// summary, even at a starved budget that fits nothing else — and a
    /// starved ranked pass reports the score-positive drops.
    #[test]
    fn ranked_pass_pins_locks_raw_exactly_once() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let locked = [3u64, 37]; // one old filler, one recent filler
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &locked);
        settle_all(&facts, &store);

        let starved = wake(&facts, &store, Some("daemon socket errors"), 1, Vec::new())
            .expect("starved ranked wake");
        let raw = raw_ids(&starved);
        for id in locked {
            assert_eq!(
                raw.iter().filter(|&&served| served == id).count(),
                1,
                "lock {id} exactly once, raw"
            );
            for item in &starved.items {
                if let WakeItem::Summary { covers, .. } = item {
                    assert!(
                        !(covers.0..covers.1).contains(&id),
                        "summary {covers:?} covers lock {id}"
                    );
                }
            }
        }
        let task = starved.task.as_ref().expect("task block");
        assert_eq!(task.served_scored, 0, "nothing fits budget 1");
        assert_eq!(task.dropped_scored, 10);
        assert!(starved.truncated);
    }

    /// A score-positive fact the budget dropped from raw service never
    /// rides raw through the layout's tail or a split — it was counted
    /// dropped; a stored summary may cover it, or budget took it too.
    #[test]
    fn ranked_pass_never_serves_dropped_scored_facts_raw() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[]);
        settle_all(&facts, &store);

        // ~3 cluster facts fit: the rest must be absent from the raw set.
        // (C1-R sizing: the budget must stay under TASK_BUDGET_CEILING.)
        let report =
            wake(&facts, &store, Some("daemon socket errors"), 50, Vec::new()).expect("ranked");
        let task = report.task.as_ref().expect("task block");
        let raw = raw_ids(&report);
        let served_cluster = cluster.iter().filter(|id| raw.contains(id)).count() as u64;
        assert_eq!(served_cluster, task.served_scored, "counts match items");
        assert!(task.dropped_scored >= 1);
        assert!(report.estimate <= 50, "estimate {}", report.estimate);
    }

    /// The completeness gate is unchanged under ranking: compression
    /// needed, tiling needs an unwritten summary ⇒ the typed refusal.
    #[test]
    fn ranked_pass_keeps_the_completeness_gate() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[]); // nothing settled
        let err = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            300,
            Vec::new(),
        )
        .expect_err("gate fires under --task");
        assert_eq!(err.code(), "E_NAP_PENDING");
    }

    /// Everything fits ⇒ everything raw, in serve order (the cluster
    /// leads), no summaries, no drops — TREE is never consulted, so an
    /// unsettled store is fine. C1-R: the budget must stay below
    /// [`TASK_BUDGET_CEILING`] or the wake degrades to positional order.
    #[test]
    fn ranked_pass_serves_everything_raw_when_it_fits() {
        let cluster = [2u64, 4];
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 8, &cluster, &[]); // no settle needed
        let budget = 300u64; // < ceiling, and > the 8-fact raw total

        let report = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            budget,
            Vec::new(),
        )
        .expect("everything fits");
        assert!(
            report
                .items
                .iter()
                .all(|item| matches!(item, WakeItem::Fact { .. }))
        );
        assert_eq!(report.items.len(), 8);
        let first = raw_ids(&report)[0];
        assert!(cluster.contains(&first), "a cluster fact leads: {first}");
        let task = report.task.as_ref().expect("task block");
        assert_eq!(task.served_scored, 2);
        assert_eq!(task.dropped_scored, 0);
        assert!(!report.truncated);
    }

    // --- C1-R: the task-budget ceiling ----------------------------------------

    /// The boundary is exact, at the owner's pinned value: one token
    /// below 350 the ranked pass runs (no `degraded` key, facts actually
    /// served scored); at 350 the wake degrades — the positional payload
    /// plus the honest task block. The literal budgets (not
    /// `TASK_BUDGET_CEILING ± 1`) are deliberate: moving or removing the
    /// constant fails here, not just downstream.
    #[test]
    fn the_ceiling_boundary_is_exact_below_ranked_at_it_degraded() {
        assert_eq!(
            TASK_BUDGET_CEILING, 350,
            "the owner's post-KP decision: crossover measured between 300 and 400"
        );
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[]);
        settle_all(&facts, &store);

        let below = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            349,
            Vec::new(),
        )
        .expect("ranked below the ceiling");
        let task = below.task.as_ref().expect("task block");
        assert_eq!(task.degraded, None, "below the ceiling nothing degrades");
        assert!(
            serde_json::to_value(task)
                .expect("task json")
                .get("degraded")
                .is_none(),
            "append-only: the key appears only on a degraded wake"
        );
        assert!(
            task.served_scored > 0,
            "the ranked pass really ran: {task:?}"
        );

        let at = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            350,
            Vec::new(),
        )
        .expect("degraded at the ceiling");
        let task = at.task.as_ref().expect("task block");
        assert_eq!(task.degraded, Some(TASK_DEGRADED_AT_CEILING));
        assert_eq!(task.query, "daemon socket errors");
        assert_eq!(task.scorer, rank::SCORER);
        assert_eq!(task.served_scored, 0, "nothing was scored");
        assert_eq!(task.dropped_scored, 0);
    }

    /// Degradation serves the positional payload: above the ceiling the
    /// items, estimate, and truncation are exactly the no-task wake's —
    /// the task block is the only difference — and two runs are
    /// byte-identical. Teeth: a removed ceiling, or a degradation that
    /// serves ranked items instead of positional ones, diverges from the
    /// positional payload here.
    #[test]
    fn degraded_task_wake_is_the_positional_payload_plus_the_task_block() {
        let cluster: Vec<u64> = (0..10).map(|i| i * 2).collect();
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = cluster_setup(tmp.path(), 40, &cluster, &[5]);
        settle_all(&facts, &store);
        let budget = 600u64; // above the ceiling, below everything-fits

        let positional = wake(&facts, &store, None, budget, Vec::new()).expect("positional");
        let degraded = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            budget,
            Vec::new(),
        )
        .expect("degraded");
        assert_eq!(
            degraded.items, positional.items,
            "the positional items serve verbatim"
        );
        assert_eq!(degraded.estimate, positional.estimate);
        assert_eq!(degraded.truncated, positional.truncated);
        assert!(positional.task.is_none());
        let task = degraded.task.as_ref().expect("task block");
        assert_eq!(task.degraded, Some(TASK_DEGRADED_AT_CEILING));
        assert_eq!(task.served_scored, 0);
        assert_eq!(task.dropped_scored, 0);

        // Stripping the task block leaves the positional payload exactly:
        // the one-key difference (both sides through `Value`, so the
        // compare is structural — the per-field asserts above already
        // pinned the typed equality).
        let mut json = serde_json::to_value(&degraded).expect("degraded json");
        json.as_object_mut().expect("report object").remove("task");
        assert_eq!(
            json,
            serde_json::to_value(&positional).expect("positional json"),
            "the task block is the only difference above the ceiling"
        );

        let again = wake(
            &facts,
            &store,
            Some("daemon socket errors"),
            budget,
            Vec::new(),
        )
        .expect("degraded again");
        assert_eq!(
            serde_json::to_string(&degraded).expect("json a"),
            serde_json::to_string(&again).expect("json b"),
            "degraded wakes are byte-identical"
        );
    }

    // --- C4: agent-asserted confidence ----------------------------------------

    /// Two equal-score facts under a budget that seats exactly one (the
    /// C4 acceptance shape): the normal-confidence fact survives, the
    /// `low` fact is the one dropped, and `truncated: true` reports it.
    /// Pins the sabotage teeth both ways — an inverted preference (low
    /// wins) or a filter (neither serves) fails here.
    #[test]
    fn ranked_tie_break_and_budget_pressure_drop_the_low_fact_first() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        // Two facts with identical query-term content (equal score by
        // construction: same tf per term, same idf) and equal cost;
        // filler facts carry none of the vocabulary and push the raw
        // total over the budget so the serve pass runs.
        let low_text = "daemon socket retry quota bravo under load";
        let facts: Vec<FactRecord> = (0..12u64)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-18T00:00:00Z".to_owned(),
                ptr: None,
                text: match id {
                    2 => "daemon socket retry quota alpha under load".to_owned(),
                    6 => low_text.to_owned(),
                    _ => format!("fact {id}: the overlay tiling stays honest and the budget holds"),
                },
                flags: Vec::new(),
                confidence: (id == 6).then_some(Confidence::Low),
            })
            .collect();
        let store = TreeStore::new(&root);
        settle_all(&facts, &store);
        let query = "daemon socket retry quota under load";
        let equal_cost = tokens(facts[2].text.len());
        assert_eq!(tokens(facts[6].text.len()), equal_cost);

        // Budget seats exactly one of the two: the normal one, never the
        // low one — the preference is pinned, not assumed.
        let tight = wake(&facts, &store, Some(query), equal_cost + 2, Vec::new())
            .expect("tight ranked wake");
        let raw = raw_ids(&tight);
        assert!(raw.contains(&2), "the normal fact keeps its seat: {raw:?}");
        assert!(
            !raw.contains(&6),
            "the low fact is the dropped one: {raw:?}"
        );
        let task = tight.task.as_ref().expect("task block");
        assert_eq!(task.served_scored, 1);
        assert_eq!(task.dropped_scored, 1);
        assert!(tight.truncated, "the drop is reported");

        // Room for both ⇒ both serve — down-weight, not filter. And the
        // serve order says the demotion: equal score, normal first, even
        // though the low fact is the newer id.
        let roomy = wake(&facts, &store, Some(query), 6 * equal_cost, Vec::new())
            .expect("roomy ranked wake");
        let raw = raw_ids(&roomy);
        assert!(raw.contains(&2) && raw.contains(&6), "both serve: {raw:?}");
        assert_eq!(
            &raw[..2],
            &[2, 6],
            "normal wins the tie ahead of the newer low"
        );
        let task = roomy.task.as_ref().expect("task block");
        assert_eq!(task.served_scored, 2);
        assert_eq!(task.dropped_scored, 0);
        assert!(!roomy.truncated);
    }

    /// Clemency in the positional pass: the same log, one store with fact
    /// 10 tagged `low` and one without — the tag alone moves the fact out
    /// of its raw seat into the coarser (cheaper) block covering it.
    /// Nothing is dropped: the fact still serves, under a summary.
    #[test]
    fn positional_clemency_seats_a_low_fact_one_level_coarser() {
        let layouts = |low: bool| {
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join(crate::memory::LOCAL_DIR);
            std::fs::create_dir_all(&root).expect("root");
            let facts: Vec<FactRecord> = (0..12u64)
                .map(|id| FactRecord {
                    v: 1,
                    id,
                    ts: "2026-09-14T00:00:00Z".to_owned(),
                    ptr: None,
                    text: format!("fact {id}: a real decision worth {id} tokens of context"),
                    flags: Vec::new(),
                    confidence: (low && id == 10).then_some(Confidence::Low),
                })
                .collect();
            let store = TreeStore::new(&root);
            settle_all(&facts, &store);
            wake(&facts, &store, None, 40, Vec::new()).expect("positional wake")
        };

        let plain = layouts(false);
        let demoted = layouts(true);

        // Untagged (the pre-C4 layout): fact 10 is raw, and the [8,12)
        // region costs a pair summary plus two raw seats.
        assert!(
            raw_ids(&plain).contains(&10),
            "plain layout keeps fact 10 raw: {:?}",
            raw_ids(&plain)
        );
        let tail_cost = |report: &WakeReport| -> u64 {
            report
                .items
                .iter()
                .filter(|item| match item {
                    WakeItem::Fact { id, .. } => *id >= 8,
                    WakeItem::Summary { covers, .. } => covers.0 >= 8,
                })
                .map(|item| match item {
                    WakeItem::Fact { text, .. } | WakeItem::Summary { text, .. } => {
                        tokens(text.len())
                    }
                })
                .sum()
        };

        // Tagged: fact 10 is not raw — a summary covers it — and the
        // region it sits in costs strictly less (the cheaper seat). The
        // clemency layout also fits the budget without dropping anything
        // where the plain layout had to truncate the oldest block.
        let raw = raw_ids(&demoted);
        assert!(!raw.contains(&10), "clemency moved the low fact: {raw:?}");
        let covering = demoted.items.iter().any(|item| match item {
            WakeItem::Summary { covers, .. } => (covers.0..covers.1).contains(&10),
            _ => false,
        });
        assert!(covering, "the low fact still serves, under a summary");
        assert!(
            tail_cost(&demoted) < tail_cost(&plain),
            "cheaper seat: the [8,12) region costs {} tagged vs {} plain",
            tail_cost(&demoted),
            tail_cost(&plain)
        );
        assert!(!demoted.truncated, "the clemency layout fits un-truncated");
        assert!(plain.truncated, "the plain layout had to drop the oldest");

        // Everything fits ⇒ clemency is moot: every fact raw, low or not
        // (the not-filter pin for the positional pass).
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts: Vec<FactRecord> = (0..12u64)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}: a real decision worth {id} tokens of context"),
                flags: Vec::new(),
                confidence: (id == 10).then_some(Confidence::Low),
            })
            .collect();
        let store = TreeStore::new(&root);
        let fits = wake(&facts, &store, None, 100_000, Vec::new()).expect("everything fits");
        assert!(raw_ids(&fits).contains(&10));
        assert!(
            fits.items
                .iter()
                .all(|item| matches!(item, WakeItem::Fact { .. }))
        );
    }

    /// Pins beat clemency: a locked `low` fact stays raw exactly like any
    /// lock (the §1.5.2 invariant is untouched by C4), and its payload
    /// item carries both the flag and the tag.
    #[test]
    fn a_locked_low_fact_stays_raw_and_carries_both_marks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts: Vec<FactRecord> = (0..12u64)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}: a real decision worth {id} tokens of context"),
                flags: if id == 10 {
                    vec![MemoryFlag::ArchitecturalLock]
                } else {
                    Vec::new()
                },
                confidence: (id == 10).then_some(Confidence::Low),
            })
            .collect();
        let store = TreeStore::new(&root);
        settle_all(&facts, &store);

        let report = wake(&facts, &store, None, 40, Vec::new()).expect("starved positional");
        let raw = raw_ids(&report);
        assert_eq!(raw.iter().filter(|&&id| id == 10).count(), 1, "lock raw");
        for item in &report.items {
            if let WakeItem::Summary { covers, .. } = item {
                assert!(
                    !(covers.0..covers.1).contains(&10),
                    "no summary over the lock"
                );
            }
        }

        // The payload item carries both marks, and only tagged facts
        // carry the key at all (append-only: untagged items are the
        // pre-C4 shape) — checked at a roomy budget where the whole
        // layout survives, summaries included.
        let roomy = wake(&facts, &store, None, 60, Vec::new()).expect("roomy positional");
        let item = roomy
            .items
            .iter()
            .find_map(|item| match item {
                WakeItem::Fact {
                    id: 10,
                    flags,
                    confidence,
                    ..
                } => Some((flags, confidence)),
                _ => None,
            })
            .expect("the locked low fact is served");
        assert_eq!(*item.0, vec![MemoryFlag::ArchitecturalLock]);
        assert_eq!(*item.1, Some(Confidence::Low));
        let json = serde_json::to_value(&roomy).expect("json");
        for item in json["items"].as_array().expect("items") {
            if item["id"] == 10 {
                assert_eq!(item["confidence"], "low");
                assert_eq!(item["flags"], serde_json::json!(["architectural_lock"]));
            } else {
                assert!(
                    item.get("confidence").is_none(),
                    "untagged items carry no confidence key: {item}"
                );
            }
        }
    }

    /// Deterministic with tagged facts in the store: identical inputs,
    /// byte-identical payloads in both passes.
    #[test]
    fn tagged_wakes_are_deterministic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts: Vec<FactRecord> = (0..16u64)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-18T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}: a real decision worth {id} tokens of context"),
                flags: Vec::new(),
                confidence: (id % 3 == 0).then_some(Confidence::Low),
            })
            .collect();
        let store = TreeStore::new(&root);
        settle_all(&facts, &store);
        for task in [None, Some("decision worth")] {
            let a = wake(&facts, &store, task, 90, Vec::new()).expect("wake a");
            let b = wake(&facts, &store, task, 90, Vec::new()).expect("wake b");
            assert_eq!(
                serde_json::to_string(&a).expect("json a"),
                serde_json::to_string(&b).expect("json b")
            );
        }
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

            let report = wake(&facts, &store, None, budget, Vec::new()).expect("settled tree wakes");

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

    /// The ranked-pass contract, over randomized logs and budgets (C1-R:
    /// always below [`TASK_BUDGET_CEILING`] — above it the ranked pass
    /// does not run, and its own tests own that regime): budget
    /// respected (or honestly over with only pinned raws left), locks
    /// exactly once raw and never under a summary, no summary covers a
    /// served fact, the score-positive raw count is exactly what the task
    /// block reports (dropped facts never ride raw), and every summary
    /// still satisfies the bias rule. Two query regimes alternate by `n`:
    /// one matching every fact's vocabulary (all-positive) and one the
    /// store does not carry (none-positive) — the in-between shapes are
    /// the unit fixtures' job.
    #[test]
    fn ranked_properties_hold_over_random_logs_and_budgets() {
        let config = ProptestConfig::with_cases(24);
        proptest!(config, |(n in 2u64..24, lock_bits in prop::collection::vec(any::<bool>(), 0..24), budget in 1u64..TASK_BUDGET_CEILING)| {
            let locked: Vec<u64> = (0..n)
                .filter(|id| lock_bits.get(*id as usize).copied().unwrap_or(false))
                .collect();
            let query = if n % 2 == 0 { "decision worth" } else { "zzz qqq absent" };
            let tmp = tempfile::tempdir().expect("tempdir");
            let (facts, store) = setup(tmp.path(), n, &locked);
            settle_all(&facts, &store);

            let report = wake(&facts, &store, Some(query), budget, Vec::new())
                .expect("settled tree wakes");
            let task = report.task.as_ref().expect("task block");

            // Score-positive raws, recomputed from the same scorer the
            // pass used — the task block must agree with the items.
            let scored = rank::Query::new(query, &facts);
            let positive: Vec<u64> = facts
                .iter()
                .filter(|fact| scored.score(fact) > 0.0)
                .map(|fact| fact.id)
                .collect();
            let raw = raw_ids(&report);
            let raw_positive = positive.iter().filter(|id| raw.contains(id)).count() as u64;
            prop_assert_eq!(raw_positive, task.served_scored);
            prop_assert_eq!(task.dropped_scored, positive.len() as u64 - task.served_scored);
            prop_assert_eq!(report.truncated, task.dropped_scored > 0);
            prop_assert_eq!(task.scorer, rank::SCORER);

            // Budget respected — or honestly over with only pinned raws.
            let only_pinned = report.items.iter().all(|item| match item {
                WakeItem::Fact { id, flags, .. } => {
                    positive.contains(id) || flags.contains(&MemoryFlag::ArchitecturalLock)
                }
                WakeItem::Summary { .. } => false,
            });
            prop_assert!(report.estimate <= budget || only_pinned,
                "estimate {} budget {} items {:?}",
                report.estimate, budget, report.items.len());

            // Locks: exactly once, raw, never covered.
            for id in &locked {
                prop_assert_eq!(
                    raw.iter().filter(|&&r| r == *id).count(),
                    1,
                    "lock {} present exactly once",
                    id
                );
            }

            // No summary covers a lock or a served (raw) fact; bias holds.
            let total = n;
            for item in &report.items {
                if let WakeItem::Summary { covers, .. } = item {
                    let (lo, hi) = *covers;
                    for id in locked.iter().chain(raw.iter()) {
                        prop_assert!(!(lo..hi).contains(id), "summary [{lo},{hi}) covers raw {id}");
                    }
                    prop_assert!(lo.is_multiple_of(hi - lo));
                    prop_assert!(hi - lo <= PRESENT_BIAS_ALPHA * (total - hi), "bias [{lo},{hi})");
                }
            }
        });
    }

    /// The C4 contract over randomized tagged stores: a `low` tag
    /// down-weights, it never filters. When the budget allows the whole
    /// log raw, every tagged fact serves raw in BOTH passes (no tier,
    /// no clemency can hide it); under arbitrary budgets the positional
    /// pass never leaves a tagged fact unserved while budget remains —
    /// clemency moves it under a summary, it does not drop it. The bias
    /// invariant keeps holding for every block that covers no tagged
    /// fact; a tagged block may sit one level coarser (the documented
    /// exception), never more.
    #[test]
    fn confidence_properties_hold_over_random_tagged_logs() {
        let config = ProptestConfig::with_cases(24);
        proptest!(config, |(n in 2u64..24, low_bits in prop::collection::vec(any::<bool>(), 0..24), budget in 1u64..4000)| {
            let tagged: Vec<u64> = (0..n)
                .filter(|id| low_bits.get(*id as usize).copied().unwrap_or(false))
                .collect();
            if tagged.is_empty() {
                return Ok(());
            }
            let tmp = tempfile::tempdir().expect("tempdir");
            let root = tmp.path().join(crate::memory::LOCAL_DIR);
            std::fs::create_dir_all(&root).expect("root");
            let facts: Vec<FactRecord> = (0..n)
                .map(|id| FactRecord {
                    v: 1,
                    id,
                    ts: "2026-09-18T00:00:00Z".to_owned(),
                    ptr: None,
                    text: format!("fact {id}: a real decision worth {id} tokens of context"),
                    flags: Vec::new(),
                    confidence: tagged.contains(&id).then_some(Confidence::Low),
                })
                .collect();
            let store = TreeStore::new(&root);
            settle_all(&facts, &store);

            // Budget allows the whole log raw ⇒ every tagged fact is a
            // raw item in both passes, each exactly once, each tagged.
            // C1-R: the `--task` leg must also stay below the ceiling or
            // it degrades to positional — the fixture's short texts keep
            // raw_total under it, and the assert makes that precondition
            // loud if the fixture ever outgrows it.
            let raw_total: u64 = facts.iter().map(|f| tokens(f.text.len())).sum();
            prop_assert!(
                raw_total < TASK_BUDGET_CEILING,
                "fixture outgrew the ranked budget range: raw_total {raw_total}"
            );
            for task in [None, Some("decision worth")] {
                let fits = wake(&facts, &store, task, raw_total, Vec::new())
                    .expect("everything fits, both passes");
                for id in &tagged {
                    let hits = fits
                        .items
                        .iter()
                        .filter(|item| matches!(item,
                            WakeItem::Fact { id: fact, confidence: Some(Confidence::Low), .. }
                                if fact == id))
                        .count();
                    prop_assert_eq!(hits, 1, "tagged fact {} raw exactly once under task {:?}", id, task);
                }
            }

            // Arbitrary budget, positional: every fact is served (raw or
            // under a summary) or the payload is honestly truncated — a
            // tagged fact is never silently singled out.
            let report = wake(&facts, &store, None, budget, Vec::new()).expect("positional wake");
            let covered: Vec<u64> = report
                .items
                .iter()
                .flat_map(|item| match item {
                    WakeItem::Fact { id, .. } => vec![*id],
                    WakeItem::Summary { covers, .. } => (covers.0..covers.1).collect(),
                })
                .collect();
            if !report.truncated {
                for id in 0..n {
                    prop_assert!(covered.contains(&id), "fact {id} unserved without truncation");
                }
            }

            // The bias invariant keeps its shape: a summary block either
            // satisfies the rule, is exactly one level coarser than a
            // rule-satisfying block AND covers a tagged fact (C4
            // clemency), or descends from a settled aligned ancestor
            // covering a tagged fact — the shaping passes (steps 3–4)
            // legitimately split a clemency-blessed block into halves,
            // and each half inherits the blessing by alignment: same
            // ancestry, same store line, finer detail than the blessed
            // parent is never a bias violation. Without a tagged fact in
            // the ancestry chain, the strict rule still bites.
            let total = n;
            for item in &report.items {
                if let WakeItem::Summary { covers, .. } = item {
                    let (lo, hi) = *covers;
                    let size = hi - lo;
                    let age = total - hi;
                    if size > PRESENT_BIAS_ALPHA * age {
                        // Blessed ancestry: some aligned power-of-two
                        // block CONTAINING this one covers a tagged fact
                        // (its summary was settled — the store served the
                        // split children from that very line). The tiler's
                        // clemency blessed the container; the shaping
                        // passes' splits of it inherit the blessing: same
                        // store line, finer detail than blessed is never a
                        // bias violation. No tagged fact in any containing
                        // aligned block ⇒ the strict rule bites.
                        let ancestor_blessed = (1u64..)
                            .map(|shift| 1u64 << shift)
                            .take_while(|&sz| sz <= total)
                            .any(|sz| {
                                let start = lo - (lo % sz);
                                let end = start + sz;
                                end <= total
                                    && start <= lo
                                    && hi <= end
                                    && tagged.iter().any(|id| (start..end).contains(id))
                            });
                        let covers_tagged = tagged.iter().any(|id| (lo..hi).contains(id));
                        let half_ok =
                            size / 2 <= PRESENT_BIAS_ALPHA * (total - (lo + size / 2));
                        prop_assert!(covers_tagged && half_ok || ancestor_blessed,
                            "block [{lo},{hi}) violates bias beyond the one-level clemency");
                    }
                }
            }
        });
    }
}
