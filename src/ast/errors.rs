//! ERROR/MISSING counting and the validity-gate basis (PLAN §1.2.1 step 7,
//! §1.6, §3 F3.5; KP-4 outcome).
//!
//! **The v0.1 apply gate is whole-file** (`error_count == 0` on the
//! re-parsed entry): the localized variant — errors within the edited byte
//! ranges only — measured a 1/10 false-negative rate on the mangled corpus
//! (a MISSING `;` parks at the *next* statement's start, byte-adjacent but
//! outside the half-open range), which is exactly the dishonesty risk R7
//! exists for. Whole-file is also the cheaper gate: `error_count` is
//! precomputed on the `ParseEntry` at parse time, with `has_error` as the
//! O(1) subtree flag keeping clean files walk-free.
//!
//! [`errors_in_range`] remains available (reports intersecting a byte
//! range) for diagnostics and the F5 `E_PARSE_INVALID` payload, but it is
//! explicitly *not* the gate — the corpus test pins the known
//! false-negative case so the boundary stays honest.
//!
//! Reports are plain copied values (Rule T4a): no `Node` handles cross the
//! work item, nothing outlives its `Arc<Tree>`.

use std::ops::Range;

use crate::ast::ffi_shim::{Node, Point};
use crate::ast::lang::BlockComment;
use crate::ast::walk::CursorPool;

/// One ERROR or MISSING node, as copied values (Rule T4a) — the
/// `E_PARSE_INVALID` payload shape for F5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ErrorReport {
    pub kind_id: u16,
    /// `true` for MISSING nodes (recovery inserted), `false` for ERROR
    /// nodes (unparseable region).
    pub is_missing: bool,
    pub start_byte: usize,
    pub end_byte: usize,
    pub start_point: Point,
    pub end_point: Point,
}

/// Whole-file ERROR/MISSING count (KP-4 outcome; the v0.1 gate basis).
/// Clean trees return without a walk (`has_error` is the O(1) subtree flag);
/// dirty trees pay one traversal. MISSING nodes contribute — pinned by the
/// corpus test, which includes missing-only files.
pub fn error_count(root: Node<'_>) -> u32 {
    if !root.has_error() {
        return 0;
    }
    let mut reports = Vec::new();
    collect_error_reports(root, &mut reports);
    reports.len() as u32
}

/// Every ERROR/MISSING node in the tree, whole-file. Empty when the tree
/// parses clean.
pub fn error_reports(root: Node<'_>) -> Vec<ErrorReport> {
    let mut reports = Vec::new();
    if root.has_error() {
        collect_error_reports(root, &mut reports);
    }
    reports
}

/// Reports whose span intersects the half-open `range` (new-byte
/// coordinates) — the **diagnostic** query, not the gate: on the KP-4
/// corpus it misses 1/10 real errors by design of grammar error recovery.
pub fn errors_in_range(root: Node<'_>, range: Range<usize>) -> Vec<ErrorReport> {
    error_reports(root)
        .into_iter()
        .filter(|report| report.start_byte < range.end && range.start < report.end_byte)
        .collect()
}

/// Walk the whole tree with the pooled cursors (Rule T5), collecting
/// ERROR/MISSING nodes as plain values. The `has_error` root gate (the
/// callers') keeps clean files walk-free; a dirty file pays one
/// allocation-free traversal.
fn collect_error_reports(root: Node<'_>, out: &mut Vec<ErrorReport>) {
    let mut pool = CursorPool::new();
    for node in pool.dfs(root) {
        if node.is_error() || node.is_missing() {
            out.push(ErrorReport {
                kind_id: node.kind_id(),
                is_missing: node.is_missing(),
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                start_point: node.start_position(),
                end_point: node.end_position(),
            });
        }
    }
}

/// Whole-tree node count (F10a): one pooled-cursor walk, the basis of a
/// [`crate::ast::cache::ParseEntry`]'s retained-cost estimate — the daemon's
/// LRU bound (`--max-cache-mb`, ADR-0018 D4) accounts trees, not just source
/// bytes, because the tree is the dominant retention cost of a warm entry.
pub fn node_count(root: Node<'_>) -> u64 {
    let mut pool = CursorPool::new();
    pool.dfs(root).count() as u64
}

/// One comment node whose span runs to the file's last byte without a
/// balancing close delimiter — the unclosed-comment honesty probe's report
/// (B9). Plain copied values (Rule T4a).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UnclosedComment {
    pub kind_id: u16,
    pub start_byte: usize,
    pub end_byte: usize,
}

/// The B9 honesty probe, run AFTER the whole-file error gate passes: some
/// grammars' scanners recover an unclosed block comment silently — ocaml
/// measured: `(* …` to EOF parses with ZERO ERROR/MISSING nodes and the
/// entire post-`(*` tail becomes one `comment` node — so `error_count == 0`
/// is not the whole truth. The probe descends the rightmost spine of nodes
/// ending exactly at EOF (a scanner that ran out of input consumes to the
/// last byte, measured) and reports the tail node iff it is a comment-class
/// node whose text opens the language's block-comment delimiter and never
/// balances. Line comments (`#`, `//`) cannot be unclosed and never trigger
/// it; languages without block comments (python, ruby, elixir) are
/// vacuously clean. Lexing of one case, never semantics (ADR-0017).
///
/// Residual, documented (docs/SCHEMA.md): an unclosed *string* containing
/// the close delimiter inside the unclosed comment (`(* "*)` to EOF) still
/// balances naively and passes — closing that would mean re-implementing
/// the grammar's string-in-comment scanner, i.e. a real lexer.
pub fn unclosed_comment_at_eof(
    root: Node<'_>,
    bytes: &[u8],
    trivia_kinds: &[&str],
    block: Option<BlockComment>,
) -> Option<UnclosedComment> {
    let block = block?;
    if root.end_byte() != bytes.len() {
        return None;
    }
    // The deepest rightmost node ending exactly at EOF: children are in
    // document order, so indices scanned from the end find the tail child
    // in O(1) for every measured shape (the walk is O(depth), not O(n) —
    // the clean-tree gate stays walk-free plus spine).
    let mut node = root;
    loop {
        let next = (0..node.child_count())
            .rev()
            .filter_map(|i| node.child(i))
            .find(|child| child.end_byte() == bytes.len());
        match next {
            Some(child) => node = child,
            None => break,
        }
    }
    if !trivia_kinds.iter().any(|kind| *kind == node.kind()) {
        return None;
    }
    let text = bytes.get(node.start_byte()..node.end_byte())?;
    if !text.starts_with(block.open) || balances(text, block) {
        return None;
    }
    Some(UnclosedComment {
        kind_id: node.kind_id(),
        start_byte: node.start_byte(),
        end_byte: node.end_byte(),
    })
}

/// Lexical balance of one block comment's bytes, under the language's own
/// nesting law: the caller has already matched `block.open` at the start;
/// nesting languages count opens against closes, first-close languages are
/// satisfied by the first close (a legal C comment may contain a bare
/// `/*`). Overlapping-delimiter shapes (`(*)`, `/*/`) land on the
/// conservative side: no close sequence, no balance.
fn balances(text: &[u8], block: BlockComment) -> bool {
    let at = |i: usize, pat: &[u8]| -> bool {
        text.len() - i >= pat.len() && &text[i..i + pat.len()] == pat
    };
    let mut i = block.open.len();
    let mut depth = 1usize;
    while i < text.len() {
        if at(i, block.close) {
            depth -= 1;
            if depth == 0 {
                return true;
            }
            i += block.close.len();
        } else if block.nests && at(i, block.open) {
            depth += 1;
            i += block.open.len();
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::parse::{Splice, parse_snapshot, reparse_edited};
    use std::sync::Arc;

    /// The KP-4 mangled corpus (10 files, one distinct error class each),
    /// with the counts the F1 spike measured under this exact grammar pin
    /// (memo `docs/planning/spikes/KP-4.md`). `(errors, missing)` totals
    /// whole-file, then `(errors, missing)` restricted to the edited range
    /// in new-byte coordinates — the second pair is what the *localized*
    /// gate would see.
    struct Mangle {
        name: &'static str,
        valid: &'static str,
        needle: &'static str,
        replacement: &'static str,
        errors_total: u32,
        missing_total: u32,
        errors_in_range: u32,
        missing_in_range: u32,
    }

    fn corpus() -> Vec<Mangle> {
        vec![
            Mangle {
                name: "deleted-fn-close-brace",
                valid: "fn gate_a(x: u32) -> u32 {\n    let y = x + 1;\n    y\n}\n",
                needle: "    y\n}\n",
                replacement: "    y\n",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "deleted-let-semicolon",
                valid: "fn gate_b() -> u32 {\n    let a = 1;\n    let b = 2;\n    a + b\n}\n",
                needle: "let a = 1;",
                replacement: "let a = 1",
                // THE false negative: the MISSING `;` parks at the next
                // statement's start — outside the half-open edited range.
                errors_total: 0,
                missing_total: 1,
                errors_in_range: 0,
                missing_in_range: 0,
            },
            Mangle {
                name: "deleted-call-close-paren",
                valid: "fn gate_c(v: Vec<u32>) -> u32 {\n    v.len() + v.len()\n}\n",
                needle: "v.len() +",
                replacement: "v.len( +",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "missing-initializer",
                valid: "fn gate_d() -> u32 {\n    let a = 1;\n    a\n}\n",
                needle: "let a = 1;",
                replacement: "let b = ;",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "extra-open-brace",
                valid: "fn gate_e() -> u32 {\n    let a = 1;\n    a\n}\n",
                needle: "let a = 1;",
                replacement: "{ let a = 1;",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "deleted-let-equals",
                valid: "fn gate_f() -> u32 {\n    let a = 1;\n    a\n}\n",
                needle: "let a = 1;",
                replacement: "let a  1;",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "deleted-match-arrow",
                valid: "fn gate_g(n: u32) -> u32 {\n    match n {\n        0 => 0,\n        _ => 1,\n    }\n}\n",
                needle: "0 => 0",
                replacement: "0  0",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "unterminated-string",
                valid: "fn gate_h() -> &'static str {\n    \"closed\"\n}\n",
                needle: "\"closed\"",
                replacement: "\"opened",
                errors_total: 1,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
            Mangle {
                name: "deleted-array-close-bracket",
                valid: "fn gate_i() -> u32 {\n    let xs = [1, 2, 3];\n    xs.len() as u32\n}\n",
                needle: "[1, 2, 3];",
                replacement: "[1, 2, 3 ;",
                errors_total: 0,
                missing_total: 1,
                errors_in_range: 0,
                missing_in_range: 1,
            },
            Mangle {
                name: "deleted-fn-name",
                valid: "fn gate_j() -> u32 {\n    7\n}\n",
                needle: "fn gate_j",
                replacement: "fn ",
                errors_total: 2,
                missing_total: 0,
                errors_in_range: 1,
                missing_in_range: 0,
            },
        ]
    }

    /// The shared rust-registry helper (B6: one definition, in
    /// `crate::ast::testing`).
    #[cfg(feature = "lang-rust")]
    use crate::ast::testing::rust as setup;

    /// The KP-4 corpus against the production surface: every pristine file
    /// parses clean under the whole-file count; every mangle goes through
    /// the guarded choreography and lands on the memo's measured counts —
    /// whole-file AND range-restricted. The deleted-let-semicolon row is
    /// the decision: range 0 / whole-file 1 is precisely why the gate is
    /// whole-file.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn kp4_corpus_counts_match_the_measured_memo() {
        let (registry, id) = setup();
        let mut parser = registry.parser(id).expect("ABI-verified pair");

        for mangle in corpus() {
            let valid: Arc<[u8]> = Arc::from(mangle.valid.as_bytes());
            let pristine = parse_snapshot(&mut parser, &valid).expect("pristine source parses");
            assert_eq!(
                error_count(pristine.root_node()),
                0,
                "{}: the pristine corpus must parse clean",
                mangle.name
            );

            let at = mangle
                .valid
                .find(mangle.needle)
                .expect("corpus needle present");
            let splice = Splice {
                start_byte: at,
                old_end_byte: at + mangle.needle.len(),
                new_text: mangle.replacement.as_bytes().to_vec(),
            };
            let mut old_tree = Arc::new(pristine);
            let (new_bytes, tree) = reparse_edited(&mut parser, &valid, &mut old_tree, &splice)
                .expect("choreography accepts the exclusively owned tree");
            let root = tree.root_node();

            // Whole-file: the gate basis — exact memo counts.
            let reports = error_reports(root);
            let errors = reports.iter().filter(|r| !r.is_missing).count() as u32;
            let missing = reports.iter().filter(|r| r.is_missing).count() as u32;
            assert_eq!(errors, mangle.errors_total, "{}: ERROR nodes", mangle.name);
            assert_eq!(
                missing, mangle.missing_total,
                "{}: MISSING nodes",
                mangle.name
            );
            assert_eq!(
                error_count(root),
                mangle.errors_total + mangle.missing_total
            );

            // MISSING-only files must still trip the has_error fast path's
            // precondition — the count above already proves it; make the
            // property explicit so a future "optimization" cannot drop it.
            if mangle.missing_total > 0 && mangle.errors_total == 0 {
                assert!(
                    root.has_error(),
                    "{}: MISSING nodes raise the subtree flag",
                    mangle.name
                );
            }

            // Range-restricted (the query that is NOT the gate): the edited
            // range in new-byte coordinates, memo counts — including the
            // false-negative row.
            let range = splice.start_byte..splice.new_end_byte();
            let in_range = errors_in_range(root, range);
            let range_errors = in_range.iter().filter(|r| !r.is_missing).count() as u32;
            let range_missing = in_range.iter().filter(|r| r.is_missing).count() as u32;
            assert_eq!(
                range_errors, mangle.errors_in_range,
                "{}: in-range ERROR",
                mangle.name
            );
            assert_eq!(
                range_missing, mangle.missing_in_range,
                "{}: in-range MISSING",
                mangle.name
            );
            if mangle.errors_total + mangle.missing_total > 0
                && mangle.errors_in_range + mangle.missing_in_range == 0
            {
                // The localized gate would call this file "valid".
                assert_eq!(
                    mangle.name, "deleted-let-semicolon",
                    "the memo measured exactly one false negative; a second means the grammar pin moved"
                );
            }

            // Paths agree: a full re-parse of the mangled bytes counts the
            // same as the choreography's incremental tree.
            let full = parse_snapshot(&mut parser, &new_bytes)
                .expect("mangled bytes still produce a tree");
            assert_eq!(
                error_count(full.root_node()),
                error_count(root),
                "{}: incremental and full parses agree",
                mangle.name
            );

            // Reports are plain data with in-file spans (T4a).
            for report in &reports {
                assert!(
                    report.end_byte <= new_bytes.len(),
                    "{}: span in file",
                    mangle.name
                );
            }
        }
    }

    /// Clean trees cost no walk and report nothing; the empty range matches
    /// nothing even on dirty trees.
    #[test]
    #[cfg(feature = "lang-rust")]
    fn clean_trees_report_nothing_and_ranges_filter() {
        let (registry, id) = setup();
        let mut parser = registry.parser(id).expect("ABI-verified pair");

        let clean: Arc<[u8]> = Arc::from(b"fn ok() -> u32 { 1 }\n".as_slice());
        let tree = parse_snapshot(&mut parser, &clean).expect("parses");
        assert_eq!(error_count(tree.root_node()), 0);
        assert!(error_reports(tree.root_node()).is_empty());
        assert!(errors_in_range(tree.root_node(), 0..clean.len()).is_empty());

        let dirty: Arc<[u8]> = Arc::from(b"fn broken( -> u32 {\n".as_slice());
        let tree = parse_snapshot(&mut parser, &dirty).expect("recovers with errors");
        let reports = error_reports(tree.root_node());
        assert!(!reports.is_empty());
        // A range disjoint from every report matches nothing.
        let last_end = reports.iter().map(|r| r.end_byte).max().unwrap_or(0);
        assert!(errors_in_range(tree.root_node(), last_end..dirty.len() + 8).is_empty());
    }

    /// B9: the probe over the real ocaml grammar — the silent-recovery
    /// measurement as a fence. An unclosed `(*` parses clean (error_count
    /// 0) yet the probe reports it, with the node's kind id and span; every
    /// honest EOF shape (balanced-at-EOF, nested-balanced, closed-then-
    /// code, unclosed *string* which the grammar DOES error on) reports
    /// nothing. Fails if the probe is disabled.
    #[test]
    #[cfg(feature = "lang-ocaml")]
    fn ocaml_unclosed_comment_probe_reports_silent_recovery_only() {
        let registry = crate::ast::lang::LangRegistry::new();
        let id = registry
            .for_extension("ml")
            .expect("lang-ocaml registers ocaml");
        let mut parser = registry.parser(id).expect("ABI pair");
        let trivia = registry.spec(id).expect("spec").trivia_kinds();
        let block = registry.spec(id).expect("spec").block_comment();

        // The hole: silent recovery, zero error nodes — probe reports it.
        let broken = b"let x = 1\n(* unclosed\nlet y = 2\n";
        let tree = crate::ast::ffi_shim::parse_bytes(&mut parser, broken, None).expect("tree");
        assert_eq!(error_count(tree.root_node()), 0, "the measured hole");
        let report =
            unclosed_comment_at_eof(tree.root_node(), broken, trivia, block).expect("reported");
        assert_eq!(report.start_byte, 10);
        assert_eq!(report.end_byte, broken.len());
        assert_eq!(
            registry.kind_name(id, report.kind_id).map(|n| n.as_str()),
            Some("comment")
        );

        // Nested unclosed: one open too many — still silent, still reported.
        let nested = b"let x = 1\n(* a (* b *)\nlet y = 2\n";
        let tree = crate::ast::ffi_shim::parse_bytes(&mut parser, nested, None).expect("tree");
        assert_eq!(error_count(tree.root_node()), 0);
        assert!(unclosed_comment_at_eof(tree.root_node(), nested, trivia, block).is_some());

        // Honest EOFs: balanced at EOF (nested included), a closed comment
        // mid-file, code tails, and the unclosed STRING class (which this
        // grammar errors on natively — the probe must stay out of its way).
        for clean in [
            &b"let x = 1\n(* a (* b *) *)"[..],
            &b"(* header *)\nlet y = 2\n"[..],
            &b"let x = 1\nlet y = 2"[..],
            &b"let x = \"unclosed\nlet y = 2\n"[..],
        ] {
            let tree =
                crate::ast::ffi_shim::parse_bytes(&mut parser, clean, None).expect("tree parses");
            assert!(
                unclosed_comment_at_eof(tree.root_node(), clean, trivia, block).is_none(),
                "honest tail must not be refused: {clean:?}"
            );
        }

        // No block delimiters, no probe: a line-comment tail is vacuously
        // clean even when the language is disabled-adjacent — python-shaped
        // bytes under a None block never report.
        assert!(
            unclosed_comment_at_eof(tree.root_node(), b"x = 1 # tail", &["comment"], None)
                .is_none()
        );
    }

    /// The lexical balance law, pure and feature-free: nesting languages
    /// count opens against closes; first-close languages are satisfied by
    /// the first close (`/* /* */` is a LEGAL C comment); overlapping
    /// delimiter shapes never balance by accident.
    #[test]
    fn block_comment_balance_follows_the_language_law() {
        use crate::ast::lang::BlockComment;

        // Nesting (ocaml, rust): balanced pairs at any depth.
        assert!(balances(b"(* ok *)", BlockComment::OCAML));
        assert!(balances(b"(* a (* b *) *)", BlockComment::OCAML));
        assert!(balances(b"/* a /* b */ */", BlockComment::RUST));
        // One open too many, or none closed at all.
        assert!(!balances(b"(* a (* b *)", BlockComment::OCAML));
        assert!(!balances(b"(* never closed", BlockComment::OCAML));
        // Overlaps: `(*)` and `/*/` carry no complete close sequence.
        assert!(!balances(b"(*)", BlockComment::OCAML));
        assert!(!balances(b"/*/", BlockComment::C_FAMILY));
        assert!(balances(b"/**/", BlockComment::C_FAMILY));

        // First-close (C family): a bare inner `/*` is TEXT, not nesting,
        // and there is no string awareness — the close inside quotes IS
        // the close (the reference lexers' law too).
        assert!(balances(b"/* /* */", BlockComment::C_FAMILY));
        assert!(balances(b"/* \"*/\" x", BlockComment::C_FAMILY));
        assert!(!balances(b"/* \"*  x", BlockComment::C_FAMILY));

        // The ocaml residual, pinned as a KNOWN limit (documented in
        // docs/SCHEMA.md): an unclosed string containing the close
        // delimiter inside the unclosed comment balances naively. A real
        // fix means re-implementing the grammar's string-in-comment
        // scanner — a lexer, not a probe.
        assert!(balances(b"(* \"*)", BlockComment::OCAML));
    }
}
