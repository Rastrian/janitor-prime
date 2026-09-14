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
#![allow(dead_code)] // scaffold: F5's apply gate and E_PARSE_INVALID payload
// are the production consumers of the report queries; the bin crate has no
// live root until the lib target (F3.6) — same precedent as src/vfs.rs.

use std::ops::Range;

use crate::ast::ffi_shim::{Node, Point};

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

/// Walk `node`, collecting ERROR/MISSING nodes as plain values. Recursion
/// only enters subtrees whose `has_error` flag is set, and depth is bounded
/// by the grammar's recovery nesting; F3.6's pooled traversal takes over
/// the engine's general walks.
fn collect_error_reports(node: Node<'_>, out: &mut Vec<ErrorReport>) {
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
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.has_error() {
            collect_error_reports(child, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::lang::LangRegistry;
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

    #[cfg(feature = "lang-rust")]
    fn setup() -> (LangRegistry, crate::ast::lang::LangId) {
        let registry = LangRegistry::new();
        let id = registry
            .for_extension("rs")
            .expect("lang-rust is the default feature set");
        (registry, id)
    }

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
}
