//! Typed error taxonomy (PLAN §1.7) — exhaustive, thiserror-only (no anyhow).
//!
//! Every error maps onto the JSON envelope via one `From<JanitorError>` impl
//! in `crate::envelope`; the `E_*` codes below are the agent-facing contract
//! and are frozen from F0 onward. Payloads are typed skeletons: placeholder
//! `serde_json::Value` fields marked `TODO(...)` become their real report
//! types when the owning phase lands, without touching the code names.
#![allow(dead_code)] // scaffold: variants are constructed from F2+; the frozen
// E_* surface is the deliverable of F0 (PLAN §1.7)

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
    UnknownKind { lang: String, kind: String },
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
    #[error("compression slot {slot:?} pending; write it via nap before wake")]
    NapPending { slot: String },
    #[error("memory log corrupt; trailing line quarantined to {quarantined}")]
    LogCorrupt { quarantined: String },
    #[error("invalid budget: {detail}")]
    BudgetInvalid { detail: String },
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
            Self::Validate(ValidateError::ParseInvalid { .. }) => "E_PARSE_INVALID",
            Self::Validate(ValidateError::FlushPartial { .. }) => "E_FLUSH_PARTIAL",
            Self::Memory(MemoryError::PointerDangling { .. }) => "E_POINTER_DANGLING",
            Self::Memory(MemoryError::NapPending { .. }) => "E_NAP_PENDING",
            Self::Memory(MemoryError::LogCorrupt { .. }) => "E_LOG_CORRUPT",
            Self::Memory(MemoryError::BudgetInvalid { .. }) => "E_BUDGET_INVALID",
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
            Self::Dsl(DslError::UnknownKind { lang, kind }) => {
                Some(serde_json::json!({ "lang": lang, "kind": kind }))
            }
            Self::Edit(EditError::StaleView { path, expected_sha }) => {
                Some(serde_json::json!({ "path": path, "expected_sha": expected_sha }))
            }
            Self::Edit(EditError::OutOfRange { path, offset, len }) => {
                Some(serde_json::json!({ "path": path, "offset": offset, "len": len }))
            }
            Self::Validate(ValidateError::ParseInvalid { nodes, .. }) => {
                Some(serde_json::json!({ "nodes": nodes }))
            }
            Self::Validate(ValidateError::FlushPartial { report, .. }) => Some(report.clone()),
            Self::Memory(MemoryError::PointerDangling { path, span }) => {
                Some(serde_json::json!({ "path": path, "span": [span.0, span.1] }))
            }
            Self::Memory(MemoryError::NapPending { slot }) => {
                Some(serde_json::json!({ "slot": slot }))
            }
            Self::Memory(MemoryError::LogCorrupt { quarantined }) => {
                Some(serde_json::json!({ "quarantined": quarantined }))
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
                MemoryError::NapPending { slot: "0-1".into() }.into(),
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
}
