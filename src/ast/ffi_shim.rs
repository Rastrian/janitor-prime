//! The ONLY file permitted to import `tree-sitter` types (PLAN §2, ADR-0015,
//! risk R22): a tree-sitter core pin-swap touches this file alone.
//!
//! F3 will implement, per PLAN §1.2 and §3 F3.1:
//!
//! - grammar registration via `tree_sitter_rust::LANGUAGE.into()`
//!   (`LanguageFn` — the current constructor; the old `language()` fn no
//!   longer exists, A.3.7);
//! - the `parse_with_options` wrapper whose callback serves chunks from an
//!   `Arc<[u8]>` overlay snapshot (no lock held during the parse, Rule T2);
//! - the `InputEdit` choreography helpers with byte-column `Point` math
//!   (A.3.4), behind the exclusivity guard of §1.2.1 step 5.
//!
//! F0 keeps this a declared, empty boundary so the fence is visible in the
//! tree from the first code commit. No `unsafe` will ever live in janitor
//! code (Rule T6).
//!
//! F1 adds the minimal seed the kill-point spikes parse through (PLAN §3 F1
//! brief: KP-10 parses "via ffi_shim"): grammar registration via
//! `LanguageFn` and a chunked `parse_with_options` wrapper. The real F3
//! wrapper (PLAN §3 F3.1) generalizes these to per-worker parser pools,
//! `Arc<[u8]>` snapshots and the InputEdit choreography; the seed keeps the
//! same API surface so the spikes already exercise the F3 call shape.

/// The Rust grammar registered via the current `LanguageFn` constructor
/// (`tree_sitter_rust::LANGUAGE`; the old `language()` fn no longer exists,
/// A.3.7). Runtime `set_language` compatibility (ABI 15 vs. core 13..=15) is
/// verified evidence per A.3.8 and re-checked by every caller that loads it.
// F1 seed: see the note on `parse_bytes` below.
#[allow(dead_code)]
#[cfg(feature = "lang-rust")]
pub fn rust_language() -> tree_sitter::Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// Parse `bytes` through the chunked-callback API (A.3.3): the callback
/// serves the remainder of the snapshot from any requested offset and an
/// empty slice signals EOF — the exact shape the F3 overlay-snapshot parse
/// will use, so no lock needs to be held during the parse (Rule T2).
///
/// `old_tree` enables incremental re-parse (PLAN §1.2.1 step 6).
// F1 seed: its production caller is F3's parse layer; until then only the
// (dead-allowed) spike runners and tests reach it, so silence the wiring
// artifact — the same scaffold precedent as `src/error.rs` at F0.
#[allow(dead_code)]
pub fn parse_bytes(
    parser: &mut tree_sitter::Parser,
    bytes: &[u8],
    old_tree: Option<&tree_sitter::Tree>,
) -> Option<tree_sitter::Tree> {
    parser.parse_with_options(
        &mut |offset: usize, _: tree_sitter::Point| {
            if offset < bytes.len() {
                &bytes[offset..]
            } else {
                &[]
            }
        },
        old_tree,
        None,
    )
}
