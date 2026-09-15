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
//!   traversal — plus the F4 DSL, compiler, scoped-NFA evaluator, and
//!   match-set layer;
//! - `scan` — the F4 vertical slice's orchestrator: walk, skip policy,
//!   rayon match, canonical janitor.v1 report;
//! - `apply` — the F5 transactional rewrite loop: diff payload in,
//!   whole-diff all-or-nothing stage → validate → flush out;
//! - `memory` — the F6 persistent layer: append-only JSONL fact log with
//!   position-derived ids and torn-tail quarantine (ADR-0006), the
//!   re-anchorable `SyntaxPointer` anti-hallucination gate (ADR-0012),
//!   and the synchronous `nap` compression protocol (ADR-0013);
//! - `orchestrator` — the F4 `Session` composition root;
//! - `mcp` — the F10b MCP stdio server: the same `Session` code paths
//!   the CLI dispatches, exposed as JSON-RPC tools (ADR-0019).
//!
//! Hard rules in force from F0 (PLAN §1.1): no `unwrap`/`expect`/`panic`
//! in production code (clippy-denied; tests exempt), stdout is 100% JSON
//! through the `janitor.v1` envelope, diagnostics go to stderr via
//! `tracing`, and no `unsafe` in janitor code (Rule T6 — the single,
//! test-only exception is the counting allocator `tests/alloc.rs`
//! requires by the `GlobalAlloc` contract).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

pub mod apply;
pub mod ast;
pub mod cli;
pub mod envelope;
pub mod error;
pub mod mcp;
pub mod memory;
pub mod orchestrator;
pub mod scan;
#[cfg(feature = "spikes")]
pub mod spikes;
pub mod vfs;
