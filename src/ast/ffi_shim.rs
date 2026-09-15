//! The ONLY file permitted to import `tree-sitter` types (PLAN §2, §3 F3.1,
//! ADR-0015, risk R22): a tree-sitter core pin-swap touches this file alone.
//! The compile-time fence pinning that rule lives in `src/ast/mod.rs`
//! (`tree_sitter_imports_are_confined_to_the_shim`); `send_assert.rs` is the
//! one documented exception — a canary must name exactly the types it fences.
//!
//! Everything downstream in `src/ast` reaches the types through the
//! re-export below, so the crate's tree-sitter surface is navigable from
//! this one file:
//!
//! - **Grammar registration** — one constructor per enabled cargo feature,
//!   each via the grammar crate's `LANGUAGE` const (`LanguageFn` — the
//!   current constructor; the old `language()` fn no longer exists, A.3.7).
//! - **Parsing** — [`parse_snapshot_bytes`] wraps `parse_with_options`
//!   (A.3.3; `parse_with` was removed in 0.26): the callback serves the
//!   remainder of an `Arc<[u8]>` overlay snapshot from any requested offset
//!   and an empty slice signals EOF, so no lock is held during the parse
//!   (Rule T2) and the bytes stay alive for the tree's whole life.
//!   [`parse_bytes`] is the same contract over a plain slice — the boundary
//!   the F1 spikes parse through; it stays so their measurement shapes do
//!   not move under F3.
//! - **Edit math** — [`point_at`]/[`input_edit_for`] are the PLAN §1.2.1
//!   step-4 reference implementation: `Point.column` is in BYTES (KP-3,
//!   A.3.4), never chars and never UTF-16 code units.
//! - **The exclusivity guard** — [`edit_exclusive`] is the only call site of
//!   `Tree::edit` in janitor code (Rule T1): it refuses to edit while any
//!   other `Arc<Tree>` handle exists, because `Tree::clone` is a shallow
//!   refcount-incrementing alias and `Arc::make_mut`'s clone-on-write does
//!   NOT isolate the C tree (A.3.2 forbids mutating a `TSTree` while clones
//!   exist). A typed [`ParseError::SharedTree`] — never a hidden
//!   `ts_tree_copy`, never a mutated shared tree.
//!
//! No `unsafe` will ever live in janitor code (Rule T6): all C pointer
//! management stays inside the upstream crate.
use std::sync::Arc;

use crate::error::ParseError;

/// The tree-sitter surface the rest of `src/ast` is allowed to name. Re-exported
/// here (not used directly elsewhere) so a core pin-swap re-types the whole
/// engine from this file (ADR-0015 / R22; fence test in `src/ast/mod.rs`).
pub use tree_sitter::{InputEdit, Language, Node, Parser, Point, Tree, TreeCursor};

// --- grammar registration (one fn per enabled cargo feature) ----------------
//
// Each grammar crate exposes `pub const LANGUAGE: LanguageFn`; `.into()` is
// the verified constructor (A.3.7). Runtime `set_language` compatibility
// (grammar ABI 14–15 vs core 13..=15) is build-fenced by CI's `pin-check`
// job and runtime-fenced by `LangRegistry::parser` mapping failures to
// typed `ParseError::Setup` (A.3.8).

/// The Rust grammar (`lang-rust`, the default set per ADR-0015).
#[cfg(feature = "lang-rust")]
pub fn rust_language() -> Language {
    tree_sitter_rust::LANGUAGE.into()
}

/// The Python grammar (`lang-python`).
#[cfg(feature = "lang-python")]
pub fn python_language() -> Language {
    tree_sitter_python::LANGUAGE.into()
}

/// The Go grammar (`lang-go`).
#[cfg(feature = "lang-go")]
pub fn go_language() -> Language {
    tree_sitter_go::LANGUAGE.into()
}

/// The JavaScript grammar (`lang-javascript`).
#[cfg(feature = "lang-javascript")]
pub fn javascript_language() -> Language {
    tree_sitter_javascript::LANGUAGE.into()
}

/// The TypeScript grammar (`lang-typescript`, `.ts`/`.mts`/`.cts`).
/// Does NOT parse JSX — a `.tsx` file must go to [`tsx_language`] (the
/// same crate's second grammar); routing JSX to this grammar produces
/// ERROR nodes, which the apply re-parse gate would misread as a broken
/// diff (measured at F9b; the reason tsx is its own registry entry).
#[cfg(feature = "lang-typescript")]
pub fn typescript_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
}

/// The TSX grammar (`lang-typescript` feature, `.tsx`): the same crate's
/// JSX-bearing superset of [`typescript_language`]. One cargo feature,
/// two registered languages — the multi-grammar-crate precedent (the
/// ocaml crate ships ocaml + menhir the same way).
#[cfg(feature = "lang-typescript")]
pub fn tsx_language() -> Language {
    tree_sitter_typescript::LANGUAGE_TSX.into()
}

/// The Java grammar (`lang-java`).
#[cfg(feature = "lang-java")]
pub fn java_language() -> Language {
    tree_sitter_java::LANGUAGE.into()
}

/// The C grammar (`lang-c`).
#[cfg(feature = "lang-c")]
pub fn c_language() -> Language {
    tree_sitter_c::LANGUAGE.into()
}

// --- parsing (A.3.3 contract) ------------------------------------------------

/// Parse an overlay snapshot through the chunked-callback API (PLAN §3 F3.1,
/// Rule T2): the callback serves the remainder of `bytes` starting exactly at
/// the requested offset — an empty slice means EOF. The caller keeps the
/// `Arc` alive across the call, so the parse runs against stable bytes with
/// no lock held (guard discipline, §1.1).
///
/// `old_tree` enables incremental re-parse (§1.2.1 step 6); callers must have
/// already applied the matching [`InputEdit`] through [`edit_exclusive`].
///
/// `None` is the C-level "no tree" outcome (e.g. a cancelled parse); parse
/// consumers turn it into the typed `ParseError::NoTree` — never an unwrap.
pub fn parse_snapshot_bytes(
    parser: &mut Parser,
    bytes: &Arc<[u8]>,
    old_tree: Option<&Tree>,
) -> Option<Tree> {
    parser.parse_with_options(
        &mut |offset: usize, _: Point| {
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

/// The A.3.3 contract over a plain slice: the F1 spikes' parse boundary
/// (they measure over `Vec`-backed corpora and must not pay per-iteration
/// `Arc` construction inside timed loops). Production code prefers
/// [`parse_snapshot_bytes`], which ties the parse to an owned snapshot.
pub fn parse_bytes(parser: &mut Parser, bytes: &[u8], old_tree: Option<&Tree>) -> Option<Tree> {
    parser.parse_with_options(
        &mut |offset: usize, _: Point| {
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

// --- edit math (PLAN §1.2.1 step 4; KP-3-locked byte columns) -----------------

/// Byte-based `Point` derivation: row = newline count before `byte`, column =
/// `byte` minus the start of that row. Column units are BYTES (A.3.4,
/// confirmed empirically by the KP-3 fence) — never chars, never UTF-16 code
/// units. `byte` beyond the source clamps to its end.
pub fn point_at(source: &[u8], byte: usize) -> Point {
    let byte = byte.min(source.len());
    let mut row = 0_usize;
    let mut row_start = 0_usize;
    for (i, b) in source[..byte].iter().enumerate() {
        if *b == b'\n' {
            row += 1;
            row_start = i + 1;
        }
    }
    Point {
        row,
        column: byte - row_start,
    }
}

/// Derive the [`InputEdit`] for replacing `[start_byte, old_end_byte)` of
/// `old` with the `new` buffer's `[start_byte, new_end_byte)` region (the
/// post-splice bytes; PLAN §1.2.1 step 4). Points come from `old` for the
/// old coordinates and from `new` for the new end. If tree-sitter's `Point`
/// units ever disagree with this math, the KP-3 round-trip fence fails.
pub fn input_edit_for(
    old: &[u8],
    new: &[u8],
    start_byte: usize,
    old_end_byte: usize,
    new_end_byte: usize,
) -> InputEdit {
    InputEdit {
        start_byte,
        old_end_byte,
        new_end_byte,
        start_position: point_at(old, start_byte),
        old_end_position: point_at(old, old_end_byte),
        new_end_position: point_at(new, new_end_byte),
    }
}

/// Rule T1's exclusivity guard (§1.2.1 step 5 as amended by E3(b)): edit
/// `tree` only when this handle is the ONLY one. `Tree::clone` is a shallow
/// refcount-incrementing alias (A.3.2), so `Arc::make_mut`'s clone-on-write
/// does **not** isolate the C tree — with any other handle alive, `make_mut`
/// would hand back a handle aliasing the same `TSTree` being mutated. The
/// guard refuses first, with a typed error; nothing outside this function
/// ever calls `Tree::edit`. The apply path therefore removes the
/// `ParseEntry` from the cache and takes ownership *before* reaching for
/// this (ADR-0007), and the count check makes the discipline mechanical.
pub fn edit_exclusive(tree: &mut Arc<Tree>, edit: &InputEdit) -> Result<(), ParseError> {
    let count = Arc::strong_count(tree);
    if count != 1 {
        return Err(ParseError::SharedTree { count });
    }
    Arc::make_mut(tree).edit(edit);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "lang-rust")]
    fn rust_parser() -> Parser {
        let mut parser = Parser::new();
        parser
            .set_language(&rust_language())
            .expect("rust grammar x core 0.27 is an ABI-verified pair (A.3.8)");
        parser
    }

    /// The A.3.3 callback contract: any requested offset is served from
    /// exactly there; past-the-end requests get an empty slice (EOF). A
    /// parser driven this way must produce a tree spanning the whole source.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn snapshot_parse_serves_chunks_from_the_requested_offset() {
        let bytes: Arc<[u8]> =
            Arc::from(b"fn one() -> u32 { 1 }\nfn two() -> u32 { 2 }\n".as_slice());
        let mut parser = rust_parser();

        let tree =
            parse_snapshot_bytes(&mut parser, &bytes, None).expect("valid rust parses to a tree");
        let root = tree.root_node();
        assert_eq!(root.byte_range(), 0..bytes.len());
        assert!(!root.has_error());
    }

    /// Byte-column math (KP-3): multibyte content must not shift columns —
    /// column = bytes since the row start, row = newline count.
    #[test]
    fn point_at_counts_bytes_not_chars() {
        let source = "fn a() {\n    // αβγ δε ζη θικ — CJK: 日本語\n    let x = 1;\n}\n";
        let row1_start = source.find('\n').unwrap_or_default() + 1;

        // The 'l' of `let` sits 4 columns (bytes) into row 2 — the 4-space
        // indent, not a character count.
        let let_at = source.find("let x").unwrap_or_default();
        let p = point_at(source.as_bytes(), let_at);
        assert_eq!(p.row, 2);
        assert_eq!(p.column, 4);

        // Inside the multibyte comment row: a byte offset mid-rune still
        // counts raw bytes (α = 2 bytes), so column grows by raw offset.
        let alpha = source.find('α').unwrap_or_default();
        let p = point_at(source.as_bytes(), alpha + 1);
        assert_eq!(p.row, 1);
        assert_eq!(p.column, (alpha + 1) - row1_start);

        // Clamping: past-the-end asks for EOF's point — after the fourth
        // newline, at column 0 of row 4.
        let p = point_at(source.as_bytes(), source.len() + 100);
        assert_eq!(p.row, 4);
        assert_eq!(p.column, 0);
    }

    /// `input_edit_for` composes the point/byte triples of §1.2.1 step 4:
    /// old coordinates from the old buffer, new-end from the new buffer.
    #[test]
    fn input_edit_for_derives_coordinates_from_both_buffers() {
        let old = b"fn a() {\n    let x = 1;\n}\n";
        let new = b"fn a() {\n    let renamed = 100;\n}\n";
        let row1_start = old.iter().position(|b| *b == b'\n').unwrap_or_default() + 1;

        // Replace `let x = ` with `let renamed`, anchors derived not guessed.
        let start = old
            .windows(8)
            .position(|w| w == b"let x = ")
            .unwrap_or_default();
        let old_end = start + 8;
        let new_text_len = "let renamed".len();
        let edit = input_edit_for(old, new, start, old_end, start + new_text_len);

        assert_eq!(edit.start_byte, start);
        assert_eq!(edit.old_end_byte, old_end);
        assert_eq!(edit.new_end_byte, start + new_text_len);
        assert_eq!(
            edit.start_position,
            Point {
                row: 1,
                column: start - row1_start
            }
        );
        assert_eq!(
            edit.old_end_position,
            Point {
                row: 1,
                column: old_end - row1_start
            }
        );
        assert_eq!(
            edit.new_end_position,
            Point {
                row: 1,
                column: start + new_text_len - row1_start
            }
        );
    }

    /// The guard (Rule T1 / KP-1 shared-clone leg, production boundary):
    /// a second handle alive ⇒ typed `SharedTree`, no `make_mut`, no
    /// observable mutation on either handle; the same edit goes through
    /// once the reader is gone.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn edit_exclusive_refuses_shared_trees_without_mutating() {
        let bytes: Arc<[u8]> = Arc::from(b"fn one() -> u32 { 1 }\n".as_slice());
        let mut parser = rust_parser();
        let tree = parse_snapshot_bytes(&mut parser, &bytes, None).expect("corpus parses");
        let mut owned = Arc::new(tree);

        let new_bytes: Arc<[u8]> = Arc::from(b"fn one_renamed() -> u32 { 1 }\n".as_slice());
        let edit = input_edit_for(&bytes, &new_bytes, 3, 6, 3 + "one_renamed".len());

        let observer = Arc::clone(&owned);
        let before = owned.root_node().byte_range();
        let err = edit_exclusive(&mut owned, &edit)
            .expect_err("the guard trips while a reader holds a handle");
        assert!(
            matches!(err, ParseError::SharedTree { count: 2 }),
            "typed shared-tree error, got {err:?}"
        );
        assert_eq!(Arc::strong_count(&owned), 2, "no hidden ts_tree_copy");
        assert_eq!(owned.root_node().byte_range(), before, "tree not mutated");
        assert_eq!(
            observer.root_node().byte_range(),
            before,
            "reader unaffected"
        );

        drop(observer);
        edit_exclusive(&mut owned, &edit).expect("exclusive edit succeeds");
        assert_eq!(
            owned.root_node().byte_range(),
            0..new_bytes.len(),
            "edit applied: root spans the new bytes"
        );
    }
}
