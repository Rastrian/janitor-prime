//! Persistent memory (PLAN §1.5) — F6 scope.
//!
//! The model this module will implement (OptMem-inspired, re-engineered —
//! ideas and formats only, zero code copied, ADR-0013/R21):
//!
//! - `.janitor/LOG.txt`: append-only JSONL facts with fsync and
//!   position-derived ids; torn-tail quarantine replaces torn-record repair
//!   (ADR-0006);
//! - `TREE/<size>`: fixed-width (256 B) summary records, dense prefix per
//!   level = build frontier; pure cache rebuildable from LOG.txt;
//! - `wake(budget)`: chars/4 token estimate (ADR-0011), OptMem-style
//!   block-tiling recall with present-bias, `ArchitecturalLock` force-split,
//!   `E_NAP_PENDING` completeness gating;
//! - `note`: anti-hallucination `SyntaxPointer`s (span + content hash,
//!   re-anchorable; ADR-0012) — pointers that cannot be anchored are
//!   refused with `E_POINTER_DANGLING`;
//! - `nap`: the synchronous agent-owned compression protocol — the tool
//!   sequences and stores, the consuming agent summarizes; nothing ever
//!   runs in the background (ADR-0013).
//!
//! F6 lands only after the AST engine (the `memory -> ast` dependency,
//! PLAN §1.0): pointer validation needs a live tree.
// TODO(F6): log, pointer, context (wake), nap (PLAN §1.5, §3 F6).
