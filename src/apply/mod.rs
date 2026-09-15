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
pub mod diff;
pub mod splice;
