//! The flush phase (PLAN §1.3, §3 F5.4): commit the staged diff through
//! the vfs transaction — per-file tmp+rename, ordered by path, honest
//! partial-failure reporting.
//!
//! This is the second half of the two-phase boundary: [`super::validate`]
//! has already gated every file, so reaching this code means the whole
//! diff re-parses clean. Staging is a single sequential splice of all
//! prepared files into one [`Transaction`] (journaling every path's prior
//! entry and generation — rollback restores both, for free via the prior
//! `Arc`, ADR-0009); committing flushes each staged file to disk through
//! `Os::write_atomic` (tmp+rename). The unit of commit is the whole diff:
//!
//! - `--dry-run` rolls the transaction back — content and generations
//!   restored, disk never touched — after the full validate+gate ran
//!   (PLAN §3 F5.3);
//! - a partial OS failure mid-commit is the typed `E_FLUSH_PARTIAL` with
//!   the per-file outcome list (`persisted` / `rolled_back` /
//!   `failures`) — honest reporting, never a silent partial (true
//!   cross-file atomicity needs a journal; post-v0.1, PLAN §7).
//!
//! After a successful commit, a fresh `ParseEntry` is inserted per file
//! (ADR-0007: "a fresh entry is inserted after re-parse"): the staged
//! generation survives `commit` (it clears buffer entries but leaves
//! generations, precisely so cache pairs stay valid), and the disk now
//! holds the same bytes the entry indexes. After a dry-run rollback no
//! entry is inserted: the taken entry's tree was consumed by the
//! choreography (Rule T1: edited in place under exclusive ownership), so
//! the honest post-rollback cache is the one without it — the next scan
//! simply re-parses.

use std::sync::Arc;

use crate::apply::validate::PreparedFile;
use crate::ast::cache::{ParseCache, ParseEntry};
use crate::error::JanitorError;
use crate::vfs::{CommitReport, Overlay, Transaction, TxnOp};

/// How the flush phase ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlushOutcome {
    /// Every staged file persisted (the report lists them, ordered by
    /// path).
    Flushed(CommitReport),
    /// `--dry-run`: validated and gated, staged, then rolled back — disk
    /// never touched.
    DryRun,
}

/// Stage every prepared file and finish the transaction. `prepared` must
/// already be path-sorted (the orchestrator sorts) — the journal order is
/// the flush order, and a deterministic order keeps generation numbering
/// and any partial-failure report a function of the input, not the thread
/// schedule.
pub fn flush(
    overlay: &Arc<Overlay>,
    cache: &ParseCache,
    prepared: Vec<PreparedFile>,
    dry_run: bool,
) -> Result<FlushOutcome, JanitorError> {
    let mut txn: Transaction = overlay.txn();
    txn.stage(prepared.iter().map(|file| TxnOp::Set {
        path: file.key.clone(),
        bytes: Arc::clone(&file.new_bytes),
    }))?;

    if dry_run {
        txn.rollback()?;
        return Ok(FlushOutcome::DryRun);
    }
    let report = txn.commit()?;

    // Fresh entries for the post-flush state (see module docs). The
    // entries consume the trees; the report facts were computed by the
    // orchestrator before this call.
    for file in prepared {
        let generation = overlay.current_gen(&file.key)?;
        cache.insert(
            file.key,
            ParseEntry::new(generation, file.new_bytes, file.tree),
        );
    }
    Ok(FlushOutcome::Flushed(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::diff::DiffPayload;
    use crate::apply::validate::validate_one;
    use crate::ast::lang::LangRegistry;
    use crate::vfs::Os;

    /// Two files staged and flushed: disk holds the new bytes, the buffer
    /// is cleared, and fresh cache entries are valid for the post-flush
    /// (generation, disk bytes) pair.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn flush_persists_both_files_and_inserts_entries() {
        let tmp = tempfile::tempdir().expect("tempdir");
        for name in ["a.rs", "b.rs"] {
            std::fs::write(tmp.path().join(name), "fn one() -> u32 { 1 }\n").expect("write");
        }
        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let cache = ParseCache::new();
        let registry = LangRegistry::new();
        let mut worker = None;

        let mut prepared = Vec::new();
        for name in ["a.rs", "b.rs"] {
            let path = tmp.path().join(name);
            let source = std::fs::read(&path).expect("read");
            let at = source
                .windows(3)
                .position(|w| w == b"one")
                .unwrap_or_default();
            let payload = DiffPayload::from_json_str(&format!(
                r#"{{"operations":[{{"path":"{name}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"two"}}]}}]}}"#,
                at + 3
            ))
            .expect("payload");
            let key = crate::vfs::VfsPath::new(&path).expect("key");
            prepared.push(
                validate_one(
                    &overlay,
                    &cache,
                    &registry,
                    &mut worker,
                    &payload.operations[0],
                    &key,
                )
                .expect("prepares"),
            );
        }

        let outcome = flush(&overlay, &cache, prepared, false).expect("flushes");
        let FlushOutcome::Flushed(report) = outcome else {
            unreachable!("asserted by construction");
        };
        assert_eq!(report.persisted.len(), 2);
        assert!(report.removed.is_empty());
        for name in ["a.rs", "b.rs"] {
            assert_eq!(
                std::fs::read(tmp.path().join(name)).expect("read"),
                b"fn two() -> u32 { 1 }\n",
                "{name} persisted"
            );
        }

        // Fresh entries: valid against the post-flush disk bytes.
        assert_eq!(cache.len(), 2);
        for name in ["a.rs", "b.rs"] {
            let key = crate::vfs::VfsPath::new(tmp.path().join(name)).expect("key");
            let bytes = overlay.read_file(&key).expect("disk bytes");
            let generation = overlay.current_gen(&key).expect("gen");
            let entry = cache
                .get(&key, generation, &bytes)
                .expect("entry valid post-flush");
            assert_eq!(entry.error_count, 0);
        }
    }

    /// Dry-run: full stage, then rollback — disk untouched, content and
    /// generations restored, no cache entry left behind.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn dry_run_rolls_back_content_and_generations() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("d.rs");
        std::fs::write(&path, "fn one() -> u32 { 1 }\n").expect("write");
        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let cache = ParseCache::new();
        let registry = LangRegistry::new();
        let mut worker = None;

        let gen_before = overlay.current_gen(&path).expect("gen");
        let source = std::fs::read(&path).expect("read");
        let at = source
            .windows(3)
            .position(|w| w == b"one")
            .unwrap_or_default();
        let payload = DiffPayload::from_json_str(&format!(
            r#"{{"operations":[{{"path":"d.rs","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"two"}}]}}]}}"#,
            at + 3
        ))
        .expect("payload");
        let key = crate::vfs::VfsPath::new(&path).expect("key");
        let prepared = vec![
            validate_one(
                &overlay,
                &cache,
                &registry,
                &mut worker,
                &payload.operations[0],
                &key,
            )
            .expect("prepares"),
        ];

        let outcome = flush(&overlay, &cache, prepared, true).expect("dry-run completes");
        assert_eq!(outcome, FlushOutcome::DryRun);
        assert_eq!(
            std::fs::read(&path).expect("disk untouched"),
            b"fn one() -> u32 { 1 }\n"
        );
        assert_eq!(
            overlay.read_file(&key).expect("content restored"),
            Arc::from(b"fn one() -> u32 { 1 }\n".as_slice())
        );
        assert_eq!(
            overlay.current_gen(&path).expect("gen restored"),
            gen_before
        );
        assert!(cache.is_empty(), "no entry after rollback");
    }
}
