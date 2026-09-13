//! Transaction boundaries (PLAN §1.3, F2.6): `stage(ops) / commit() /
//! rollback()` around multi-file `apply`.
//!
//! Staging splices ops into the overlay buffer immediately (bumping
//! generations per Rule T4) while journaling each path's prior entry — the
//! prior `Arc` stays alive for rollback for free (ADR-0009). `commit()`
//! flushes to disk through [`Os`], ordered by path, per-file tmp+rename;
//! a staged delete flushes as a file removal. On partial OS failure the
//! failed paths are rolled back to their journaled prior state, the
//! persisted ones stay on disk, and the whole outcome is the typed
//! [`VfsError::FlushPartial`] report — honest reporting, never silent
//! partial state (true cross-file atomicity needs a journal; post-v0.1,
//! PLAN §7).
//!
//! A [`Transaction`] assumes it is the only mutator of its paths while open
//! (the apply loop owns staging); `commit` and `rollback` consume the
//! transaction, so double-finish is a compile error, not a runtime state.
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::vfs::error::{VfsError, VfsResult};
use crate::vfs::fs::Os;
use crate::vfs::overlay::{BufferEntry, Overlay};
use crate::vfs::path::VfsPath;

/// One staged operation.
#[derive(Clone, Debug)]
pub enum TxnOp {
    Set { path: VfsPath, bytes: Arc<[u8]> },
    Delete { path: VfsPath },
}

impl TxnOp {
    /// [`TxnOp::Set`] with the E6 UTF-8 precondition: invalid bytes are a
    /// typed error and build no op.
    pub fn set_string(path: VfsPath, text: impl AsRef<[u8]>) -> VfsResult<Self> {
        let bytes = text.as_ref();
        if std::str::from_utf8(bytes).is_err() {
            return Err(VfsError::InvalidUtf8 {
                path: path.to_string(),
            });
        }
        Ok(Self::Set {
            path,
            bytes: Arc::from(bytes),
        })
    }
}

/// What the journal remembers per touched path (first touch only): the
/// pre-transaction entry and generation to restore on rollback.
#[derive(Clone, Debug)]
struct JournalEntry {
    path: VfsPath,
    prior: Option<BufferEntry>,
    prior_gen: crate::vfs::overlay::Generation,
}

/// The per-file outcome of a flush that did not fully succeed — the
/// `E_FLUSH_PARTIAL` payload.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushReport {
    /// Staged files persisted to disk (tmp+rename).
    pub persisted: Vec<String>,
    /// Disk files removed by committed tombstones.
    pub removed: Vec<String>,
    /// Staged paths rolled back to their pre-transaction state.
    pub rolled_back: Vec<String>,
    /// The failures that made the flush partial, one per path.
    pub failures: Vec<FlushFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FlushFailure {
    pub path: String,
    pub error: String,
}

/// The outcome of a fully successful commit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitReport {
    pub persisted: Vec<String>,
    pub removed: Vec<String>,
}

pub struct Transaction {
    overlay: Arc<Overlay>,
    journal: Vec<JournalEntry>,
}

impl Overlay {
    /// Open a transaction over this overlay.
    pub fn txn(self: &Arc<Self>) -> Transaction {
        Transaction {
            overlay: Arc::clone(self),
            journal: Vec::new(),
        }
    }
}

impl Transaction {
    /// The paths this transaction has touched, canonically ordered.
    pub fn staged_paths(&self) -> Vec<VfsPath> {
        let mut paths: Vec<VfsPath> = self.journal.iter().map(|e| e.path.clone()).collect();
        paths.sort();
        paths
    }

    /// Splice `ops` into the overlay buffer, journaling prior state.
    /// All-or-nothing: if any op fails (e.g. a delete of an untracked path),
    /// the ops already applied in this call are unwound and the error is
    /// returned with the buffer exactly as it was.
    pub fn stage(&mut self, ops: impl IntoIterator<Item = TxnOp>) -> VfsResult<()> {
        let mut applied: Vec<JournalEntry> = Vec::new();
        for op in ops {
            if let Err(err) = self.apply_one(op, &mut applied) {
                for entry in applied.iter().rev() {
                    self.overlay
                        .restore(&entry.path, entry.prior.clone(), entry.prior_gen);
                }
                return Err(err);
            }
        }
        self.journal.extend(applied);
        Ok(())
    }

    fn apply_one(&self, op: TxnOp, applied: &mut Vec<JournalEntry>) -> VfsResult<()> {
        match op {
            TxnOp::Set { path, bytes } => {
                let (prior, prior_gen, _) = self.overlay.mutate(&path, BufferEntry::File(bytes));
                journal_touch(applied, &path, prior, prior_gen);
            }
            TxnOp::Delete { path } => {
                let (prior, prior_gen, _) = self.overlay.swap_delete(&path)?;
                journal_touch(applied, &path, prior, prior_gen);
            }
        }
        Ok(())
    }

    /// Flush the staged set to disk, ordered by path: `File` entries via
    /// tmp+rename, tombstones via removal. Success clears the flushed
    /// entries (disk now holds the same bytes; generations untouched, so
    /// cache pairs stay valid). Any failure rolls that path's entry back to
    /// its journaled prior state, keeps what already persisted, and returns
    /// [`VfsError::FlushPartial`] with the full report.
    pub fn commit(mut self) -> VfsResult<CommitReport> {
        self.journal.sort_by(|a, b| a.path.cmp(&b.path));
        let journal = &self.journal;

        let mut report = FlushReport::default();
        for entry in journal {
            let shown = entry.path.to_string();
            match self.overlay.buffer_entry(&entry.path) {
                Some(BufferEntry::File(bytes)) => {
                    match Os.write_atomic(entry.path.as_path(), &bytes) {
                        Ok(()) => {
                            self.overlay.clear_entry(&entry.path);
                            report.persisted.push(shown);
                        }
                        Err(err) => self.record_failure(&mut report, entry, shown, err),
                    }
                }
                Some(BufferEntry::Tombstone) => match Os.remove_file(entry.path.as_path()) {
                    Ok(()) => {
                        self.overlay.clear_entry(&entry.path);
                        report.removed.push(shown);
                    }
                    Err(err) => self.record_failure(&mut report, entry, shown, err),
                },
                // The path's staged entry was consumed outside this
                // transaction; there is nothing here to flush.
                None => {}
            }
        }

        if report.failures.is_empty() {
            Ok(CommitReport {
                persisted: report.persisted,
                removed: report.removed,
            })
        } else {
            Err(VfsError::FlushPartial { report })
        }
    }

    /// Rewind the whole transaction: every journaled path gets its prior
    /// entry and prior generation back. Disk is never touched.
    pub fn rollback(self) -> VfsResult<()> {
        for entry in self.journal.iter().rev() {
            self.overlay
                .restore(&entry.path, entry.prior.clone(), entry.prior_gen);
        }
        Ok(())
    }

    fn record_failure(
        &self,
        report: &mut FlushReport,
        entry: &JournalEntry,
        shown: String,
        err: VfsError,
    ) {
        report.failures.push(FlushFailure {
            path: shown.clone(),
            error: err.to_string(),
        });
        report.rolled_back.push(shown);
        self.overlay
            .restore(&entry.path, entry.prior.clone(), entry.prior_gen);
    }
}

/// Journal only the first touch of a path in a transaction: later ops on the
/// same path restore to the *pre-transaction* state, not to each other.
fn journal_touch(
    applied: &mut Vec<JournalEntry>,
    path: &VfsPath,
    prior: Option<BufferEntry>,
    prior_gen: crate::vfs::overlay::Generation,
) {
    if !applied.iter().any(|e| e.path == *path) {
        applied.push(JournalEntry {
            path: path.clone(),
            prior,
            prior_gen,
        });
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::sync::Arc;

    use tempfile::TempDir;

    use super::{CommitReport, FlushReport, Transaction, TxnOp};
    use crate::vfs::error::VfsError;
    use crate::vfs::fs::Os;
    use crate::vfs::overlay::Overlay;
    use crate::vfs::path::VfsPath;

    fn set_op(path: &Path, bytes: &[u8]) -> TxnOp {
        TxnOp::Set {
            path: VfsPath::new(path).unwrap(),
            bytes: Arc::from(bytes),
        }
    }

    fn delete_op(path: &Path) -> TxnOp {
        TxnOp::Delete {
            path: VfsPath::new(path).unwrap(),
        }
    }

    /// The mandated two-file commit: staged content is visible to reads
    /// before commit (and hidden from listings — anti-phantom), then commit
    /// persists both files, clears the buffer, and the post-commit view is
    /// the disk truth, listing included.
    #[test]
    fn two_file_commit_persists_and_clears_the_buffer() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir(&src).unwrap();
        let a = src.join("a.rs");
        let b = src.join("b.rs");

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let mut txn = overlay.txn();
        txn.stage([set_op(&a, b"staged a"), set_op(&b, b"staged b")])
            .unwrap();

        // Pre-commit: read-through serves staged bytes; listings stay disk.
        assert_eq!(overlay.read_file(&a).unwrap().as_ref(), b"staged a");
        assert!(!overlay.read_dir(&src).unwrap().contains("a.rs"));

        let report = txn.commit().unwrap();
        assert_eq!(
            report,
            CommitReport {
                persisted: vec![a.display().to_string(), b.display().to_string()],
                removed: vec![],
            }
        );

        // Post-commit: buffer cleared (pure lookup is empty), reads fall
        // through to disk and return the same bytes, listings now show the
        // files — they are real.
        assert!(overlay.get(&a).unwrap().is_none());
        assert_eq!(overlay.read_file(&a).unwrap().as_ref(), b"staged a");
        assert_eq!(overlay.read_file(&b).unwrap().as_ref(), b"staged b");
        let listing = overlay.read_dir(&src).unwrap();
        assert!(listing.contains("a.rs") && listing.contains("b.rs"));
    }

    /// Committing a tombstone removes the disk file: post-commit the path
    /// is a base-level `NotFound` and the listing no longer shows it.
    #[test]
    fn tombstone_commit_removes_the_disk_file() {
        let tmp = TempDir::new().unwrap();
        let victim = tmp.path().join("victim.rs");
        fs::write(&victim, b"old content").unwrap();

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let mut txn = overlay.txn();
        txn.stage([set_op(&victim, b"replacement"), delete_op(&victim)])
            .unwrap();
        // The staged delete won inside the transaction.
        assert!(matches!(
            overlay.read_file(&victim),
            Err(VfsError::BufferDeleted { .. })
        ));

        let report = txn.commit().unwrap();
        assert_eq!(report.removed, vec![victim.display().to_string()]);
        assert!(report.persisted.is_empty());

        assert!(matches!(
            overlay.read_file(&victim),
            Err(VfsError::NotFound { .. })
        ));
        assert!(!overlay.read_dir(tmp.path()).unwrap().contains("victim.rs"));
        assert!(!victim.exists());
    }

    /// Flush-failure injection (read-only dir): one file flushes, the other
    /// cannot, and the typed `E_FLUSH_PARTIAL` report says exactly which is
    /// which — the rolled-back path's buffer returns to its pre-txn state
    /// while the persisted file stays on disk.
    #[test]
    fn flush_failure_injection_reports_partial_outcomes() {
        // Root ignores directory read-only bits, which would make the
        // injection a no-op: probe the real precondition (can we write into
        // a 0o555 dir?) and skip honestly when running as root.
        let probe = TempDir::new().unwrap();
        fs::set_permissions(probe.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let writable_as_root = fs::File::create(probe.path().join("probe")).is_ok();
        fs::set_permissions(probe.path(), fs::Permissions::from_mode(0o755)).unwrap();

        let tmp = TempDir::new().unwrap();
        let ok_dir = tmp.path().join("a_ok");
        let ro_dir = tmp.path().join("z_ro");
        fs::create_dir(&ok_dir).unwrap();
        fs::create_dir(&ro_dir).unwrap();
        let ok_file = ok_dir.join("keep.rs");
        let ro_file = ro_dir.join("fail.rs");
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o555)).unwrap();
        if writable_as_root {
            fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("running as root: read-only injection unavailable, skipping");
            return;
        }

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let mut txn = overlay.txn();
        txn.stage([set_op(&ok_file, b"flushes"), set_op(&ro_file, b"fails")])
            .unwrap();

        let err = txn.commit().unwrap_err();
        fs::set_permissions(&ro_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            matches!(err, VfsError::FlushPartial { .. }),
            "expected FlushPartial from a read-only injection"
        );
        let report = match err {
            VfsError::FlushPartial { report } => report,
            // Dead arm: the assert above pins the shape.
            _ => FlushReport::default(),
        };
        assert_eq!(report.persisted, vec![ok_file.display().to_string()]);
        assert_eq!(report.rolled_back, vec![ro_file.display().to_string()]);
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].path, ro_file.display().to_string());

        // Disk truth: the writable one persisted, the read-only one did not.
        assert_eq!(fs::read(&ok_file).unwrap(), b"flushes");
        assert!(!ro_file.exists());
        // Buffer truth: the failed path rolled back to absent.
        assert!(overlay.get(&ro_file).unwrap().is_none());
    }

    /// Rollback restores both the entry and the generation (F2.6): the
    /// pre-txn staged bytes and their T4 key come back exactly, a path that
    /// had no prior state vanishes, and disk is never touched.
    #[test]
    fn rollback_restores_entries_and_generations() {
        let tmp = TempDir::new().unwrap();
        let kept = tmp.path().join("kept.rs");
        let fresh = tmp.path().join("fresh.rs");
        fs::write(&kept, b"disk old").unwrap();

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let gen_before = overlay.set(&kept, b"staged one").unwrap();

        let mut txn = overlay.txn();
        txn.stage([set_op(&kept, b"staged two"), set_op(&fresh, b"new")])
            .unwrap();
        assert_eq!(overlay.read_file(&kept).unwrap().as_ref(), b"staged two");
        assert_eq!(overlay.read_file(&fresh).unwrap().as_ref(), b"new");

        txn.rollback().unwrap();

        assert_eq!(overlay.read_file(&kept).unwrap().as_ref(), b"staged one");
        assert_eq!(overlay.current_gen(&kept).unwrap(), gen_before);
        assert!(overlay.get(&fresh).unwrap().is_none());
        assert_eq!(overlay.current_gen(&fresh).unwrap().value(), 0);
        // Disk was never touched by staging or rollback.
        assert_eq!(fs::read(&kept).unwrap(), b"disk old");
        assert!(!fresh.exists());
    }

    /// Staging is all-or-nothing: an op that fails mid-stage (delete of an
    /// untracked path) unwinds everything this call applied — buffer and
    /// generations — and returns the typed error.
    #[test]
    fn failing_stage_unwinds_its_own_ops() {
        let tmp = TempDir::new().unwrap();
        let ok = tmp.path().join("ok.rs");
        let untracked = tmp.path().join("untracked.rs");

        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let mut txn = overlay.txn();
        let err = txn
            .stage([set_op(&ok, b"applied first"), delete_op(&untracked)])
            .unwrap_err();
        assert!(matches!(err, VfsError::NotStaged { .. }));

        assert!(overlay.get(&ok).unwrap().is_none());
        assert_eq!(overlay.current_gen(&ok).unwrap().value(), 0);
        assert!(txn.staged_paths().is_empty());
    }

    /// E6 at the op level: `TxnOp::set_string` builds no op from invalid
    /// UTF-8.
    #[test]
    fn txn_set_string_validates_utf8() {
        let path = VfsPath::new("/proj/src/bad.rs").unwrap();
        assert!(TxnOp::set_string(path.clone(), "fine").is_ok());
        assert!(matches!(
            TxnOp::set_string(path, [0xff, 0xfe]),
            Err(VfsError::InvalidUtf8 { .. })
        ));
    }

    /// The report shape serializes for the envelope's `details` body.
    #[test]
    fn flush_report_serializes() {
        let report = FlushReport {
            persisted: vec!["/a.rs".to_owned()],
            removed: vec![],
            rolled_back: vec!["/b.rs".to_owned()],
            failures: vec![super::FlushFailure {
                path: "/b.rs".to_owned(),
                error: "permission denied: /b.rs".to_owned(),
            }],
        };
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["persisted"][0], "/a.rs");
        assert_eq!(json["rolled_back"][0], "/b.rs");
        assert_eq!(json["failures"][0]["path"], "/b.rs");
    }

    /// `Transaction` is single-shot by construction: commit and rollback
    /// consume it (compile-time guarantee; this test just exercises both
    /// paths through the type).
    #[test]
    fn transaction_is_single_shot_by_move() {
        fn consume(txn: Transaction) {
            txn.rollback().unwrap();
        }
        let overlay = Arc::new(Overlay::new(Arc::new(Os)));
        let txn: Transaction = overlay.txn();
        consume(txn);
    }
}
