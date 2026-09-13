//! Structural search/rewrite over tree-sitter (PLAN §1.4) — F3/F4 scope.
//!
//! Submodules land with their phases; the module exists from F0 so the
//! crate's shape matches ADR-0001 (BRIEF-named module dirs).
pub mod ffi_shim;
pub mod send_assert;

// TODO(F3): lang registry, parse layer, generation-keyed ParseCache.
// TODO(F4): s-expression DSL, scoped state-machine evaluator, exec.
