//! `TREE/<size>` — the fixed-width summary store (ADR-0006, PLAN §1.5.4).
//!
//! One file per block size under `.janitor/TREE/` (`TREE/2`, `TREE/4`, …),
//! each record **exactly 256 bytes**: the one-line summary text NUL-padded
//! to width. Fixed width keeps the O(1) seek-by-slot property OptMem's
//! summary files have; the *raw* log stays JSONL because facts carry
//! structured pointers (ADR-0006's split — each format where it pays).
//!
//! **Position is identity here too, but over *compressible* slots**: a
//! level's file holds, in ascending slot order, one record per slot that
//! is legal to compress — aligned, fully inside the fact range, and free
//! of `architectural_lock` facts (locks never enter compression, §1.5.1).
//! A slot's record index is therefore its **rank among the level's
//! compressible slots**, a pure function of LOG.txt — position stays
//! derivable, the file stays rebuildable from the log alone, and a
//! lock-containing slot (never writable, never needed) does not stall the
//! level's build frontier behind it.
//!
//! The **dense prefix is the build frontier**: slot *k* of a level is
//! writable only when every earlier compressible slot of that level is
//! settled (out-of-order ⇒ typed [`MemoryError::SlotInvalid`]; re-submitting
//! a settled slot with the same text is an idempotent no-op, with different
//! text a refusal). Torn trailing records (file length not a multiple of
//! the width) are trimmed with a warning — the tree is a pure cache; the
//! log is the source of truth. A `forget`-style truncate command is F10.

use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{JanitorError, MemoryError, VfsError};
use crate::memory::log::{FactRecord, LogWarning};
use crate::vfs::Os;

/// One summary record's exact width (ADR-0006: 256 B one-liners).
pub const RECORD_WIDTH: usize = 256;

/// The summary line limit — a record's whole payload (the text IS the
/// record, NUL-padded), so the limits coincide.
pub const MAX_SUMMARY_BYTES: usize = RECORD_WIDTH;

/// The TREE directory name inside `.janitor/`.
pub const TREE_DIR: &str = "TREE";

/// One compression block: level `size` covering fact ids `[lo, lo + size)`
/// — aligned (`lo % size == 0`), power-of-two size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockSlot {
    pub size: u64,
    pub lo: u64,
}

impl BlockSlot {
    /// The slot's CLI notation: `"<lo>-<hi>"` (half-open, e.g. `"0-2"`).
    pub fn notation(&self) -> String {
        format!("{}-{}", self.lo, self.lo + self.size)
    }

    /// Parse the `"<lo>-<hi>"` notation. Power-of-two width ≥ 2 is checked
    /// here; alignment and range against the log live in [`BlockPlan`].
    pub fn parse(notation: &str) -> Result<Self, MemoryError> {
        let bad = |detail: String| MemoryError::SlotInvalid {
            slot: notation.to_owned(),
            detail,
        };
        let (lo, hi) = notation
            .split_once('-')
            .ok_or_else(|| bad(format!("expected <lo>-<hi>, got {notation:?}")))?;
        let lo: u64 = lo
            .parse()
            .map_err(|_| bad(format!("slot start {lo:?} is not an id")))?;
        let hi: u64 = hi
            .parse()
            .map_err(|_| bad(format!("slot end {hi:?} is not an id")))?;
        let size = hi
            .checked_sub(lo)
            .ok_or_else(|| bad(format!("slot [{lo}, {hi}) is empty or inverted")))?;
        if size < 2 || !size.is_power_of_two() {
            return Err(bad(format!("block size {size} is not a power of two ≥ 2")));
        }
        Ok(Self { size, lo })
    }

    /// The slot's two children (half size); the children of a size-2 block
    /// are the raw facts themselves.
    pub fn children(&self) -> [BlockSlot; 2] {
        let half = self.size / 2;
        [
            BlockSlot {
                size: half,
                lo: self.lo,
            },
            BlockSlot {
                size: half,
                lo: self.lo + half,
            },
        ]
    }
}

/// Which blocks exist over a log — a pure function of the facts (so the
/// tree stays rebuildable from LOG.txt alone): the levels, the
/// compressible slots per level, and slot↔rank translation.
#[derive(Clone, Copy, Debug)]
pub struct BlockPlan {
    fact_count: u64,
}

impl BlockPlan {
    pub fn new(facts: &[FactRecord]) -> Self {
        Self {
            fact_count: facts.len() as u64,
        }
    }

    pub fn fact_count(&self) -> u64 {
        self.fact_count
    }

    /// Every level with at least one possible slot: 2, 4, … up to the
    /// largest power of two ≤ the fact count.
    pub fn levels(&self) -> Vec<u64> {
        let mut levels = Vec::new();
        let mut size = 2u64;
        while size <= self.fact_count {
            levels.push(size);
            size *= 2;
        }
        levels
    }

    /// Whether the block `[lo, lo + size)` may be compressed: aligned,
    /// fully inside the fact range, and free of locked facts. Locks never
    /// enter compression ranges (§1.5.1/§1.5.4) — the wake tiler and this
    /// store agree by construction, both deriving from the log.
    pub fn is_compressible(&self, facts: &[FactRecord], size: u64, lo: u64) -> bool {
        size >= 2
            && size.is_power_of_two()
            && lo.is_multiple_of(size)
            && lo + size <= self.fact_count
            && facts[lo as usize..(lo + size) as usize]
                .iter()
                .all(|fact| !fact.is_locked())
    }

    /// Compressible slots of `size`, ascending by `lo`.
    pub fn compressible_slots(&self, facts: &[FactRecord], size: u64) -> Vec<u64> {
        if self.fact_count == 0 {
            return Vec::new();
        }
        (0..self.fact_count)
            .step_by(size.max(1) as usize)
            .take_while(|&lo| lo + size <= self.fact_count)
            .filter(|&lo| self.is_compressible(facts, size, lo))
            .collect()
    }

    /// The record rank of slot `(size, lo)` among the level's compressible
    /// slots (None when the slot is not compressible). Position-derived:
    /// recomputable from the log at any time.
    pub fn rank(&self, facts: &[FactRecord], size: u64, lo: u64) -> Option<u64> {
        if !self.is_compressible(facts, size, lo) {
            return None;
        }
        Some(
            self.compressible_slots(facts, size)
                .into_iter()
                .take_while(|&slot| slot < lo)
                .count() as u64,
        )
    }
}

/// One level's settled records (ascending by slot) plus any load warnings.
pub type LevelRecords = (Vec<String>, Vec<LogWarning>);

/// The fixed-width summary store rooted at `.janitor/TREE/`.
#[derive(Clone, Debug)]
pub struct TreeStore {
    root: PathBuf,
}

impl TreeStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The TREE directory (`.janitor/TREE`).
    pub fn dir(&self) -> PathBuf {
        self.root.join(TREE_DIR)
    }

    fn file(&self, size: u64) -> PathBuf {
        self.dir().join(size.to_string())
    }

    /// Load one level's settled summaries, ascending by slot. `None` when
    /// the level file does not exist. A torn trailing record (length not a
    /// multiple of the width) is trimmed with a warning — the tree is a
    /// cache, the log is the truth. Structurally invalid records are the
    /// typed [`MemoryError::TreeCorrupt`].
    pub fn load(&self, size: u64) -> Result<Option<LevelRecords>, JanitorError> {
        let path = self.file(size);
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(err) => {
                return Err(VfsError::Io {
                    path: path.display().to_string(),
                    message: err.to_string(),
                }
                .into());
            }
        };

        let mut warnings = Vec::new();
        let whole = bytes.len() / RECORD_WIDTH;
        let mut records = Vec::with_capacity(whole);
        let (chunks, _) = bytes[..whole * RECORD_WIDTH].as_chunks::<RECORD_WIDTH>();
        for record in chunks {
            records.push(decode_record(record, &path)?);
        }
        if bytes.len() % RECORD_WIDTH != 0 {
            // Torn tail record: trim it (atomic rewrite); the slot simply
            // becomes pending again — nap will re-request it.
            Os.write_atomic(&path, &bytes[..whole * RECORD_WIDTH])
                .map_err(JanitorError::from)?;
            warnings.push(LogWarning {
                kind: "tree_torn_record_trimmed",
                message: format!(
                    "TREE/{size} ended in a partial record ({} bytes); trimmed to {whole} \
                     whole records — the slot is pending again (the log is the source of truth)",
                    bytes.len() % RECORD_WIDTH
                ),
                quarantined: None,
            });
        }
        Ok(Some((records, warnings)))
    }

    /// The summary stored for slot `(size, lo)`, if settled (`facts`
    /// supplies the compressible ordering — rank is a function of the log).
    pub fn summary(
        &self,
        facts: &[FactRecord],
        size: u64,
        lo: u64,
    ) -> Result<Option<String>, JanitorError> {
        let plan = BlockPlan::new(facts);
        let Some(rank) = plan.rank(facts, size, lo) else {
            return Ok(None); // not a compressible slot: nothing can be there
        };
        match self.load(size)? {
            None => Ok(None),
            Some((records, _)) => Ok(records.get(rank as usize).cloned()),
        }
    }

    /// Store one summary record (the `nap --slot --text` half of the
    /// protocol). Validation, in order: the text discipline; the slot's
    /// compressibility (alignment, range, lock-freedom); the dense prefix
    /// (every earlier compressible slot of the level settled); the
    /// children's availability (the two half-size summaries for size ≥ 4 —
    /// size-2 blocks summarize raw facts, always present). Then either the
    /// idempotent no-op (same text) or the refusal (different text), or
    /// the append + fsync.
    pub fn settle(
        &self,
        facts: &[FactRecord],
        size: u64,
        lo: u64,
        text: &str,
    ) -> Result<SettleOutcome, JanitorError> {
        let plan = BlockPlan::new(facts);
        let slot = BlockSlot { size, lo };
        validate_summary_text(&slot, text)?;

        if !plan.is_compressible(facts, size, lo) {
            let detail = if !lo.is_multiple_of(size) {
                format!("slot is unaligned: {lo} is not a multiple of {size}")
            } else if lo + size > plan.fact_count() {
                format!(
                    "slot [{lo}, {}) extends beyond the {} logged facts",
                    lo + size,
                    plan.fact_count()
                )
            } else {
                let locked = facts[lo as usize..(lo + size) as usize]
                    .iter()
                    .find(|fact| fact.is_locked())
                    .map_or(lo, |fact| fact.id);
                format!(
                    "slot contains architectural_lock fact {locked} — locks are never compressed"
                )
            };
            return Err(MemoryError::SlotInvalid {
                slot: slot.notation(),
                detail,
            }
            .into());
        }
        let rank = plan.rank(facts, size, lo).unwrap_or(0); // compressible — checked above

        let settled = self
            .load(size)?
            .map_or(0, |(records, _)| records.len() as u64);
        if rank < settled {
            let stored = self.summary(facts, size, lo)?.unwrap_or_default();
            if stored == text {
                return Ok(SettleOutcome {
                    slot: slot.notation(),
                    idempotent: true,
                });
            }
            return Err(MemoryError::SlotInvalid {
                slot: slot.notation(),
                detail: format!(
                    "already settled with different text ({stored:?}); TREE/* is append-only"
                ),
            }
            .into());
        }
        if rank > settled {
            return Err(MemoryError::SlotInvalid {
                slot: slot.notation(),
                detail: format!(
                    "out of order: {settled} slot(s) of TREE/{size} are settled, this is rank \
                     {rank} — blocks are built in order (dense prefix)"
                ),
            }
            .into());
        }

        // Children before parents: a size ≥ 4 block summarizes its two
        // half-size summaries; a size-2 block summarizes raw facts.
        if size >= 4 {
            for child in slot.children() {
                if self.summary(facts, child.size, child.lo)?.is_none() {
                    return Err(MemoryError::SlotInvalid {
                        slot: slot.notation(),
                        detail: format!(
                            "child slot {} is not settled yet — blocks are built bottom-up",
                            child.notation()
                        ),
                    }
                    .into());
                }
            }
        }

        let mut record = [0u8; RECORD_WIDTH];
        record[..text.len()].copy_from_slice(text.as_bytes());
        let path = self.file(size);
        fs::create_dir_all(self.dir()).map_err(|err| VfsError::Io {
            path: self.dir().display().to_string(),
            message: err.to_string(),
        })?;
        let io_err = |err: std::io::Error| -> JanitorError {
            VfsError::Io {
                path: path.display().to_string(),
                message: err.to_string(),
            }
            .into()
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(io_err)?;
        file.write_all(&record)
            .and_then(|()| file.sync_all())
            .map_err(io_err)?;
        Ok(SettleOutcome {
            slot: slot.notation(),
            idempotent: false,
        })
    }
}

/// The `nap --slot --text` result.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct SettleOutcome {
    /// The slot as submitted.
    pub slot: String,
    /// True when the identical text was already stored (idempotent no-op).
    pub idempotent: bool,
}

/// The summary discipline: one line, ≤ [`MAX_SUMMARY_BYTES`] bytes, no NUL
/// (NUL is the record padding), non-empty.
pub fn validate_summary_text(slot: &BlockSlot, text: &str) -> Result<(), MemoryError> {
    let bad = |detail: String| MemoryError::SlotInvalid {
        slot: slot.notation(),
        detail,
    };
    if text.is_empty() {
        return Err(bad("summary text is empty".to_owned()));
    }
    if text.contains('\n') || text.contains('\r') {
        return Err(bad("summary text must be a single line".to_owned()));
    }
    if text.contains('\0') {
        return Err(bad(
            "summary text must not contain NUL (record padding)".to_owned()
        ));
    }
    if text.len() > MAX_SUMMARY_BYTES {
        return Err(bad(format!(
            "summary text is {len} bytes; the limit is {MAX_SUMMARY_BYTES}",
            len = text.len()
        )));
    }
    Ok(())
}

/// Decode one 256-byte record: the text runs to the first NUL (or the full
/// width); everything after must be NUL padding. Structurally impossible
/// records are the typed [`MemoryError::TreeCorrupt`].
fn decode_record(record: &[u8], path: &Path) -> Result<String, JanitorError> {
    let corrupt = |detail: String| MemoryError::TreeCorrupt { detail };
    let text_end = record.iter().position(|b| *b == 0).unwrap_or(RECORD_WIDTH);
    if record[text_end..].iter().any(|b| *b != 0) {
        return Err(corrupt(format!(
            "record with garbage after its NUL padding in {}",
            path.display()
        ))
        .into());
    }
    let text = std::str::from_utf8(&record[..text_end])
        .map_err(|_| corrupt(format!("record is not valid UTF-8 in {}", path.display())))?;
    Ok(text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::log::MemoryFlag;

    fn store_in(tmp: &Path) -> TreeStore {
        let root = tmp.join(crate::memory::LOCAL_DIR);
        std::fs::create_dir_all(&root).expect("root");
        TreeStore::new(&root)
    }

    fn facts(n: u64, locked: &[u64]) -> Vec<FactRecord> {
        (0..n)
            .map(|id| FactRecord {
                v: 1,
                id,
                ts: "2026-09-14T00:00:00Z".to_owned(),
                ptr: None,
                text: format!("fact {id}"),
                flags: if locked.contains(&id) {
                    vec![MemoryFlag::ArchitecturalLock]
                } else {
                    Vec::new()
                },
            })
            .collect()
    }

    /// Slot notation: parse/round-trip, and the malformed refusals.
    #[test]
    fn slot_notation_parses_and_refuses() {
        let slot = BlockSlot::parse("4-8").expect("valid");
        assert_eq!(slot, BlockSlot { size: 4, lo: 4 });
        assert_eq!(slot.notation(), "4-8");
        assert_eq!(
            slot.children(),
            [BlockSlot { size: 2, lo: 4 }, BlockSlot { size: 2, lo: 6 }]
        );

        for bad in ["0-1", "0-3", "8-4", "0-x", "04", "2-2"] {
            let err: JanitorError = BlockSlot::parse(bad).unwrap_err().into();
            assert_eq!(err.code(), "E_SLOT_INVALID", "{bad:?}");
        }
    }

    /// The plan is a function of the log: levels, compressibility (locks
    /// excluded), and rank skips unwritable slots.
    #[test]
    fn plan_derives_levels_compressibility_and_rank_from_the_log() {
        let log = facts(8, &[5]); // fact 5 is locked
        let plan = BlockPlan::new(&log);
        assert_eq!(plan.levels(), vec![2, 4, 8]);

        // Lock-free aligned blocks are compressible; any block containing
        // fact 5 is not.
        assert!(plan.is_compressible(&log, 2, 0));
        assert!(!plan.is_compressible(&log, 8, 0)); // contains 5
        assert!(!plan.is_compressible(&log, 2, 4)); // 4-6 contains 5
        assert!(plan.is_compressible(&log, 2, 6));
        assert!(!plan.is_compressible(&log, 2, 8)); // out of range
        assert!(!plan.is_compressible(&log, 3, 0)); // not a power of two
        assert!(!plan.is_compressible(&log, 2, 1)); // unaligned

        // Ranks skip the unwritable: TREE/2 slots are 0,2,6 (4 excluded).
        assert_eq!(
            plan.compressible_slots(&log, 2),
            vec![0, 2, 6],
            "the slot containing the lock is not part of the level's file"
        );
        assert_eq!(plan.rank(&log, 2, 6), Some(2));
        assert_eq!(plan.rank(&log, 2, 4), None);
    }

    /// The settle/load round trip: exact 256-byte records, NUL padding,
    /// O(1)-shaped files (len == records × width).
    #[test]
    fn settle_and_load_round_trip_at_fixed_width() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let log = facts(4, &[]);

        let out = store
            .settle(&log, 2, 0, "facts 0-1: the overlay invariants")
            .expect("settle 0-1");
        assert!(!out.idempotent);
        store
            .settle(&log, 2, 2, "facts 2-3: the txn flush")
            .expect("settle 2-3");

        let path = store.file(2);
        let raw = std::fs::read(&path).expect("file");
        assert_eq!(raw.len(), 2 * RECORD_WIDTH);
        assert!(raw.starts_with(b"facts 0-1: the overlay invariants"));
        assert_eq!(raw[RECORD_WIDTH - 1], 0, "NUL padding");

        let (records, warnings) = store.load(2).expect("load").expect("level file");
        assert_eq!(records.len(), 2);
        assert!(warnings.is_empty());
        assert_eq!(records[0], "facts 0-1: the overlay invariants");
        assert_eq!(
            store.summary(&log, 2, 0).expect("summary"),
            Some("facts 0-1: the overlay invariants".to_owned())
        );
        assert_eq!(store.summary(&log, 4, 0).expect("unsettled level"), None);
    }

    /// Dense prefix: rank 1 before rank 0 is out of order; parents need
    /// children; idempotent resubmission is a no-op, different text a
    /// refusal.
    #[test]
    fn settle_enforces_order_children_and_idempotency() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let log = facts(4, &[]);

        // Children first: TREE/4 rank 0 needs both TREE/2 halves.
        let err = store
            .settle(&log, 4, 0, "premature parent")
            .expect_err("children missing");
        assert_eq!(err.code(), "E_SLOT_INVALID");
        assert!(err.to_string().contains("child"));

        // Out of order: slot 2-4 (rank 1) before 0-2 (rank 0).
        let err = store
            .settle(&log, 2, 2, "second first")
            .expect_err("out of order");
        assert_eq!(err.code(), "E_SLOT_INVALID");
        assert!(err.to_string().contains("out of order"));

        store.settle(&log, 2, 0, "first half").expect("settle 0-2");
        store.settle(&log, 2, 2, "second half").expect("settle 2-4");
        let parent = store
            .settle(&log, 4, 0, "both halves: the overlay story")
            .expect("parent now legal");
        assert!(!parent.idempotent);

        // Idempotent: identical text is a no-op; different text refuses.
        let again = store
            .settle(&log, 4, 0, "both halves: the overlay story")
            .expect("idempotent");
        assert!(again.idempotent);
        let err = store
            .settle(&log, 4, 0, "a different story")
            .expect_err("divergent resubmission");
        assert_eq!(err.code(), "E_SLOT_INVALID");

        let (_, _) = store.load(4).expect("load").expect("file");
        let (records, _) = store.load(4).expect("reload").expect("file");
        assert_eq!(records, vec!["both halves: the overlay story".to_owned()]);
    }

    /// Locks never enter compression: a slot containing one is refused
    /// with the fact named, and the level's ordering skips past it.
    #[test]
    fn settle_refuses_lock_ranges_and_ranks_skip_them() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let log = facts(4, &[1]); // 0-2 contains the lock

        let err = store
            .settle(&log, 2, 0, "would swallow the lock")
            .expect_err("locked range");
        assert_eq!(err.code(), "E_SLOT_INVALID");
        assert!(err.to_string().contains("architectural_lock fact 1"));

        // 2-4 is rank 0 (the locked 0-2 is not part of this level's file),
        // so it settles immediately — the lock does not stall the frontier.
        store
            .settle(&log, 2, 2, "the lock-free tail")
            .expect("rank 0");
        assert_eq!(
            store.summary(&log, 2, 2).expect("summary"),
            Some("the lock-free tail".to_owned())
        );
    }

    /// The summary text discipline: empty, multi-line, NUL, over-width.
    #[test]
    fn summary_text_discipline_is_typed() {
        let slot = BlockSlot { size: 2, lo: 0 };
        for bad in ["", "two\nlines", "nul\0inside"] {
            assert!(validate_summary_text(&slot, bad).is_err(), "{bad:?}");
        }
        let over = "s".repeat(MAX_SUMMARY_BYTES + 1);
        assert!(validate_summary_text(&slot, &over).is_err());
        validate_summary_text(&slot, &"s".repeat(MAX_SUMMARY_BYTES)).expect("at the limit");
        validate_summary_text(&slot, "one line").expect("ordinary");
    }

    /// A torn trailing record is trimmed with a warning and the slot
    /// becomes pending again; garbage after the NUL padding is the typed
    /// E_TREE_CORRUPT.
    #[test]
    fn torn_record_trims_and_padding_garbage_refuses() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let log = facts(2, &[]);
        store
            .settle(&log, 2, 0, "the only settled block")
            .expect("settle");

        // Torn tail: 100 stray bytes.
        let path = store.file(2);
        let mut raw = std::fs::read(&path).expect("read");
        raw.extend_from_slice(&[b'x'; 100]);
        std::fs::write(&path, &raw).expect("write torn");
        let (records, warnings) = store.load(2).expect("load").expect("file");
        assert_eq!(records.len(), 1, "the whole record survives");
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].kind, "tree_torn_record_trimmed");
        assert_eq!(std::fs::read(&path).expect("trimmed").len(), RECORD_WIDTH);

        // Padding garbage: text, then a non-NUL byte after the first NUL.
        let mut corrupt = vec![b'a'; RECORD_WIDTH];
        corrupt[4] = 0;
        corrupt[200] = b'!';
        std::fs::write(&path, &corrupt).expect("write corrupt");
        let err = store.load(2).expect_err("corrupt record");
        assert_eq!(err.code(), "E_TREE_CORRUPT");
    }

    /// TREE is a pure cache: delete it, and the same slots are settle-able
    /// again with identical ranks (rebuildable from LOG.txt alone).
    #[test]
    fn tree_is_rebuildable_from_the_log_alone() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let store = store_in(tmp.path());
        let log = facts(4, &[]);
        store.settle(&log, 2, 0, "first").expect("settle");
        store.settle(&log, 2, 2, "second").expect("settle");

        std::fs::remove_dir_all(store.dir()).expect("delete TREE");
        assert_eq!(store.summary(&log, 2, 0).expect("gone"), None);

        // Same ranks, same order — the plan came from the log, not the file.
        store
            .settle(&log, 2, 0, "first again")
            .expect("rank 0 rebuilds first");
        store
            .settle(&log, 2, 2, "second again")
            .expect("then rank 1");
        assert_eq!(
            store.summary(&log, 2, 2).expect("restored"),
            Some("second again".to_owned())
        );
    }
}
