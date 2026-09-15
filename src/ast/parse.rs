//! Snapshot parsing and the guarded edit/re-parse choreography
//! (PLAN §1.2.1, §3 F3.3).
//!
//! Two entry points, one discipline:
//!
//! - [`parse_snapshot`] — a full parse of an `Arc<[u8]>` overlay snapshot.
//!   The parse itself holds no lock (the caller owns the `Arc`; the ffi
//!   callback reads it — Rule T2), and bytes and tree travel together from
//!   here on: every `Node` the tree hands out is only ever interpreted
//!   against the exact bytes it was parsed from.
//! - [`reparse_edited`] — §1.2.1 steps 3–6 as one primitive: validate the
//!   [`Splice`] against the old bytes, splice the new bytes, derive the
//!   `InputEdit` (byte-column points, KP-3), apply it through the
//!   exclusivity guard (§1.2.1 step 5 as amended by E3(b)), and re-parse
//!   with the old tree (the KP-1-measured fast path, 6.0–7.4× at every
//!   size). The guard makes the ownership rule mechanical: the caller
//!   passes `&mut Arc<Tree>` it has already taken exclusive ownership of
//!   (F5's apply removes the `ParseEntry` from the cache first); a second
//!   live handle fails to the typed `ParseError::SharedTree` before any
//!   mutation — `Arc::make_mut`'s clone-on-write does not isolate the C
//!   tree (A.3.2).
//!
//! Multi-edit composition (descending-by-offset fold, §1.2.1 steps 2–3) is
//! the F5 apply loop's job; this primitive is the single-edit core it folds
//! over. `child_count()` being `u32` in 0.27 (A.3.5) is invisible here: the
//! choreography only ever deals in byte offsets and trees.

use std::sync::Arc;

use crate::ast::ffi_shim::{self, Parser, Tree};
use crate::error::ParseError;

/// A byte splice over a snapshot: replace `[start_byte, old_end_byte)` with
/// `new_text` — half-open UTF-8 byte ranges, ADR-0014's span model.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Splice {
    pub start_byte: usize,
    pub old_end_byte: usize,
    pub new_text: Vec<u8>,
}

impl Splice {
    /// The first byte past the replacement in new-byte coordinates.
    pub fn new_end_byte(&self) -> usize {
        self.start_byte + self.new_text.len()
    }

    /// §1.2.1 step 2's structural precondition (the expected-bytes half is
    /// F5's payload validation): the range must be well-formed and inside
    /// the old bytes.
    pub fn validate_against(&self, len: usize) -> Result<(), ParseError> {
        if self.start_byte > self.old_end_byte || self.old_end_byte > len {
            return Err(ParseError::EditRange {
                start: self.start_byte,
                old_end: self.old_end_byte,
                len,
            });
        }
        Ok(())
    }
}

/// Parse a snapshot with no lock held (Rule T2). The caller owns the `Arc`
/// and keeps it alive for the tree's whole life (bytes and tree die
/// together — §1.2 Rule T2); the returned tree's offsets index exactly
/// `bytes`.
///
/// The C-level "no tree" outcome is the typed `ParseError::NoTree`.
pub fn parse_snapshot(parser: &mut Parser, bytes: &Arc<[u8]>) -> Result<Tree, ParseError> {
    ffi_shim::parse_snapshot_bytes(parser, bytes, None).ok_or_else(|| ParseError::NoTree {
        detail: format!("full parse of {} snapshot bytes", bytes.len()),
    })
}

/// Apply `splice` to `old`, producing the new bytes (§1.2.1 step 3).
/// Validation happens before any allocation of the output.
pub fn splice_bytes(old: &[u8], splice: &Splice) -> Result<Vec<u8>, ParseError> {
    splice.validate_against(old.len())?;
    let mut out = Vec::with_capacity(old.len() + splice.new_text.len());
    out.extend_from_slice(&old[..splice.start_byte]);
    out.extend_from_slice(&splice.new_text);
    out.extend_from_slice(&old[splice.old_end_byte..]);
    Ok(out)
}

/// §1.2.1 steps 3–6: splice the new bytes, edit the (exclusively owned) old
/// tree, and re-parse against the old tree. Returns the new snapshot and its
/// tree as a pair — the (bytes, tree) unit the `ParseEntry` cache stores
/// (F3.4) and the F5 apply loop validates.
///
/// `old_tree` must be the caller's *only* handle; the guard inside refuses
/// otherwise, leaving both the old tree and the cache untouched (the typed
/// `ParseError::SharedTree` — never a hidden `ts_tree_copy`, never a
/// mutated shared tree). On success the old tree's inner `TSTree` has been
/// edited to new-byte coordinates; it is superseded by the returned tree
/// and simply drops when the caller's ownership ends.
pub fn reparse_edited(
    parser: &mut Parser,
    old_bytes: &Arc<[u8]>,
    old_tree: &mut Arc<Tree>,
    splice: &Splice,
) -> Result<(Arc<[u8]>, Tree), ParseError> {
    let new_bytes: Arc<[u8]> = Arc::from(splice_bytes(old_bytes, splice)?);
    let edit = ffi_shim::input_edit_for(
        old_bytes,
        &new_bytes,
        splice.start_byte,
        splice.old_end_byte,
        splice.new_end_byte(),
    );
    ffi_shim::edit_exclusive(old_tree, &edit)?;

    let tree =
        ffi_shim::parse_snapshot_bytes(parser, &new_bytes, Some(old_tree)).ok_or_else(|| {
            ParseError::NoTree {
                detail: format!("incremental re-parse of {} new bytes", new_bytes.len()),
            }
        })?;
    Ok((new_bytes, tree))
}

/// §1.2.1 steps 3–6 folded over a file's whole edit set (the F5 apply
/// loop's core): apply every splice's `InputEdit` to the (exclusively
/// owned) old tree **descending by offset** — the same ordering as the
/// byte splice, so each edit's original coordinates still address the
/// right tree positions when its turn comes (every already-applied edit
/// lives entirely at higher offsets) — then re-parse **once** against the
/// final new bytes. `splices` must be ascending, non-overlapping, and
/// in-range (the apply path validates this before calling);
/// `new_bytes` must be their splice of `old_bytes` (the caller computes it
/// once via `apply::splice::splice_descending` and shares the `Arc`).
///
/// The exclusivity guard runs per edit (Rule T1): a second live handle
/// fails the whole fold to the typed `ParseError::SharedTree` before any
/// mutation — the bytes never moved and the caller discards the taken
/// entry.
pub fn reparse_edited_multi(
    parser: &mut Parser,
    old_bytes: &Arc<[u8]>,
    old_tree: &mut Arc<Tree>,
    splices: &[Splice],
    new_bytes: &Arc<[u8]>,
) -> Result<Tree, ParseError> {
    for splice in splices.iter().rev() {
        splice.validate_against(old_bytes.len())?;
        let start_position = ffi_shim::point_at(old_bytes, splice.start_byte);
        let edit = ffi_shim::InputEdit {
            start_byte: splice.start_byte,
            old_end_byte: splice.old_end_byte,
            new_end_byte: splice.new_end_byte(),
            start_position,
            old_end_position: ffi_shim::point_at(old_bytes, splice.old_end_byte),
            // The region between start and the new end is exactly the
            // inserted bytes (the replacement's remainder lies beyond
            // old_end, untouched by later-applied higher edits), so the
            // new end's position is the start's position advanced across
            // new_text.
            new_end_position: point_after(start_position, &splice.new_text),
        };
        ffi_shim::edit_exclusive(old_tree, &edit)?;
    }
    ffi_shim::parse_snapshot_bytes(parser, new_bytes, Some(old_tree)).ok_or_else(|| {
        ParseError::NoTree {
            detail: format!(
                "incremental multi-edit re-parse of {} new bytes",
                new_bytes.len()
            ),
        }
    })
}

/// The position of `start + inserted.len()` in the post-insertion frame:
/// rows cross at each newline, and the column is raw bytes since the last
/// one (columns are BYTES — KP-3, A.3.4). The descending-fold companion of
/// `point_at` for an edit's new end.
fn point_after(start: ffi_shim::Point, inserted: &[u8]) -> ffi_shim::Point {
    match inserted.iter().rposition(|b| *b == b'\n') {
        None => ffi_shim::Point {
            row: start.row,
            column: start.column + inserted.len(),
        },
        Some(last_nl) => ffi_shim::Point {
            row: start.row + inserted.iter().filter(|b| **b == b'\n').count(),
            column: inserted.len() - last_nl - 1,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;

    #[cfg(feature = "lang-rust")]
    fn fixture() -> (LangRegistry, crate::ast::lang::LangId, Parser) {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        let parser = registry.parser(id).expect("ABI-verified pair (A.3.8)");
        (registry, id, parser)
    }

    /// A synthetic-but-real-shaped corpus with unique anchors, unlike the
    /// fixtures: the choreography tests mutate it per case.
    #[cfg(feature = "lang-rust")]
    fn corpus() -> Vec<u8> {
        b"use std::sync::Arc;\n\
          \n\
          fn alpha(x: u32) -> u32 {\n\
          \x20   let base = x + 1;\n\
          \x20   base * 2\n\
          }\n\
          \n\
          // free-floating comment between items\n\
          fn beta(flag: bool) -> bool {\n\
          \x20   if flag && (7 % 2 == 0) {\n\
          \x20       true\n\
          \x20   } else {\n\
          \x20       false\n\
          \x20   }\n\
          }\n\
          \n\
          fn gamma() -> u32 {\n\
          \x20   match 3 {\n\
          \x20       0 => 0,\n\
          \x20       n if n % 3 == 0 => n * 3,\n\
          \x20       _ => 4,\n\
          \x20   }\n\
          }\n"
        .to_vec()
    }

    #[cfg(feature = "lang-rust")]
    fn find(haystack: &[u8], needle: &[u8]) -> usize {
        haystack
            .windows(needle.len())
            .position(|w| w == needle)
            .unwrap_or_default()
    }

    /// Rule T2: the parse runs against the snapshot the caller owns and the
    /// tree spans exactly those bytes.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn parse_snapshot_binds_tree_offsets_to_the_snapshot_bytes() {
        let (registry, id, mut parser) = fixture();
        let bytes: Arc<[u8]> = Arc::from(corpus());

        let tree = parse_snapshot(&mut parser, &bytes).expect("corpus is valid rust");
        let root = tree.root_node();
        assert_eq!(root.byte_range(), 0..bytes.len());
        assert!(!root.has_error());
        let source_file = registry
            .resolve_kind(id, "source_file")
            .expect("the rust grammar names its root kind");
        assert_eq!(
            root.kind_id(),
            source_file,
            "a rust parse roots at source_file"
        );
    }

    /// KP-1's correctness contract through the production API: for rename,
    /// insertion and deletion shapes, the choreography lands on the same
    /// tree a full re-parse of the new bytes builds (root facts plus the
    /// whole-tree s-expression).
    #[test]
    #[cfg(feature = "lang-rust")]
    fn reparse_edited_lands_on_the_full_parse_tree() {
        let (_registry, _id, mut parser) = fixture();
        let old: Arc<[u8]> = Arc::from(corpus());

        let rename_at = find(&old, b"alpha");
        let stmt_at = find(&old, b"    let base = x + 1;\n") + "    ".len();
        let brace_at = find(&old, b"    base * 2\n") + "    base * 2\n".len();

        let cases: Vec<(&str, Splice)> = vec![
            (
                "rename",
                Splice {
                    start_byte: rename_at,
                    old_end_byte: rename_at + "alpha".len(),
                    new_text: b"alpha_renamed".to_vec(),
                },
            ),
            (
                "insertion",
                Splice {
                    start_byte: stmt_at,
                    old_end_byte: stmt_at,
                    new_text: b"let probe = 2;\n    ".to_vec(),
                },
            ),
            (
                "deletion",
                Splice {
                    start_byte: brace_at.saturating_sub(1),
                    old_end_byte: brace_at,
                    new_text: Vec::new(),
                },
            ),
        ];

        for (name, splice) in cases {
            let mut old_tree = Arc::new(parse_snapshot(&mut parser, &old).expect("old parses"));
            let (new_bytes, tree) = reparse_edited(&mut parser, &old, &mut old_tree, &splice)
                .expect("the choreography succeeds (case name in the asserts below)");

            let full = parse_snapshot(&mut parser, &new_bytes).expect("the full re-parse succeeds");
            assert_eq!(
                tree.root_node().kind_id(),
                full.root_node().kind_id(),
                "{name}"
            );
            assert_eq!(
                tree.root_node().byte_range(),
                full.root_node().byte_range(),
                "{name}"
            );
            assert_eq!(
                tree.root_node().has_error(),
                full.root_node().has_error(),
                "{name}"
            );
            assert!(
                !full.root_node().has_error(),
                "{name}: edits stay valid rust"
            );
            assert_eq!(
                tree.root_node().to_sexp(),
                full.root_node().to_sexp(),
                "{name}: incremental reuse must be byte-identical to a fresh parse"
            );
            assert_eq!(
                tree.root_node().byte_range(),
                0..new_bytes.len(),
                "{name}: the returned tree indexes the returned bytes (T2)"
            );
        }
    }

    /// The E3(b) guard through the full choreography: with a reader holding
    /// a second handle, `reparse_edited` fails to the typed
    /// `ParseError::SharedTree` having mutated nothing; the identical edit
    /// succeeds once the reader is gone.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn reparse_edited_refuses_shared_trees_leaving_no_partial_state() {
        let (_registry, _id, mut parser) = fixture();
        let old: Arc<[u8]> = Arc::from(corpus());
        let mut old_tree = Arc::new(parse_snapshot(&mut parser, &old).expect("corpus parses"));

        let at = find(&old, b"beta");
        let splice = Splice {
            start_byte: at,
            old_end_byte: at + "beta".len(),
            new_text: b"beta_renamed".to_vec(),
        };

        let reader = Arc::clone(&old_tree);
        let before = old_tree.root_node().byte_range();
        let err = reparse_edited(&mut parser, &old, &mut old_tree, &splice)
            .expect_err("the choreography must refuse a shared tree");
        assert!(
            matches!(err, ParseError::SharedTree { count: 2 }),
            "got {err:?}"
        );
        assert_eq!(Arc::strong_count(&old_tree), 2, "no hidden ts_tree_copy");
        assert_eq!(old_tree.root_node().byte_range(), before, "tree unmutated");
        assert_eq!(reader.root_node().byte_range(), before, "reader unmutated");

        // Reader gone: the identical splice goes through and the retry sees
        // the same pre-edit state (the refusal left no partial state).
        drop(reader);
        let (new_bytes, tree) = reparse_edited(&mut parser, &old, &mut old_tree, &splice)
            .expect("exclusive choreography succeeds");
        assert_eq!(tree.root_node().byte_range(), 0..new_bytes.len());
        assert!(
            !tree.root_node().has_error(),
            "the rename keeps the file valid"
        );
    }

    /// The KP-1 correctness contract extended to the multi-edit fold: a
    /// file's whole edit set — including length-shifting edits earlier in
    /// the file than later ones, the exact shape that makes ordering matter
    /// — must land on the same tree a full re-parse of the new bytes
    /// builds. The splices here are deliberately given in ascending order
    /// with offsets against the ORIGINAL bytes, as the apply payload
    /// supplies them.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn multi_edit_reparse_lands_on_the_full_parse_tree() {
        use crate::apply::splice::splice_descending;

        let (_registry, _id, mut parser) = fixture();
        let old: Arc<[u8]> = Arc::from(corpus());

        let rename_at = find(&old, b"alpha");
        let stmt_at = find(&old, b"    let base = x + 1;\n") + "    ".len();
        let gamma_at = find(&old, b"gamma");
        let times_at = gamma_at + find(&old[gamma_at..], b" * 3");

        let splices: Vec<Splice> = vec![
            // Grows by 4 bytes before the next edit's site.
            Splice {
                start_byte: rename_at,
                old_end_byte: rename_at + "alpha".len(),
                new_text: b"alpha_two".to_vec(),
            },
            // Pure insertion at an offset the first edit shifted past.
            Splice {
                start_byte: stmt_at,
                old_end_byte: stmt_at,
                new_text: b"let probe = 2;\n    ".to_vec(),
            },
            // A later rename keeps the offsets honest mid-file.
            Splice {
                start_byte: gamma_at,
                old_end_byte: gamma_at + "gamma".len(),
                new_text: b"delta".to_vec(),
            },
            // Pure deletion at the last edit's site.
            Splice {
                start_byte: times_at,
                old_end_byte: times_at + " * 3".len(),
                new_text: Vec::new(),
            },
        ];

        let new_bytes = splice_descending(&old, &splices).expect("validated splices fold");
        let mut old_tree = Arc::new(parse_snapshot(&mut parser, &old).expect("old parses"));
        let tree = reparse_edited_multi(&mut parser, &old, &mut old_tree, &splices, &new_bytes)
            .expect("the fold succeeds");

        let full = parse_snapshot(&mut parser, &new_bytes).expect("full re-parse succeeds");
        assert_eq!(
            tree.root_node().to_sexp(),
            full.root_node().to_sexp(),
            "incremental reuse must be byte-identical to a fresh parse"
        );
        assert_eq!(tree.root_node().byte_range(), 0..new_bytes.len());
        assert!(
            !tree.root_node().has_error(),
            "the edits keep the corpus valid rust"
        );

        // The fold refuses a shared tree per edit (the guard runs inside
        // the loop), with nothing mutated on either handle.
        let mut shared = Arc::new(parse_snapshot(&mut parser, &old).expect("old parses"));
        let reader = Arc::clone(&shared);
        let err = reparse_edited_multi(&mut parser, &old, &mut shared, &splices, &new_bytes)
            .expect_err("a reader is holding a handle");
        assert!(
            matches!(err, ParseError::SharedTree { count: 2 }),
            "got {err:?}"
        );
        assert_eq!(Arc::strong_count(&shared), 2, "no hidden ts_tree_copy");
        assert_eq!(
            shared.root_node().byte_range(),
            reader.root_node().byte_range()
        );
    }

    /// `point_after`: columns stay bytes, rows cross at newlines, and an
    /// insertion ending on a newline parks the new end at column 0 of the
    /// next row — the frame `point_at` would report on the spliced buffer.
    #[test]
    fn point_after_advances_rows_and_byte_columns() {
        use crate::ast::ffi_shim::Point;

        let start = Point { row: 3, column: 8 };
        let same_line = point_after(start, b"renamed");
        assert_eq!(same_line, Point { row: 3, column: 15 });

        let crossed = point_after(start, b"one\ntwo\nthree");
        // 11 bytes after the last newline ("three").
        assert_eq!(crossed, Point { row: 5, column: 5 });

        let trailing = point_after(start, b"tail\n");
        assert_eq!(trailing, Point { row: 4, column: 0 });

        let empty = point_after(start, b"");
        assert_eq!(empty, start);
    }

    /// Range validation (§1.2.1 step 2's structural half): inverted or
    /// out-of-bounds splices are the typed `ParseError::EditRange` before
    /// any bytes move.
    #[test]
    fn splices_out_of_range_are_typed_errors() {
        let old = b"fn a() {}".as_slice();
        let inverted = Splice {
            start_byte: 5,
            old_end_byte: 2,
            new_text: Vec::new(),
        };
        assert!(matches!(
            inverted.validate_against(old.len()),
            Err(ParseError::EditRange { .. })
        ));
        let beyond = Splice {
            start_byte: 0,
            old_end_byte: old.len() + 1,
            new_text: Vec::new(),
        };
        assert!(matches!(
            beyond.validate_against(old.len()),
            Err(ParseError::EditRange { .. })
        ));
        assert!(splice_bytes(old, &inverted).is_err());
        assert!(splice_bytes(old, &beyond).is_err());

        // The boundary itself is fine: replacing the whole file, and an
        // empty splice at EOF.
        let whole = Splice {
            start_byte: 0,
            old_end_byte: old.len(),
            new_text: b"fn b() {}".to_vec(),
        };
        let out = splice_bytes(old, &whole).expect("whole-file replacement is in range");
        assert_eq!(out.as_slice(), b"fn b() {}".as_slice());
        let tail = Splice {
            start_byte: old.len(),
            old_end_byte: old.len(),
            new_text: b"\n".to_vec(),
        };
        assert_eq!(tail.new_end_byte(), old.len() + 1);
        let out = splice_bytes(old, &tail).expect("empty splice at EOF is in range");
        assert_eq!(out.as_slice(), b"fn a() {}\n".as_slice());
    }
}
