//! janitor-prime: deterministic refactoring engine, transactional VFS and
//! persistent memory for autonomous agents (PLAN §0).
//!
//! The library behind the `janitor` binary — split out at F3.6 so the
//! mandated integration tests (`tests/alloc.rs` now; the F4–F6 fixture and
//! e2e suites later) can reach the domain modules. The binary keeps only
//! the CLI entry point (`src/main.rs`); everything else lives here:
//!
//! - `vfs` — transactional overlay over an `Os` base (F2);
//! - `ast` — the parse layer over tree-sitter (F3): ffi boundary, language
//!   registry, snapshot parsing with the guarded edit/re-parse
//!   choreography, the generation-keyed cache, error counting, pooled
//!   traversal; the DSL and evaluator are F4;
//! - `memory`, `orchestrator` — typed skeletons for F6/F4.
//!
//! Hard rules in force from F0 (PLAN §1.1): no `unwrap`/`expect`/`panic`
//! in production code (clippy-denied; tests exempt), stdout is 100% JSON
//! through the `janitor.v1` envelope, diagnostics go to stderr via
//! `tracing`, and no `unsafe` in janitor code (Rule T6 — the single,
//! test-only exception is the counting allocator `tests/alloc.rs`
//! requires by the `GlobalAlloc` contract).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod ast;
pub mod cli;
pub mod envelope;
pub mod error;
pub mod memory;
pub mod orchestrator;
#[cfg(feature = "spikes")]
pub mod spikes;
pub mod vfs;
