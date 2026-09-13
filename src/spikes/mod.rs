//! F1 kill-point spikes (PLAN §3 F1, §5).
//!
//! Compiled only under `--features spikes` — never in the default set and
//! never in the `lang-all` CI legs; a dedicated CI job builds and tests this
//! feature so the spike code keeps compiling until it is **deleted at F3
//! entry** (the plan's rule, not a suggestion). Spikes are measurement
//! harnesses and fences, not product code, but the deny lints apply to them
//! too (`unwrap`/`expect`/`panic` stay out of the runners; failures surface
//! as [`SpikeError`] and only `#[test]` entry points may `expect`).
//!
//! Conventions kept by every spike file:
//!
//! - self-contained in its measurement logic: no spike depends on another
//!   spike's helpers (this module's shared `rust_parser`/`SpikeError` are
//!   plumbing, not logic);
//! - parsing goes through `crate::ast::ffi_shim` (the tree-sitter boundary);
//!   spike files may still *name* `tree_sitter` types they measure — they die
//!   at F3 entry, so the one-file pin-swap property of `ffi_shim` (ADR-0015)
//!   is unaffected;
//! - fast correctness legs run under plain `cargo test --features spikes`;
//!   heavy measurement legs are `#[ignore]`d and run explicitly with
//!   `cargo test --features spikes -- --ignored`, so CI stays quick while the
//!   numbers land in `benches/` and in the decision memos
//!   (`docs/planning/spikes/`).
//!
//! Runner functions are invoked only from `#[test]` entry points, so under
//! the CI compile fence (`cargo build --features spikes`) they have no
//! caller — the `dead_code` allowance below is that wiring artifact and
//! nothing more.
#![allow(dead_code)]

pub mod incremental_reparse;
pub mod point_semantics;

use thiserror::Error;

/// Typed failure surface for spike runners — the deny lints apply to spike
/// code too, so runners return errors instead of panicking; only `#[test]`
/// entry points may `expect` on them.
#[derive(Debug, Error)]
pub enum SpikeError {
    #[error("parser setup failed: {message}")]
    ParserSetup { message: String },
    #[error("parse returned no tree ({what})")]
    NoTree { what: String },
    #[error("refusing to edit a shared tree: Arc::strong_count == {count}")]
    SharedTree { count: usize },
    #[error("corpus anchor {needle:?} not found")]
    AnchorMissing { needle: String },
    #[error("correctness divergence between full and incremental paths: {detail}")]
    CorrectnessDivergence { detail: String },
}

/// Parser primed with the Rust grammar via the ffi_shim boundary — one
/// parser per worker per language is the §1.2 ownership-map shape; the
/// spikes' serial legs simply keep one for the whole run.
pub fn rust_parser() -> Result<tree_sitter::Parser, SpikeError> {
    let mut parser = tree_sitter::Parser::new();
    let language = crate::ast::ffi_shim::rust_language();
    parser
        .set_language(&language)
        .map_err(|err| SpikeError::ParserSetup {
            message: err.to_string(),
        })?;
    Ok(parser)
}

// --- shared edit plumbing (the KP-3-locked byte math) ---------------------
//
// `point_at`/`input_edit_for` are the reference implementation of PLAN
// §1.2.1 step 4; the KP-3 fence tests in `point_semantics` pin them against
// the real parser, and KP-1/KP-4 build their edits on the same math so all
// spikes agree with what F3's `ffi_shim` will embed.

/// Byte-based `Point` derivation: row = newline count before `byte`,
/// column = `byte` minus the start of that row. Column units are BYTES
/// (A.3.4), never chars and never UTF-16 code units.
pub fn point_at(source: &[u8], byte: usize) -> tree_sitter::Point {
    let byte = byte.min(source.len());
    let mut row = 0_usize;
    let mut row_start = 0_usize;
    for (i, b) in source[..byte].iter().enumerate() {
        if *b == b'\n' {
            row += 1;
            row_start = i + 1;
        }
    }
    tree_sitter::Point {
        row,
        column: byte - row_start,
    }
}

/// A byte splice: replace `[start_byte, old_end_byte)` of the old source
/// with `new_text` (half-open UTF-8 byte ranges, ADR-0014's span model).
pub struct Splice {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_text: Vec<u8>,
}

/// Apply a splice, producing the new source bytes.
pub fn splice(old: &[u8], edit: &Splice) -> Vec<u8> {
    let mut out = Vec::with_capacity(old.len() + edit.new_text.len());
    out.extend_from_slice(&old[..edit.start_byte]);
    out.extend_from_slice(&edit.new_text);
    out.extend_from_slice(&old[edit.old_end_byte..]);
    out
}

/// Derive the `InputEdit` for a splice using byte-based points only
/// (PLAN §1.2.1 step 4). If tree-sitter's `Point` units ever disagree with
/// this math, the KP-3 round-trip tests fail.
pub fn input_edit_for(old: &[u8], new: &[u8], edit: &Splice) -> tree_sitter::InputEdit {
    let new_end_byte = edit.start_byte + edit.new_text.len();
    tree_sitter::InputEdit {
        start_byte: edit.start_byte,
        old_end_byte: edit.old_end_byte,
        new_end_byte,
        start_position: point_at(old, edit.start_byte),
        old_end_position: point_at(old, edit.old_end_byte),
        new_end_position: point_at(new, new_end_byte),
    }
}
