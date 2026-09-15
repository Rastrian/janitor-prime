//! `apply` — the transactional rewrite loop (PLAN §1.2.1, §3 F5): stage →
//! validate → flush, the integration heart of the vertical slice.
//!
//! - `diff` — the ADR-0014 wire payload and its validation against the
//!   current snapshot bytes (F5.1);
//! - `splice` — the descending-by-offset multi-edit byte fold (F5.2);
//! - `validate` — the whole-file re-parse gate with the exclusivity-guarded
//!   choreography (F5.3);
//! - `flush` — the vfs-transaction commit with honest partial-failure
//!   reporting (F5.4);
//! - [`apply`] (this file) — the two-phase orchestration (F5.5).
//!
//! # The transaction is the whole diff
//!
//! Phase A (rayon, files independent — embarrassingly parallel, per-file
//! owned snapshots): each file runs §1.2.1 steps 1–7 —
//! validate payload → splice → **take** the cache entry → guarded edit →
//! re-parse → gate — producing an owned [`validate::PreparedFile`] or a
//! typed error. Nothing is staged and nothing is flushed in phase A, so a
//! failure anywhere leaves the overlay, the cache footprint, and every
//! other file untouched.
//!
//! Phase B (sequential): if every file prepared, the whole set stages
//! into one [`Transaction`] and flushes — ordered by path, per-file
//! tmp+rename — or, under `--dry-run`, rolls back (content and
//! generations restored, disk never touched). **All-or-nothing at the
//! granularity of the entire payload**: one failure ⇒ full rollback,
//! nothing flushed. Staging is deferred to phase B deliberately: the
//! buffer splice is pure bookkeeping once the gates have passed, and one
//! sequential stage keeps the journal — and therefore the generation
//! numbering — deterministic, independent of the rayon schedule (the
//! observable per-file state is identical to PLAN's per-file staging
//! order; only the moment of the buffer splice moves, to after the last
//! gate).
//!
//! Determinism (ADR-0016's spirit, applied to apply): files processed,
//! staged, and reported in path order; the report is a function of the
//! input alone — `stats.wall_ms` is the one field allowed to vary, and
//! the e2e suite fences byte-identical JSON across `RAYON_NUM_THREADS`
//! 1 vs 8.

pub mod diff;
pub mod flush;
pub mod splice;
pub mod validate;

pub use diff::DiffPayload;

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use serde::Serialize;

use crate::apply::diff::content_sha;
use crate::apply::flush::flush;
use crate::apply::validate::validate_one;
use crate::ast::cache::ParseCache;
use crate::ast::lang::LangRegistry;
use crate::error::{EditError, JanitorError};
use crate::vfs::{Overlay, VfsPath};

/// Everything `apply` needs besides the session's own state.
#[derive(Clone, Debug)]
pub struct ApplyParams {
    /// The parsed whole-diff payload (ADR-0014).
    pub payload: DiffPayload,
    /// Validate + gate + stage, then roll back: report only, no flush.
    pub dry_run: bool,
}

/// One file's outcome, report-shaped (`docs/SCHEMA.md`).
#[derive(Clone, Debug, Serialize)]
pub struct FileResult {
    /// The payload's path spelling, echoed verbatim.
    pub path: String,
    /// `"flushed"` after a real apply; `"rolled_back"` under `--dry-run`.
    pub status: &'static str,
    pub edits: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// [`content_sha`] of the pre-edit content — a natural `expect_sha`
    /// for a follow-up apply.
    pub sha_before: String,
    pub sha_after: String,
    /// Whole-file ERROR/MISSING count before the edit (informational: the
    /// gate judges only the result).
    pub errors_before: u32,
    /// Always 0 on success — that is the gate.
    pub errors_after: u32,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyStats {
    pub files: u64,
    pub edits: u64,
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// Wall-clock time — the only non-deterministic field.
    pub wall_ms: u64,
}

/// The `janitor apply` report payload (the `data` of the janitor.v1
/// envelope; `docs/SCHEMA.md` documents the exact shape).
#[derive(Clone, Debug, Serialize)]
pub struct ApplyReport {
    pub dry_run: bool,
    /// Sorted by path.
    pub files: Vec<FileResult>,
    pub stats: ApplyStats,
}

/// One phase-A work item: the canonical key plus the payload operation.
struct Job {
    key: VfsPath,
    op: diff::FileEdit,
}

/// Run the whole transaction. See the module docs for the two-phase
/// design; every failure is a typed [`JanitorError`] naming the offending
/// file, and by construction nothing was flushed when one is returned.
pub fn apply(
    overlay: &Arc<Overlay>,
    cache: &ParseCache,
    registry: &LangRegistry,
    params: &ApplyParams,
) -> Result<ApplyReport, JanitorError> {
    let started = Instant::now();

    // Resolve and canonically order the work set. Sorting by the
    // canonical key makes processing, staging, and the report functions
    // of the input alone; different spellings of one file (a genuine
    // payload contradiction) are refused rather than interleaved.
    let mut jobs: Vec<Job> = Vec::with_capacity(params.payload.operations.len());
    for op in &params.payload.operations {
        jobs.push(Job {
            key: resolve_key(&op.path)?,
            op: op.clone(),
        });
    }
    jobs.sort_by(|a, b| a.key.cmp(&b.key));
    for pair in jobs.windows(2) {
        if pair[0].key == pair[1].key {
            return Err(JanitorError::Edit(EditError::Payload {
                detail: format!(
                    "paths {:?} and {:?} canonicalize to the same file",
                    pair[0].op.path, pair[1].op.path
                ),
            }));
        }
    }

    // Phase A — validate + gate every file, in parallel. `collect`
    // preserves the (path-sorted) input order, so the first failure below
    // is deterministic.
    let outcomes: Vec<Result<validate::PreparedFile, JanitorError>> = jobs
        .into_par_iter()
        .map_init(
            || None::<(crate::ast::lang::LangId, crate::ast::ffi_shim::Parser)>,
            |worker_parser, job| {
                validate_one(overlay, cache, registry, worker_parser, &job.op, &job.key)
            },
        )
        .collect();

    let mut prepared: Vec<validate::PreparedFile> = Vec::with_capacity(outcomes.len());
    let mut first_failure: Option<JanitorError> = None;
    for outcome in outcomes {
        match outcome {
            Ok(file) => prepared.push(file),
            // Every file still runs to completion (workers are
            // independent); the first error in path order is THE error.
            Err(err) => {
                first_failure.get_or_insert(err);
            }
        }
    }
    if let Some(err) = first_failure {
        return Err(err);
    }

    // Report facts, computed before the flush consumes the trees.
    let mut files = Vec::with_capacity(prepared.len());
    let mut stats = ApplyStats::default();
    for file in &prepared {
        stats.files += 1;
        stats.edits += file.edits as u64;
        stats.bytes_before += file.bytes_before as u64;
        stats.bytes_after += file.new_bytes.len() as u64;
        files.push(FileResult {
            path: file.display.clone(),
            status: "flushed",
            edits: file.edits as u64,
            bytes_before: file.bytes_before as u64,
            bytes_after: file.new_bytes.len() as u64,
            sha_before: file.sha_before.clone(),
            sha_after: content_sha(&file.new_bytes),
            errors_before: file.errors_before,
            // The gate passed for this file to be here at all.
            errors_after: 0,
        });
    }

    // Phase B — stage the whole diff and finish the transaction.
    let dry_run = params.dry_run;
    let outcome = flush(overlay, cache, prepared, dry_run)?;
    let status = match outcome {
        crate::apply::flush::FlushOutcome::Flushed(_) => "flushed",
        crate::apply::flush::FlushOutcome::DryRun => "rolled_back",
    };
    for result in &mut files {
        result.status = status;
    }

    stats.wall_ms = started.elapsed().as_millis() as u64;
    Ok(ApplyReport {
        dry_run,
        files,
        stats,
    })
}

/// Resolve the payload's path spelling to a canonical buffer key:
/// absolute as written, relative against the process working directory
/// (the agent's frame), then lexically canonicalized by [`VfsPath`]
/// (symlinks deliberately not resolved — the overlay's contract).
fn resolve_key(spelled: &str) -> Result<VfsPath, JanitorError> {
    let raw = Path::new(spelled);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| std::path::PathBuf::from("."))
            .join(raw)
    };
    Ok(VfsPath::new(absolute)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{EditError, JanitorError, ValidateError};
    use crate::orchestrator::Session;

    /// A two-file corpus with a shared identifier, for rename flows.
    fn corpus() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(tmp.path().join("src")).expect("dirs");
        std::fs::write(
            tmp.path().join("src/lib.rs"),
            "pub fn alpha(x: u32) -> u32 {\n    x + 1\n}\n",
        )
        .expect("write");
        std::fs::write(
            tmp.path().join("src/main.rs"),
            "mod lib;\nfn main() {\n    let v = lib::alpha(2);\n    println!(\"{v}\");\n}\n",
        )
        .expect("write");
        tmp
    }

    fn rename_payload(tmp: &tempfile::TempDir) -> String {
        let lib_path = tmp.path().join("src/lib.rs").display().to_string();
        let main_path = tmp.path().join("src/main.rs").display().to_string();
        let lib = std::fs::read(&lib_path).expect("read");
        let main = std::fs::read(&main_path).expect("read");
        let lib_at = lib
            .windows(5)
            .position(|w| w == b"alpha")
            .unwrap_or_default();
        let main_at = main
            .windows(5)
            .position(|w| w == b"alpha")
            .unwrap_or_default();
        format!(
            r#"{{"operations":[
                {{"path":"{lib_path}","edits":[{{"start_byte":{lib_at},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}},
                {{"path":"{main_path}","edits":[{{"start_byte":{main_at},"old_end_byte":{},"new_text":"beta","old_text":"alpha"}}]}}
            ]}}"#,
            lib_at + 5,
            main_at + 5,
        )
    }

    /// The golden flow: a rename across two files in one transaction —
    /// both flushed, sorted by path, stats and shas coherent.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn rename_across_two_files_flushes_atomically() {
        let tmp = corpus();
        let session = Session::new();
        let params = ApplyParams {
            payload: DiffPayload::from_json_str(&rename_payload(&tmp)).expect("payload"),
            dry_run: false,
        };
        let report = session.apply(&params).expect("applies");
        assert!(!report.dry_run);
        assert_eq!(report.files.len(), 2);
        assert_eq!(
            report.files[0].path,
            tmp.path().join("src/lib.rs").display().to_string()
        );
        assert_eq!(
            report.files[1].path,
            tmp.path().join("src/main.rs").display().to_string()
        );
        assert!(
            report
                .files
                .iter()
                .all(|f| f.status == "flushed" && f.errors_after == 0)
        );
        assert_eq!(report.stats.files, 2);
        assert_eq!(report.stats.edits, 2);
        // alpha → beta: one byte shorter per file.
        assert_eq!(report.stats.bytes_after, report.stats.bytes_before - 2);
        assert!(
            String::from_utf8(std::fs::read(tmp.path().join("src/lib.rs")).expect("read"))
                .expect("utf-8")
                .contains("pub fn beta(x: u32)")
        );
        assert!(
            String::from_utf8(std::fs::read(tmp.path().join("src/main.rs")).expect("read"))
                .expect("utf-8")
                .contains("lib::beta(2)")
        );
        // The inserted fresh entries are valid for the post-flush state.
        assert_eq!(session.cache().len(), 2);
    }

    /// All-or-nothing: one file's `old_text` lying about the content
    /// fails the whole diff — the other file's valid edit is NOT flushed,
    /// and both disks stay byte-identical to before (the rollback proof,
    /// trivially: nothing was ever staged).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn one_bad_file_rolls_back_the_whole_diff() {
        let tmp = corpus();
        let before_lib = std::fs::read(tmp.path().join("src/lib.rs")).expect("read");
        let before_main = std::fs::read(tmp.path().join("src/main.rs")).expect("read");

        let mut json = rename_payload(&tmp);
        // Corrupt the second operation's edit-level guard: wrong old_text.
        let pos = json.rfind("\"old_text\":\"alpha\"").unwrap_or_default();
        json.replace_range(
            pos..pos + "\"old_text\":\"alpha\"".len(),
            "\"old_text\":\"omega\"",
        );

        let session = Session::new();
        let params = ApplyParams {
            payload: DiffPayload::from_json_str(&json).expect("payload"),
            dry_run: false,
        };
        let err = session.apply(&params).expect_err("the lying edit fails");
        assert!(
            matches!(err, JanitorError::Edit(EditError::Mismatch { .. })),
            "got {err:?}"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("src/lib.rs")).expect("read"),
            before_lib,
            "the valid file is not flushed either"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("src/main.rs")).expect("read"),
            before_main
        );
    }

    /// The gate refusal is whole-diff too: an edit that breaks the syntax
    /// of one file yields `E_PARSE_INVALID` with coordinates, and disk is
    /// untouched for every file.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn syntax_gate_failure_rolls_back_everything_with_coordinates() {
        let tmp = corpus();
        let before_lib = std::fs::read(tmp.path().join("src/lib.rs")).expect("read");
        let before_main = std::fs::read(tmp.path().join("src/main.rs")).expect("read");

        let main = std::fs::read(tmp.path().join("src/main.rs")).expect("read");
        let at = main
            .windows(5)
            .position(|w| w == b"alpha")
            .unwrap_or_default();
        let lib_path = tmp.path().join("src/lib.rs").display().to_string();
        let main_path = tmp.path().join("src/main.rs").display().to_string();
        let json = format!(
            r#"{{"operations":[
                {{"path":"{lib_path}","edits":[{{"start_byte":0,"old_end_byte":0,"new_text":"// prep\n"}}]}},
                {{"path":"{main_path}","edits":[{{"start_byte":{at},"old_end_byte":{},"new_text":"alpha (broken"}}]}}
            ]}}"#,
            at + 5,
        );

        let session = Session::new();
        let params = ApplyParams {
            payload: DiffPayload::from_json_str(&json).expect("payload"),
            dry_run: false,
        };
        let err = session
            .apply(&params)
            .expect_err("the broken edit fails the gate");
        let JanitorError::Validate(ValidateError::ParseInvalid { nodes, .. }) = err else {
            unreachable!("the gate is the failing leg, got {err:?}");
        };
        assert!(!nodes.is_empty());
        assert_eq!(
            nodes[0]["path"],
            tmp.path().join("src/main.rs").display().to_string()
        );
        assert!(nodes[0]["start_byte"].as_u64().unwrap_or_default() > 0);
        assert_eq!(
            std::fs::read(tmp.path().join("src/lib.rs")).expect("read"),
            before_lib,
            "nothing flushed when the gate refuses"
        );
        assert_eq!(
            std::fs::read(tmp.path().join("src/main.rs")).expect("read"),
            before_main
        );
    }

    /// Dry-run through the session: validated, gated, reported — then
    /// rolled back; generations restored and disk untouched.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn dry_run_validates_and_restores() {
        let tmp = corpus();
        let before = std::fs::read(tmp.path().join("src/lib.rs")).expect("read");
        let session = Session::new();
        let payload = DiffPayload::from_json_str(&rename_payload(&tmp)).expect("payload");
        let params = ApplyParams {
            payload,
            dry_run: true,
        };
        let report = session.apply(&params).expect("dry-run applies");
        assert!(report.dry_run);
        assert!(report.files.iter().all(|f| f.status == "rolled_back"));
        assert_eq!(
            std::fs::read(tmp.path().join("src/lib.rs")).expect("read"),
            before,
            "disk untouched"
        );
        assert!(
            session.cache().is_empty(),
            "no entry after dry-run rollback"
        );
    }
}
