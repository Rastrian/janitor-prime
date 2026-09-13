//! Composition root (PLAN §1.6) — the only place the three domains meet.
//!
//! `Session` will own the `Arc<Overlay>`, the `ParseCache`, the feature-gated
//! `LangRegistry` and the `ContextManager`; one session per CLI invocation
//! (stateless across invocations in v0.1, ADR-0007). The command set
//! (`init|wake|note|nap|scan|apply`) wires here as its phases land.
// TODO(F4): session + scan (rayon walk, ADR-0016 canonical sort).
// TODO(F5): apply loop (§1.2.1 steps 1-7 in overlay stage/commit/rollback).
// TODO(F6): init/wake/note/nap wiring.
