//! Typed error taxonomy (PLAN §1.7) — exhaustive, thiserror-only (no anyhow).
//!
//! Every error maps onto the JSON envelope via one `From<JanitorError>` impl
//! in `crate::envelope`; the `E_*` codes below are the agent-facing contract
//! and are frozen from F0 onward. Payloads are typed skeletons: placeholder
//! `serde_json::Value` fields marked `TODO(...)` become their real report
//! types when the owning phase lands, without touching the code names.

use serde_json::Value;
use thiserror::Error;

/// VFS domain errors — defined in and re-exported from `crate::vfs::error`
/// since F2.1 (the phase that owns them), so the taxonomy below stays the
/// single umbrella while the VFS slice grows next to its module.
pub use crate::vfs::VfsError;

/// Parse-layer errors (PLAN §1.2, F3).
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("no grammar enabled for language {lang:?}")]
    NoGrammarForLang { lang: String },
    #[error("language mismatch: expected {expected}, got {actual}")]
    LanguageMismatch { expected: String, actual: String },
    #[error("source is not valid UTF-8: {path}")]
    SourceDecoding { path: String },
    /// Rule T1's exclusivity guard (§1.2.1 step 5, E3(b)): `Tree::edit` was
    /// attempted while other `Arc<Tree>` handles exist. Never retried
    /// silently — the caller must take exclusive ownership first.
    #[error("refusing to edit a shared tree: Arc::strong_count == {count}")]
    SharedTree { count: usize },
    /// Priming a parser with a registered grammar failed at runtime (the
    /// KP-5 ABI fence's runtime leg; build-time drift is CI `pin-check`'s
    /// job, this catches the rest loudly).
    #[error("parser setup failed for {lang:?}: {message}")]
    Setup { lang: String, message: String },
    /// The C parse boundary returned no tree (e.g. a cancelled parse).
    #[error("parse returned no tree ({detail})")]
    NoTree { detail: String },
    /// §1.2.1 step 2's structural half: an edit range that is inverted or
    /// beyond the old bytes. The expected-bytes half is F5's payload
    /// validation (`E_EDIT_MISMATCH`).
    #[error("edit range [{start}, {old_end}) is invalid for {len}-byte source")]
    EditRange {
        start: usize,
        old_end: usize,
        len: usize,
    },
}

/// DSL front-end and rule-compile errors (PLAN §1.4.1, F4).
#[derive(Debug, Error)]
pub enum DslError {
    #[error("lex error at byte {pos}: expected one of {expected:?}")]
    Lex { pos: u32, expected: Vec<String> },
    #[error("parse error at byte {pos}: expected one of {expected:?}")]
    Parse {
        pos: u32,
        expected: Vec<String>,
        suggestion: Option<String>,
    },
    #[error("unknown node kind {kind:?} for language {lang:?}")]
    UnknownKind {
        lang: String,
        kind: String,
        /// Nearest known kind name when one is close (F4: the agent-retry
        /// surface docs/DSL.md §7 contracts; absent when nothing is close).
        /// Only this field was appended to the F3 skeleton — the envelope's
        /// `details` grows by key, never renames.
        suggestion: Option<String>,
    },
    #[error("rule failed to compile: {message}")]
    Compile { message: String },
    /// ADR-0016: `unify` is v0.2 (F8); a rule using it fails rule-compile
    /// with this versioned error, never a silent no-match.
    #[error("feature {feature:?} is not available in schema version {version:?}")]
    UnsupportedVersion { feature: String, version: String },
}

/// Matcher errors (PLAN §1.4.3, F4).
#[derive(Debug, Error)]
pub enum MatchError {
    // TODO(F4): real matcher failure modes; E_MATCH is reserved.
    #[error("matcher engine error: {message}")]
    Engine { message: String },
}

/// Edit-payload validation errors (PLAN §1.2.1 step 2, ADR-0014, F5).
#[derive(Debug, Error)]
pub enum EditError {
    #[error("edit mismatch on {path}: {detail}")]
    Mismatch { path: String, detail: String },
    #[error("overlapping edits on {path}: {detail}")]
    Overlap { path: String, detail: String },
    #[error("stale view of {path}: expected content hash {expected_sha:?}")]
    StaleView { path: String, expected_sha: String },
    #[error("edit out of range on {path}: offset {offset} beyond length {len}")]
    OutOfRange { path: String, offset: u64, len: u64 },
    /// F5: the `--diff` JSON payload itself is malformed (bad JSON, wrong
    /// types, duplicate paths). Append-only addition to the F0 skeleton.
    #[error("malformed edit payload: {detail}")]
    Payload { detail: String },
}

/// Post-edit validation errors (PLAN §1.2.1 step 7, F5).
#[derive(Debug, Error)]
pub enum ValidateError {
    #[error("edited ranges did not re-parse cleanly: {count} offending node(s)")]
    ParseInvalid {
        count: usize,
        /// TODO(F5): typed node report (path, span, kind, is_missing).
        nodes: Vec<Value>,
    },
    #[error("flush partially persisted: {summary}")]
    FlushPartial {
        summary: String,
        /// TODO(F5): structured `{persisted: [], rolled_back: []}` report.
        report: Value,
    },
}

/// Memory domain errors (PLAN §1.5, F6).
#[derive(Debug, Error)]
pub enum MemoryError {
    #[error("syntax pointer dangles: {path} {span:?}")]
    PointerDangling { path: String, span: (u64, u64) },
    /// The pending compression request rides in `request` (slot, source
    /// material, instruction) so an agent can act on the refusal directly
    /// (§1.5.2 — refuse with the request, never serve holes).
    #[error("compression slot {slot:?} pending; write it via nap before wake")]
    NapPending { slot: String, request: Value },
    #[error("memory log corrupt; trailing line quarantined to {quarantined}")]
    LogCorrupt { quarantined: String },
    /// Corruption *before* the trailing line: quarantining would renumber
    /// every later fact (position is identity, ADR-0006), so the log is
    /// refused, never repaired (§1.5.1).
    #[error(
        "log corruption at line {line} (before the trailing line); refusing — repair would renumber every later fact"
    )]
    LogUnrecoverable { line: u64 },
    #[error("invalid budget: {detail}")]
    BudgetInvalid { detail: String },
    /// `note` text discipline: one line, ≤ 280 bytes (OptMem-derived line
    /// budget the wake math depends on, §1.5.3).
    #[error("note rejected: {detail}")]
    NoteInvalid { detail: String },
    #[error("memory not initialized at {root}; run `janitor init` first")]
    NotInitialized { root: String },
    #[error("memory already initialized at {root}; pass --force to reset")]
    AlreadyInitialized { root: String },
    /// The CLI pointer notation itself is malformed (`path:start-end:kind`
    /// / commit hex — SCHEMA.md); distinct from `PointerDangling`, which
    /// means the notation parsed but will not anchor (ADR-0012).
    #[error("pointer notation invalid: {detail}")]
    PointerNotation { detail: String },
    #[error("commit pointer {sha:?} invalid: {detail}")]
    CommitInvalid { sha: String, detail: String },
    /// nap slot discipline: notation, ordering (dense prefix), idempotent
    /// resubmission with different text, or the ≤ 256-byte one-line summary
    /// limit (§1.5.4).
    #[error("slot {slot:?} rejected: {detail}")]
    SlotInvalid { slot: String, detail: String },
    #[error("tree store corrupt: {detail}")]
    TreeCorrupt { detail: String },
}

/// Envelope emission errors (stderr diagnostics; stdout unreachable).
#[derive(Debug, Error)]
pub enum OutputError {
    #[error("envelope serialization failed: {0}")]
    Serialization(String),
    #[error("stdout write failed: {0}")]
    Io(String),
}

/// The umbrella over the whole taxonomy (PLAN §1.7).
#[derive(Debug, Error)]
pub enum JanitorError {
    #[error(transparent)]
    Vfs(#[from] VfsError),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error(transparent)]
    Dsl(#[from] DslError),
    #[error(transparent)]
    Match(#[from] MatchError),
    #[error(transparent)]
    Edit(#[from] EditError),
    #[error(transparent)]
    Validate(#[from] ValidateError),
    #[error(transparent)]
    Memory(#[from] MemoryError),
    #[error(transparent)]
    Output(#[from] OutputError),
}

impl JanitorError {
    /// The stable `E_*` code table — one flat list, the whole contract.
    /// Codes named by the plan are verbatim (E_STALE_VIEW, E_PARSE_INVALID,
    /// E_POINTER_DANGLING, E_NAP_PENDING, E_FLUSH_PARTIAL,
    /// E_UNSUPPORTED_VERSION, …); the rest follow `E_<DOMAIN>_<VARIANT>`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Vfs(VfsError::NotFound { .. }) => "E_VFS_NOT_FOUND",
            Self::Vfs(VfsError::Permission { .. }) => "E_VFS_PERMISSION",
            Self::Vfs(VfsError::Io { .. }) => "E_VFS_IO",
            Self::Vfs(VfsError::InvalidPath { .. }) => "E_VFS_INVALID_PATH",
            Self::Vfs(VfsError::BufferDeleted { .. }) => "E_VFS_BUFFER_DELETED",
            Self::Vfs(VfsError::InvalidUtf8 { .. }) => "E_VFS_INVALID_UTF8",
            Self::Vfs(VfsError::NotStaged { .. }) => "E_VFS_NOT_STAGED",
            // Shared with `ValidateError::FlushPartial` (the F5 wrapper over
            // this same condition): one agent-facing code, by plan.
            Self::Vfs(VfsError::FlushPartial { .. }) => "E_FLUSH_PARTIAL",
            Self::Parse(ParseError::NoGrammarForLang { .. }) => "E_PARSE_NO_GRAMMAR_FOR_LANG",
            Self::Parse(ParseError::LanguageMismatch { .. }) => "E_PARSE_LANGUAGE_MISMATCH",
            Self::Parse(ParseError::SourceDecoding { .. }) => "E_PARSE_SOURCE_DECODING",
            Self::Parse(ParseError::SharedTree { .. }) => "E_PARSE_SHARED_TREE",
            Self::Parse(ParseError::Setup { .. }) => "E_PARSE_SETUP",
            Self::Parse(ParseError::NoTree { .. }) => "E_PARSE_NO_TREE",
            Self::Parse(ParseError::EditRange { .. }) => "E_PARSE_EDIT_RANGE",
            Self::Dsl(DslError::Lex { .. }) => "E_DSL_LEX",
            Self::Dsl(DslError::Parse { .. }) => "E_DSL_PARSE",
            Self::Dsl(DslError::UnknownKind { .. }) => "E_DSL_UNKNOWN_KIND",
            Self::Dsl(DslError::Compile { .. }) => "E_DSL_COMPILE",
            Self::Dsl(DslError::UnsupportedVersion { .. }) => "E_UNSUPPORTED_VERSION",
            Self::Match(MatchError::Engine { .. }) => "E_MATCH",
            Self::Edit(EditError::Mismatch { .. }) => "E_EDIT_MISMATCH",
            Self::Edit(EditError::Overlap { .. }) => "E_EDIT_OVERLAP",
            Self::Edit(EditError::StaleView { .. }) => "E_STALE_VIEW",
            Self::Edit(EditError::OutOfRange { .. }) => "E_EDIT_OUT_OF_RANGE",
            Self::Edit(EditError::Payload { .. }) => "E_EDIT_PAYLOAD",
            Self::Validate(ValidateError::ParseInvalid { .. }) => "E_PARSE_INVALID",
            Self::Validate(ValidateError::FlushPartial { .. }) => "E_FLUSH_PARTIAL",
            Self::Memory(MemoryError::PointerDangling { .. }) => "E_POINTER_DANGLING",
            Self::Memory(MemoryError::NapPending { .. }) => "E_NAP_PENDING",
            Self::Memory(MemoryError::LogCorrupt { .. }) => "E_LOG_CORRUPT",
            Self::Memory(MemoryError::LogUnrecoverable { .. }) => "E_LOG_UNRECOVERABLE",
            Self::Memory(MemoryError::BudgetInvalid { .. }) => "E_BUDGET_INVALID",
            Self::Memory(MemoryError::NoteInvalid { .. }) => "E_NOTE_INVALID",
            Self::Memory(MemoryError::NotInitialized { .. }) => "E_NOT_INITIALIZED",
            Self::Memory(MemoryError::AlreadyInitialized { .. }) => "E_ALREADY_INITIALIZED",
            Self::Memory(MemoryError::PointerNotation { .. }) => "E_POINTER_INVALID",
            Self::Memory(MemoryError::CommitInvalid { .. }) => "E_COMMIT_INVALID",
            Self::Memory(MemoryError::SlotInvalid { .. }) => "E_SLOT_INVALID",
            Self::Memory(MemoryError::TreeCorrupt { .. }) => "E_TREE_CORRUPT",
            Self::Output(OutputError::Serialization(_)) => "E_OUTPUT",
            Self::Output(OutputError::Io(_)) => "E_OUTPUT",
        }
    }

    /// Structured context for the envelope's `error.details` body, when the
    /// variant carries any.
    pub fn details(&self) -> Option<Value> {
        match self {
            Self::Vfs(VfsError::NotFound { path })
            | Self::Vfs(VfsError::Permission { path })
            | Self::Vfs(VfsError::InvalidPath { path })
            | Self::Vfs(VfsError::BufferDeleted { path })
            | Self::Vfs(VfsError::NotStaged { path }) => Some(serde_json::json!({ "path": path })),
            Self::Vfs(VfsError::Io { path, message }) => {
                Some(serde_json::json!({ "path": path, "message": message }))
            }
            Self::Vfs(VfsError::InvalidUtf8 { path }) => {
                Some(serde_json::json!({ "path": path, "encoding": "utf-8" }))
            }
            Self::Vfs(VfsError::FlushPartial { report }) => serde_json::to_value(report).ok(),
            Self::Dsl(DslError::Lex { pos, expected }) => {
                Some(serde_json::json!({ "position": pos, "expected": expected }))
            }
            Self::Dsl(DslError::Parse {
                pos,
                expected,
                suggestion,
            }) => Some(serde_json::json!({
                "position": pos,
                "expected": expected,
                "suggestion": suggestion,
            })),
            Self::Dsl(DslError::UnknownKind {
                lang,
                kind,
                suggestion,
            }) => {
                // `suggestion` appears only when one exists (append-only key).
                let mut details = serde_json::json!({ "lang": lang, "kind": kind });
                if let Some(hint) = suggestion {
                    details["suggestion"] = serde_json::json!(hint);
                }
                Some(details)
            }
            Self::Dsl(DslError::UnsupportedVersion { feature, version }) => {
                Some(serde_json::json!({ "feature": feature, "version": version }))
            }
            Self::Edit(EditError::StaleView { path, expected_sha }) => {
                Some(serde_json::json!({ "path": path, "expected_sha": expected_sha }))
            }
            Self::Edit(EditError::OutOfRange { path, offset, len }) => {
                Some(serde_json::json!({ "path": path, "offset": offset, "len": len }))
            }
            Self::Edit(EditError::Mismatch { path, detail })
            | Self::Edit(EditError::Overlap { path, detail }) => {
                Some(serde_json::json!({ "path": path, "detail": detail }))
            }
            Self::Edit(EditError::Payload { detail }) => {
                Some(serde_json::json!({ "detail": detail }))
            }
            Self::Validate(ValidateError::ParseInvalid { nodes, .. }) => {
                Some(serde_json::json!({ "nodes": nodes }))
            }
            Self::Parse(ParseError::SharedTree { count }) => {
                Some(serde_json::json!({ "strong_count": count }))
            }
            Self::Parse(ParseError::Setup { lang, message }) => {
                Some(serde_json::json!({ "lang": lang, "message": message }))
            }
            Self::Parse(ParseError::EditRange {
                start,
                old_end,
                len,
            }) => Some(serde_json::json!({ "start": start, "old_end": old_end, "len": len })),
            Self::Validate(ValidateError::FlushPartial { report, .. }) => Some(report.clone()),
            Self::Memory(MemoryError::PointerDangling { path, span }) => {
                Some(serde_json::json!({ "path": path, "span": [span.0, span.1] }))
            }
            Self::Memory(MemoryError::NapPending { slot, request }) => {
                Some(serde_json::json!({ "slot": slot, "request": request }))
            }
            Self::Memory(MemoryError::LogCorrupt { quarantined }) => {
                Some(serde_json::json!({ "quarantined": quarantined }))
            }
            Self::Memory(MemoryError::LogUnrecoverable { line }) => {
                Some(serde_json::json!({ "line": line }))
            }
            Self::Memory(MemoryError::NoteInvalid { detail })
            | Self::Memory(MemoryError::PointerNotation { detail })
            | Self::Memory(MemoryError::TreeCorrupt { detail }) => {
                Some(serde_json::json!({ "detail": detail }))
            }
            Self::Memory(MemoryError::NotInitialized { root })
            | Self::Memory(MemoryError::AlreadyInitialized { root }) => {
                Some(serde_json::json!({ "root": root }))
            }
            Self::Memory(MemoryError::CommitInvalid { sha, detail }) => {
                Some(serde_json::json!({ "sha": sha, "detail": detail }))
            }
            Self::Memory(MemoryError::SlotInvalid { slot, detail }) => {
                Some(serde_json::json!({ "slot": slot, "detail": detail }))
            }
            _ => None,
        }
    }

    /// Exit-code mapping (PLAN §1.6): every `JanitorError` is operational
    /// (`1`); usage (`2`) exists only at the clap boundary (`crate::cli`).
    pub const fn exit_code(&self) -> u8 {
        crate::envelope::EXIT_OPERATIONAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The plan-named `E_*` codes must be exact — they are the frozen
    /// agent-facing contract (PLAN §1.6/§1.7, ADR-0014/0016).
    #[test]
    fn plan_named_codes_are_exact() {
        let cases: Vec<(JanitorError, &str)> = vec![
            (
                VfsError::BufferDeleted { path: "p".into() }.into(),
                "E_VFS_BUFFER_DELETED",
            ),
            (
                VfsError::InvalidUtf8 { path: "p".into() }.into(),
                "E_VFS_INVALID_UTF8",
            ),
            (
                VfsError::NotStaged { path: "p".into() }.into(),
                "E_VFS_NOT_STAGED",
            ),
            (
                VfsError::FlushPartial {
                    report: crate::vfs::FlushReport::default(),
                }
                .into(),
                "E_FLUSH_PARTIAL",
            ),
            (
                EditError::Mismatch {
                    path: "p".into(),
                    detail: "d".into(),
                }
                .into(),
                "E_EDIT_MISMATCH",
            ),
            (
                EditError::Overlap {
                    path: "p".into(),
                    detail: "d".into(),
                }
                .into(),
                "E_EDIT_OVERLAP",
            ),
            (
                EditError::StaleView {
                    path: "p".into(),
                    expected_sha: "s".into(),
                }
                .into(),
                "E_STALE_VIEW",
            ),
            (
                EditError::OutOfRange {
                    path: "p".into(),
                    offset: 99,
                    len: 10,
                }
                .into(),
                "E_EDIT_OUT_OF_RANGE",
            ),
            (
                EditError::Payload {
                    detail: "EOF".into(),
                }
                .into(),
                "E_EDIT_PAYLOAD",
            ),
            (
                ValidateError::ParseInvalid {
                    count: 1,
                    nodes: vec![],
                }
                .into(),
                "E_PARSE_INVALID",
            ),
            (
                ValidateError::FlushPartial {
                    summary: "s".into(),
                    report: serde_json::json!({}),
                }
                .into(),
                "E_FLUSH_PARTIAL",
            ),
            (
                MemoryError::PointerDangling {
                    path: "p".into(),
                    span: (0, 1),
                }
                .into(),
                "E_POINTER_DANGLING",
            ),
            (
                MemoryError::NapPending {
                    slot: "0-1".into(),
                    request: serde_json::json!({ "slot": "0-1" }),
                }
                .into(),
                "E_NAP_PENDING",
            ),
            (
                MemoryError::LogCorrupt {
                    quarantined: "q".into(),
                }
                .into(),
                "E_LOG_CORRUPT",
            ),
            (
                MemoryError::BudgetInvalid { detail: "d".into() }.into(),
                "E_BUDGET_INVALID",
            ),
            // F6 additions: same convention, structured details, exact codes.
            (
                MemoryError::LogUnrecoverable { line: 3 }.into(),
                "E_LOG_UNRECOVERABLE",
            ),
            (
                MemoryError::NoteInvalid { detail: "d".into() }.into(),
                "E_NOTE_INVALID",
            ),
            (
                MemoryError::NotInitialized {
                    root: ".janitor".into(),
                }
                .into(),
                "E_NOT_INITIALIZED",
            ),
            (
                MemoryError::AlreadyInitialized {
                    root: ".janitor".into(),
                }
                .into(),
                "E_ALREADY_INITIALIZED",
            ),
            (
                MemoryError::PointerNotation { detail: "d".into() }.into(),
                "E_POINTER_INVALID",
            ),
            (
                MemoryError::CommitInvalid {
                    sha: "abc".into(),
                    detail: "d".into(),
                }
                .into(),
                "E_COMMIT_INVALID",
            ),
            (
                MemoryError::SlotInvalid {
                    slot: "0-1".into(),
                    detail: "d".into(),
                }
                .into(),
                "E_SLOT_INVALID",
            ),
            (
                MemoryError::TreeCorrupt { detail: "d".into() }.into(),
                "E_TREE_CORRUPT",
            ),
            (
                DslError::UnsupportedVersion {
                    feature: "unify".into(),
                    version: "janitor.v1".into(),
                }
                .into(),
                "E_UNSUPPORTED_VERSION",
            ),
        ];
        for (err, code) in cases {
            assert_eq!(err.code(), code);
            assert_eq!(err.exit_code(), crate::envelope::EXIT_OPERATIONAL);
        }
    }

    #[test]
    fn operational_exit_code_is_one() {
        let err: JanitorError = OutputError::Io("broken pipe".into()).into();
        assert_eq!(err.exit_code(), 1);
        assert_eq!(err.code(), "E_OUTPUT");
    }

    /// F3 parse-layer additions follow the `E_<DOMAIN>_<VARIANT>` convention
    /// and carry structured details (the frozen surface grows only by
    /// append, per the F0 contract note).
    #[test]
    fn f3_parse_error_codes_and_details() {
        let err: JanitorError = ParseError::SharedTree { count: 2 }.into();
        assert_eq!(err.code(), "E_PARSE_SHARED_TREE");
        assert_eq!(
            err.details(),
            Some(serde_json::json!({ "strong_count": 2 }))
        );

        let err: JanitorError = ParseError::Setup {
            lang: "rust".into(),
            message: "version mismatch".into(),
        }
        .into();
        assert_eq!(err.code(), "E_PARSE_SETUP");
        assert_eq!(
            err.details(),
            Some(serde_json::json!({ "lang": "rust", "message": "version mismatch" }))
        );

        let err: JanitorError = ParseError::EditRange {
            start: 9,
            old_end: 3,
            len: 12,
        }
        .into();
        assert_eq!(err.code(), "E_PARSE_EDIT_RANGE");
        assert_eq!(
            err.details(),
            Some(serde_json::json!({ "start": 9, "old_end": 3, "len": 12 }))
        );
    }
}
