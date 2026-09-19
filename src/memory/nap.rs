//! `nap` — compression as a synchronous, agent-owned protocol
//! (ADR-0013, PLAN §1.5.4).
//!
//! The BRIEF sketched a background routine; a static binary has no LLM,
//! and OptMem's hard-won lesson (verified, A.2) is that summarization
//! **must** be done by the consuming agent itself, one compression at a
//! time, sequenced synchronously by the tool — *"nothing ever runs in the
//! background."* `nap` is exactly that sequencing surface:
//!
//! 1. `janitor nap` (no args) returns the **next pending** compression
//!    request: the block's slot notation, the source material (the raw
//!    facts when the block has ≤ [`SOURCE_FACTS_LIMIT`] of them, else the
//!    two child summaries), and the instruction. Nothing pending ⇒
//!    `"Nothing left to compress."`;
//! 2. the agent writes the line and submits it:
//!    `janitor nap --slot "0-2" --text "…"` (≤ 256 bytes, one line),
//!    which validates and appends one fixed-width record
//!    ([`crate::memory::tree_store`]);
//! 3. blocks are **built in order** — the planner walks levels ascending
//!    and slots ascending, and by that order every proposed block's
//!    children are already settled (the induction is structural: a level
//!    is only reached once the level below it is fully settled), so the
//!    store's dense-prefix and children-before-parents rules never bite a
//!    well-meaning agent;
//! 4. `wake` gates on completeness ([`crate::memory::wake`]) — the loop
//!    closes with no background routine and no LLM in the binary.

use serde::Serialize;

use crate::error::JanitorError;
use crate::memory::log::FactRecord;
use crate::memory::tree_store::{BlockPlan, BlockSlot, LevelSnapshot, SettleOutcome, TreeStore};

/// Blocks up to this size are summarized from their raw facts; larger
/// blocks are summarized from their two child summaries (PLAN §1.5.4).
pub const SOURCE_FACTS_LIMIT: u64 = 16;

/// The settled message — the protocol's exact wording (§1.5.4).
pub const NOTHING_LEFT: &str = "Nothing left to compress.";

/// The material an agent summarizes a block from.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NapSource {
    /// The block's raw facts (small blocks).
    Facts { facts: Vec<FactRecord> },
    /// The block's two child summaries (large blocks).
    Children { children: Vec<ChildSummary> },
}

/// One settled child summary a larger block is summarized from.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ChildSummary {
    /// The child's slot notation (e.g. `"0-16"`).
    pub slot: String,
    /// The child's stored summary line.
    pub text: String,
}

/// One pending compression request — everything the agent needs to write
/// the line, and exactly what an `E_NAP_PENDING` refusal carries.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NapRequest {
    /// The slot to submit against (`nap --slot`).
    pub slot: String,
    /// The block size (the TREE level).
    pub level: u64,
    /// The covered fact ids `[lo, hi)`.
    pub covers: (u64, u64),
    /// What to summarize (see [`NapSource`]).
    pub source: NapSource,
    /// How to write and submit the line.
    pub instruction: String,
}

/// The answer to `janitor nap`: the next request, or the settled message.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct NapPlan {
    pub pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<NapRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<&'static str>,
}

/// The next pending compression request, or the settled message. The scan
/// is levels ascending, slots ascending — the canonical build order the
/// tree store's dense prefix expects (see the module docs for why every
/// proposed slot's children are guaranteed settled). One batch load pays
/// for every settlement probe the scan makes (C3).
pub fn next_pending(facts: &[FactRecord], store: &TreeStore) -> Result<NapPlan, JanitorError> {
    let levels = store.load_levels(facts)?;
    next_pending_over(facts, &levels)
}

/// [`next_pending`] over levels already batch-loaded — wake's gate path
/// reuses the snapshot it read, so a gate refusal costs no extra reads.
pub fn next_pending_over(
    facts: &[FactRecord],
    levels: &LevelSnapshot,
) -> Result<NapPlan, JanitorError> {
    let plan = BlockPlan::new(facts);
    for size in plan.levels() {
        for lo in plan.compressible_slots(facts, size) {
            let slot = BlockSlot { size, lo };
            if let Some(request) = build_request(facts, levels, slot)? {
                return Ok(NapPlan {
                    pending: true,
                    request: Some(request),
                    message: None,
                });
            }
        }
    }
    Ok(NapPlan {
        pending: false,
        request: None,
        message: Some(NOTHING_LEFT),
    })
}

/// Build the request for one specific slot, if it is pending (`None` when
/// already settled — the idempotency probe tests also use). The slot must
/// be compressible (alignment/range/locks) — an incompressible slot is
/// never a compression request, so it is `None` too.
pub fn build_request(
    facts: &[FactRecord],
    levels: &LevelSnapshot,
    slot: BlockSlot,
) -> Result<Option<NapRequest>, JanitorError> {
    let plan = BlockPlan::new(facts);
    if !plan.is_compressible(facts, slot.size, slot.lo) {
        return Ok(None);
    }
    if levels.summary(slot.size, slot.lo)?.is_some() {
        return Ok(None); // settled — not pending
    }

    let source = if slot.size <= SOURCE_FACTS_LIMIT {
        NapSource::Facts {
            facts: facts[slot.lo as usize..(slot.lo + slot.size) as usize].to_vec(),
        }
    } else {
        let mut children = Vec::with_capacity(2);
        for child in slot.children() {
            let text = levels.summary(child.size, child.lo)?.ok_or_else(|| {
                JanitorError::from(crate::error::MemoryError::SlotInvalid {
                    slot: slot.notation(),
                    detail: format!(
                        "child slot {} is not settled — blocks are built in order",
                        child.notation()
                    ),
                })
            })?;
            children.push(ChildSummary {
                slot: child.notation(),
                text,
            });
        }
        NapSource::Children { children }
    };

    let end = slot.lo + slot.size;
    let what = match &source {
        NapSource::Facts { .. } => format!("facts {lo}-{end}", lo = slot.lo),
        NapSource::Children { .. } => format!(
            "the two child summaries of block {slot}",
            slot = slot.notation()
        ),
    };
    Ok(Some(NapRequest {
        slot: slot.notation(),
        level: slot.size,
        covers: (slot.lo, end),
        source,
        instruction: format!(
            "Write ONE summary line (max 256 bytes, single line) for {what}, preserving \
             every decision, name, and constraint a future session must not get wrong. \
             Submit: janitor nap --slot {slot} --text \"<summary>\"",
            slot = slot.notation(),
        ),
    }))
}

/// Store a submitted summary (`nap --slot <lo-hi> --text <line>`):
/// parse the notation, then the tree store's full validation (text
/// discipline, compressibility, dense prefix, children, idempotency).
pub fn settle(
    facts: &[FactRecord],
    store: &TreeStore,
    notation: &str,
    text: &str,
) -> Result<SettleOutcome, JanitorError> {
    let slot = BlockSlot::parse(notation)?;
    store.settle(facts, slot.size, slot.lo, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::log::MemoryFlag;

    fn setup(tmp: &std::path::Path, n: u64, locked: &[u64]) -> (Vec<FactRecord>, TreeStore) {
        let root = tmp.join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        let facts = (0..n)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}: decision delta {id}"),
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

    fn summary_text(slot: &BlockSlot) -> String {
        format!("summary of {} covering the run", slot.notation())
    }

    /// The canonical build order: level 2 left-to-right, then 4, then 8 —
    /// and the settled message once everything is written. Settling is
    /// driven purely by following `next_pending`, the way an agent would.
    #[test]
    fn next_pending_walks_the_canonical_order_until_settled() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 8, &[]);

        let mut seen: Vec<String> = Vec::new();
        for _ in 0..32 {
            let plan = next_pending(&facts, &store).expect("plan");
            let Some(request) = plan.request else {
                assert!(!plan.pending);
                assert_eq!(plan.message, Some(NOTHING_LEFT));
                break;
            };
            let slot = BlockSlot::parse(&request.slot).expect("notation round-trips");
            seen.push(request.slot.clone());
            settle(&facts, &store, &request.slot, &summary_text(&slot)).expect("settle");
        }
        assert_eq!(
            seen,
            vec![
                "0-2", "2-4", "4-6", "6-8", // level 2
                "0-4", "4-8", // level 4
                "0-8", // level 8
            ],
            "levels ascending, slots ascending"
        );

        // Fully settled: the request is the message, byte-exact wording.
        let done = next_pending(&facts, &store).expect("plan");
        assert_eq!(
            serde_json::to_value(&done).expect("json"),
            serde_json::json!({ "pending": false, "message": "Nothing left to compress." })
        );
    }

    /// Source selection: small blocks carry raw facts; once a block grows
    /// past [`SOURCE_FACTS_LIMIT`], the source is the two child summaries.
    #[test]
    fn source_is_facts_for_small_blocks_and_children_past_the_limit() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 32, &[]);

        // Level 2 (and every level ≤ 16): raw facts.
        let plan = next_pending(&facts, &store).expect("plan");
        let request = plan.request.expect("first request");
        assert_eq!(request.slot, "0-2");
        assert!(
            matches!(&request.source, NapSource::Facts { facts } if facts.len() == 2
            && facts[0].text == "fact 0: decision delta 0")
        );
        assert!(request.instruction.contains("nap --slot 0-2"));

        // Follow the protocol until the 0-32 request appears.
        let mut level32 = None;
        for _ in 0..200 {
            let plan = next_pending(&facts, &store).expect("plan");
            let Some(request) = plan.request else { break };
            let slot = BlockSlot::parse(&request.slot).expect("slot");
            if request.level == 32 {
                level32 = Some(request.clone());
                break;
            }
            settle(&facts, &store, &request.slot, &summary_text(&slot)).expect("settle");
        }
        let request = level32.expect("the 0-32 block becomes pending");
        let NapSource::Children { children } = &request.source else {
            unreachable!("blocks past the limit summarize their children");
        };
        assert_eq!(
            children.iter().map(|c| c.slot.as_str()).collect::<Vec<_>>(),
            vec!["0-16", "16-32"]
        );
        assert!(children[0].text.contains("0-16"));
        assert!(request.instruction.contains("child summaries"));
    }

    /// Locked facts never enter compression: their slots are never
    /// proposed, and once every lock-free block is settled the protocol
    /// reports nothing left — locks stay raw forever, by construction.
    #[test]
    fn locks_are_never_proposed_and_settle_refuses_their_ranges() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 4, &[1]);

        for _ in 0..16 {
            let plan = next_pending(&facts, &store).expect("plan");
            let Some(request) = plan.request else { break };
            assert_ne!(request.slot, "0-2", "that range holds the lock");
            settle(
                &facts,
                &store,
                &request.slot,
                &summary_text(&BlockSlot::parse(&request.slot).expect("slot")),
            )
            .expect("settle");
        }
        let done = next_pending(&facts, &store).expect("plan");
        assert!(!done.pending, "nothing left except the locked range");

        // Direct submission into the locked range is refused by name.
        let err = settle(&facts, &store, "0-2", "swallow the lock").expect_err("locked");
        assert_eq!(err.code(), "E_SLOT_INVALID");
        assert!(err.to_string().contains("architectural_lock fact 1"));
    }

    /// `settle` is the notation front-end: bad slot notation is the typed
    /// E_SLOT_INVALID before anything touches the store.
    #[test]
    fn settle_parses_the_notation_typedly() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (facts, store) = setup(tmp.path(), 4, &[]);
        for bad in ["0-3", "nope", "4-2"] {
            let err = settle(&facts, &store, bad, "text").expect_err("bad notation");
            assert_eq!(err.code(), "E_SLOT_INVALID", "{bad:?}");
        }
        // And the round trip through the real path works.
        let out = settle(&facts, &store, "0-2", "first block").expect("settle");
        assert_eq!(out.slot, "0-2");
        assert!(!out.idempotent);
    }
}
