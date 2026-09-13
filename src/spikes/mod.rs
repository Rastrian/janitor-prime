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
