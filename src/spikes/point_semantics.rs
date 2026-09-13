//! KP-3 fence (PLAN §5 KP-3, §1.2.1 step 4): `Point` row/column semantics
//! for multibyte sources are **byte-based**, and the byte-based derivation
//! this crate will implement for `InputEdit` round-trips against the real
//! parser.
//!
//! Desk research pinned the fact (A.3.4: `Point.column` is measured in
//! bytes, standardized in tree-sitter 0.25); this spike is the empirical
//! fence — if a future release moved to UTF-8 code points or UTF-16 units,
//! these tests fail loudly instead of `apply` quietly corrupting every edit
//! after an emoji. Every position assertion derives the expected `Point`
//! with [`point_at`] — pure byte math over the source — and demands the
//! parser agree, both after a cold parse and after an
//! edit → `Tree::edit` → incremental re-parse round trip on content
//! containing emoji, CJK, combining marks and regional indicators.

/// Byte-based `Point` derivation — the exact math `ffi_shim` implements for
/// `InputEdit` (PLAN §1.2.1 step 4): row = newline count before `byte`,
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
/// this math, the round-trip tests below fail.
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

/// Corpus with the multibyte classes that break naive point math: emoji
/// (4-byte UTF-8), CJK (3-byte), a combining mark (e + U+0301 — written as
/// an escape so its byte length is guaranteed, not left to how this file
/// happens to normalize), regional indicators (flag) — and, for the column
/// assertions that matter, a statement sharing a line with multibyte bytes
/// ahead of it.
fn corpus() -> Vec<u8> {
    concat!(
        "// 🦀 emoji, 日本語 CJK, combining mark below\n",
        "const EMOJI: &str = \"🦀🦀🦀\";\n",
        "const CJK: &str = \"日本語のテキスト\";\n",
        "const COMBINING: &str = \"e\u{301}\"; // e + U+0301\n",
        "fn main() {\n",
        "    let flag = \"🇧🇷\";\n",
        "    let wide = \"日本語テキスト\"; let after_wide = wide.len();\n",
        "    let sum = EMOJI.len() + CJK.len() + COMBINING.len() + flag.len();\n",
        "    println!(\"{sum} 🦀 日本語 e\u{301}\");\n",
        "}\n",
    )
    .as_bytes()
    .to_vec()
}

/// Parser primed with the Rust grammar via the ffi_shim boundary (shared
/// spike plumbing; typed per the no-panic rule — callers `expect` in tests).
fn rust_parser() -> Result<tree_sitter::Parser, super::SpikeError> {
    super::rust_parser()
}

/// Depth-first search for the first node satisfying `pred` (kind-string
/// comparison is fine at spike scale; production resolves kind ids).
fn find_first<'t>(
    node: tree_sitter::Node<'t>,
    pred: impl Fn(tree_sitter::Node<'t>) -> bool + Copy,
) -> Option<tree_sitter::Node<'t>> {
    if pred(node) {
        return Some(node);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if let Some(hit) = find_first(child, pred) {
            return Some(hit);
        }
    }
    None
}

/// Locate the `let_declaration` binding `name` (e.g. `sum`).
fn let_decl<'t>(
    root: tree_sitter::Node<'t>,
    source: &[u8],
    name: &str,
) -> Option<tree_sitter::Node<'t>> {
    let prefix = format!("let {name}");
    find_first(root, |n| {
        n.kind() == "let_declaration"
            && n.utf8_text(source)
                .is_ok_and(|text| text.starts_with(&prefix))
    })
}

/// Assert every position of `node` equals the byte-based derivation against
/// `source` — the KP-3 claim, node by node.
fn assert_positions_derive_from_bytes(node: tree_sitter::Node<'_>, source: &[u8]) {
    assert_eq!(
        node.start_position(),
        point_at(source, node.start_byte()),
        "start position of {} must be byte-based",
        node.kind()
    );
    assert_eq!(
        node.end_position(),
        point_at(source, node.end_byte()),
        "end position of {} must be byte-based",
        node.kind()
    );
}

/// Cold-parse positions agree with byte math on multibyte content —
/// including a statement whose column sits *behind* CJK bytes on its own
/// line, where a char-based or UTF-16-based column would differ.
#[test]
fn cold_parse_positions_are_byte_based() {
    let source = corpus();
    let mut parser = rust_parser().expect("ABI-verified pair (A.3.8)");
    let tree = crate::ast::ffi_shim::parse_bytes(&mut parser, &source, None)
        .expect("corpus is valid Rust");

    let root = tree.root_node();
    assert!(!root.has_error(), "corpus must parse clean");
    assert_positions_derive_from_bytes(root, &source);

    // `after_wide` shares its row with a 21-char / 33-byte CJK literal
    // ahead of it: bytes column > chars column.
    let probe = let_decl(root, &source, "after_wide").expect("after_wide must bind");
    assert_positions_derive_from_bytes(probe, &source);
    let row_start = source[..probe.start_byte()]
        .iter()
        .rposition(|b| *b == b'\n')
        .map_or(0, |i| i + 1);
    let prefix = &source[row_start..probe.start_byte()];
    assert_eq!(
        probe.start_position().column,
        prefix.len(),
        "column is exactly the byte length of the same-row prefix"
    );
    assert!(
        String::from_utf8_lossy(prefix).chars().count() < prefix.len(),
        "prefix is genuinely multibyte, so this pins bytes-not-chars"
    );
}

/// One edit → `Tree::edit` → incremental re-parse, twice: once shifting
/// rows (a whole multibyte line inserted before the probe), once shifting a
/// column on a multibyte line (one 🦀 doubled inside the println string,
/// +4 bytes mid-line). Every step derives `InputEdit` points with
/// [`input_edit_for`]; the re-parsed tree must agree with the same math
/// against the new bytes.
#[test]
fn input_edit_round_trip_is_byte_based() {
    let source = corpus();
    let mut parser = rust_parser().expect("ABI-verified pair (A.3.8)");

    // Row-shifting edit: insert a full multibyte const line right before
    // the `let flag` line.
    let anchor = source
        .windows(10)
        .position(|w| w == b"    let fl")
        .expect("anchor line present");
    let row_edit = Splice {
        start_byte: anchor,
        old_end_byte: anchor,
        new_text: "const SHIFT: &str = \"日本語🦀挿入\";\n"
            .as_bytes()
            .to_vec(),
    };

    // Column-shifting edit: the first 🦀 at/after the println call.
    let emoji = "🦀".as_bytes();
    let println_at = source
        .windows(7)
        .position(|w| w == b"println")
        .expect("println line present");
    let col_at = println_at
        + source[println_at..]
            .windows(emoji.len())
            .position(|w| w == emoji)
            .expect("emoji on the println line");
    let col_edit = Splice {
        start_byte: col_at,
        old_end_byte: col_at + emoji.len(),
        new_text: "🦀🦀".as_bytes().to_vec(),
    };

    for edit in [row_edit, col_edit] {
        let new_source = splice(&source, &edit);
        let input_edit = input_edit_for(&source, &new_source, &edit);

        // A fresh, exclusively owned tree per iteration — Rule T1 forbids
        // editing a tree whose clones exist, so no `clone()` here.
        let mut tree = crate::ast::ffi_shim::parse_bytes(&mut parser, &source, None)
            .expect("corpus is valid Rust");
        let old_probe = let_decl(tree.root_node(), &source, "sum").expect("sum binds");
        let old_probe_start = old_probe.start_byte();
        let delta = edit.new_text.len() as isize - (edit.old_end_byte - edit.start_byte) as isize;

        tree.edit(&input_edit);
        let new_tree = crate::ast::ffi_shim::parse_bytes(&mut parser, &new_source, Some(&tree))
            .expect("re-parse of an in-language edit produces a tree");

        let root = new_tree.root_node();
        assert!(
            !root.has_error(),
            "edit keeps the source valid: no ERROR/MISSING nodes"
        );
        assert_eq!(
            root.byte_range(),
            0..new_source.len(),
            "root spans exactly the new source bytes"
        );

        // The probe moved by exactly the byte-length delta when the edit
        // precedes it, and every position still derives from byte math
        // against the new source.
        let expected_shift = if edit.start_byte < old_probe_start {
            delta
        } else {
            0
        };
        let new_probe = let_decl(root, &new_source, "sum").expect("sum still binds");
        assert_eq!(
            new_probe.start_byte() as isize,
            old_probe_start as isize + expected_shift,
            "probe shifts by exactly the byte delta"
        );
        assert_positions_derive_from_bytes(new_probe, &new_source);
        assert_positions_derive_from_bytes(root, &new_source);
    }
}

/// The deriving math itself is pinned: newline counting, column reset, and
/// clamping at EOF. This is the unit contract `ffi_shim` will embed.
#[test]
fn point_at_math_is_pinned() {
    let src = "ab\n🦀x\n日".as_bytes();
    assert_eq!(point_at(src, 0), tree_sitter::Point { row: 0, column: 0 });
    assert_eq!(point_at(src, 2), tree_sitter::Point { row: 0, column: 2 });
    // "ab\n" is 3 bytes; the emoji is 4 bytes, so byte 7 sits right past it.
    assert_eq!(point_at(src, 7), tree_sitter::Point { row: 1, column: 4 });
    assert_eq!(point_at(src, 8), tree_sitter::Point { row: 1, column: 5 });
    // Byte 9 starts 日 (3 bytes): bytes 9/10/11 are row 2, columns 0/1/2.
    assert_eq!(point_at(src, 10), tree_sitter::Point { row: 2, column: 1 });
    assert_eq!(point_at(src, 12), tree_sitter::Point { row: 2, column: 3 });
    // Out-of-range byte offsets clamp to EOF rather than panic.
    assert_eq!(point_at(src, 99), point_at(src, src.len()));
}
