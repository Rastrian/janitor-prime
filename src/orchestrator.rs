//! Composition root (PLAN §1.6) — the only place the three domains meet.
//!
//! [`Session`] owns the `Arc<Overlay>`, the [`ParseCache`], the
//! feature-gated [`LangRegistry`], and (since F6) the memory layer's
//! [`ContextManager`] rooted at the process working directory; one session
//! per CLI invocation (stateless across invocations in v0.1, ADR-0007).
//! `scan`/`apply` also record their last-run status into `.janitor/`
//! STATE.json when the local layer exists (advisory — ADR-0016 (8)).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::apply::{self, ApplyParams, ApplyReport};
use crate::ast::cache::ParseCache;
use crate::ast::lang::LangRegistry;
use crate::error::JanitorError;
use crate::memory::context::{AnchorMeta, ContextManager, InitReport, NoteReport, WakePayload};
use crate::memory::nap::NapPlan;
use crate::memory::pointer::{self, CliPointer, SyntaxPointer};
use crate::memory::tree_store::SettleOutcome;
use crate::scan::{self, DuplicatesParams, DuplicatesReport, ScanParams, ScanReport};
use crate::vfs::{Os, Overlay};

/// One composition root per invocation (PLAN §1.6 sketch).
pub struct Session {
    overlay: Arc<Overlay>,
    cache: Arc<ParseCache>,
    langs: LangRegistry,
    mem: ContextManager,
}

/// The anchor target of a `note`, as the CLI spelled it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoteTarget {
    /// `--ptr <notation | json>` — resolved against the current tree.
    Ptr(String),
    /// `--commit <sha>` — validated through `git rev-parse`.
    Commit(String),
}

/// `janitor note` parameters (F6.7's CLI shape).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NoteParams {
    pub target: Option<NoteTarget>,
    pub text: String,
    pub lock: bool,
}

impl Session {
    /// A session over the real disk (`Os` base, empty overlay — the CLI's
    /// one-shot shape) with the memory layer at the process working
    /// directory's `.janitor/`.
    pub fn new() -> Self {
        Self::with_overlay(Arc::new(Overlay::new(Arc::new(Os))))
    }

    /// A session over a caller-provided overlay — the staged-edit flows
    /// (F5's apply-then-scan) and the tests' virtual-file setups.
    pub fn with_overlay(overlay: Arc<Overlay>) -> Self {
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            overlay,
            cache: Arc::new(ParseCache::new()),
            langs: LangRegistry::new(),
            mem: ContextManager::discover(&cwd),
        }
    }

    /// A session whose memory layer lives at an explicit root (the
    /// library-consumer and test seam; the overlay stays over the process
    /// working directory).
    pub fn with_memory_root(overlay: Arc<Overlay>, memory_root: PathBuf) -> Self {
        Self {
            overlay,
            cache: Arc::new(ParseCache::new()),
            langs: LangRegistry::new(),
            mem: ContextManager::new(memory_root),
        }
    }

    /// The session's overlay (shared, `Arc`).
    pub fn overlay(&self) -> &Arc<Overlay> {
        &self.overlay
    }

    /// The session's parse cache (streaming discipline: empty between
    /// scans, ADR-0007).
    pub fn cache(&self) -> &Arc<ParseCache> {
        &self.cache
    }

    /// The session's language registry.
    pub fn registry(&self) -> &LangRegistry {
        &self.langs
    }

    /// The session's memory layer.
    pub fn memory(&self) -> &ContextManager {
        &self.mem
    }

    /// Run a structural scan (see [`scan`] for the pipeline); a successful
    /// scan records its status into STATE.json when the local layer exists.
    pub fn scan(&self, params: &ScanParams) -> Result<ScanReport, JanitorError> {
        let report = scan::scan(&self.overlay, &self.cache, &self.langs, params)?;
        self.mem.record_scan(
            report.stats.files,
            report.stats.matches,
            report.skipped.len() as u64 + report.skipped_for_language.values().sum::<u64>(),
        );
        Ok(report)
    }

    /// Run a duplicates scan (F8): every capturable subtree above the
    /// minimum size, grouped by Merkle fingerprint — no rule. Records its
    /// status like a scan (members are the "matches").
    pub fn duplicates(&self, params: &DuplicatesParams) -> Result<DuplicatesReport, JanitorError> {
        let report = scan::duplicates(&self.overlay, &self.cache, &self.langs, params)?;
        self.mem.record_scan(
            report.stats.files,
            report.stats.members,
            report.skipped.len() as u64,
        );
        Ok(report)
    }

    /// Run a transactional apply (see [`apply`] for the two-phase,
    /// whole-diff all-or-nothing design); a successful apply records its
    /// status into STATE.json when the local layer exists.
    pub fn apply(&self, params: &ApplyParams) -> Result<ApplyReport, JanitorError> {
        let report = apply::apply(&self.overlay, &self.cache, &self.langs, params)?;
        self.mem
            .record_apply(report.stats.files, report.stats.edits, report.dry_run);
        Ok(report)
    }

    // --- the memory commands (F6) ---------------------------------------------

    /// `janitor init [--force]`.
    pub fn init(&self, force: bool) -> Result<InitReport, JanitorError> {
        self.mem.init(force)
    }

    /// `janitor wake --budget N`.
    pub fn wake(&self, budget: u64) -> Result<WakePayload, JanitorError> {
        self.mem.wake(budget)
    }

    /// `janitor note`: resolve the anchor (the anti-hallucination gate —
    /// the pointer is validated against the current tree or git *before*
    /// anything is appended), then persist. Pointer resolution reads the
    /// file through the overlay (staged content included — the same view
    /// `apply` validates against).
    pub fn note(&self, params: &NoteParams) -> Result<NoteReport, JanitorError> {
        let (ptr, meta) = match &params.target {
            None => (None, AnchorMeta::default()),
            Some(NoteTarget::Commit(sha)) => {
                let cwd = self.mem.root().parent().unwrap_or_else(|| Path::new("."));
                let resolved = pointer::validate_commit(sha, cwd)?;
                (
                    Some(SyntaxPointer::Commit { sha: resolved }),
                    AnchorMeta::default(),
                )
            }
            Some(NoteTarget::Ptr(arg)) => match pointer::parse_cli_pointer(arg)? {
                CliPointer::Fresh {
                    path,
                    span,
                    kind_name,
                } => {
                    let bytes = self.overlay.read_file(Path::new(&path))?;
                    let stamped =
                        pointer::stamp_node_pointer(&self.langs, &path, span, &kind_name, &bytes)?;
                    (Some(stamped), AnchorMeta::default())
                }
                CliPointer::Stored(SyntaxPointer::Commit { sha }) => {
                    let cwd = self.mem.root().parent().unwrap_or_else(|| Path::new("."));
                    let resolved = pointer::validate_commit(&sha, cwd)?;
                    (
                        Some(SyntaxPointer::Commit { sha: resolved }),
                        AnchorMeta::default(),
                    )
                }
                CliPointer::Stored(SyntaxPointer::Node {
                    path,
                    span,
                    kind_name,
                    content_sha,
                }) => {
                    let bytes = self.overlay.read_file(Path::new(&path))?;
                    let outcome = pointer::resolve_node_pointer(
                        &self.langs,
                        &path,
                        span,
                        &kind_name,
                        &content_sha,
                        &bytes,
                    )?;
                    (
                        Some(outcome.anchored.clone()),
                        AnchorMeta {
                            reanchored: outcome.reanchored,
                            drift: outcome.drift,
                        },
                    )
                }
            },
        };
        self.mem.append_note(ptr, meta, &params.text, params.lock)
    }

    /// `janitor nap` (no args).
    pub fn nap_next(&self) -> Result<NapPlan, JanitorError> {
        self.mem.nap_next()
    }

    /// `janitor nap --slot <lo-hi> --text <line>`.
    pub fn nap_settle(&self, slot: &str, text: &str) -> Result<SettleOutcome, JanitorError> {
        self.mem.nap_settle(slot, text)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The session is the composition root: one overlay, one cache, one
    /// registry — and scans run against all three at once.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn session_composes_the_three_domains() {
        let session = Session::new();
        assert!(session.cache().is_empty());
        assert!(!session.registry().is_empty());

        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            tmp.path().join("s.rs"),
            "fn s() -> u32 {\n    let a = 1;\n    let b = 2;\n    a\n}\n",
        )
        .expect("corpus");
        let report = session
            .scan(&ScanParams::new(
                "(under block (seq (kind let_declaration) (kind let_declaration)))",
                tmp.path(),
            ))
            .expect("scan composes");
        assert_eq!(report.matches.len(), 1);
    }
}
