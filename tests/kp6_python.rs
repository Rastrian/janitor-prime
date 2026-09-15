//! The five KP-6-python curated patterns as integration tests (F9a task
//! 2; the F1 spike method replicated for python — PLAN §5 KP-6, memo
//! corollary): every idiom once clean and once comment-separated,
//! hand-computed strict/skip counts over curated snippets, plus the
//! measured divergence table over the bundled real-world-style python
//! corpus (`tests/fixtures/python/` — the same fixture repo the python
//! e2e acceptance drives).
//!
//! Python grammar facts this corpus pins (verified against
//! tree-sitter-python 0.25.0's own tree output, 2026-09-15):
//!
//! - `comment` is the **only** named trivia kind that ever appears
//!   between statement-level siblings; NEWLINE and the scanner's
//!   INDENT/DEDENT are anonymous tokens, so they are never in the named
//!   sibling list in either ADR-0010 mode — python's indentation
//!   discipline lives *inside* the grammar, exactly as the F9 brief
//!   suspected, and named-sibling adjacency ports to it unchanged.
//! - Comments attach to the innermost enclosing container: between a
//!   decorator and its def the comment is a child of
//!   `decorated_definition`; between elif/else clauses a child of
//!   `if_statement`; between methods a child of the class-body `block`;
//!   between imports a child of `module` — so trivia-skipping must apply
//!   at every scope the matcher enters (the rust KP-6 finding, repeated).
//! - `line_continuation` is a *named* whitespace-class extra, but it
//!   lands inside expression nodes (`binary_operator` between operands),
//!   never between statement siblings — see `python_trivia_shapes_are_measured`.
//!
//! Measured outcome (fixture corpus, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in python too —
//! byte-strict loses 28.6% of method-adjacency pairs (5 vs 7) and 10.5%
//! of docstring-then-return pairs (17 vs 19), and every curated
//! comment-separated pair is strict-invisible. ADR-0010's python entry
//! stands; the python trivia table stays `["comment"]`
//! (`line_continuation` is excluded: ADR-0010 scopes Seq skipping to
//! *comment-class* kinds, and it never sits between statement siblings
//! anyway).

#![cfg(feature = "lang-python")]

use std::path::Path;

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

/// (rule, curated snippet, strict, skip, name) — hand-computed ground
/// truth, straight from the tree shapes dumped while authoring the
/// corpus. Each snippet contains the idiom once clean and once with a
/// comment deliberately interleaved between exactly the pattern
/// siblings.
const CASES: &[(&str, &str, u64, u64, &str)] = &[
    (
        "(under decorated_definition (seq (kind decorator) (kind function_definition)))",
        include_str!("fixtures/kp6_python/decorated_def.py"),
        1,
        2,
        "decorator-above-def",
    ),
    (
        "(under if_statement (seq (kind elif_clause) (kind else_clause)))",
        include_str!("fixtures/kp6_python/elif_else.py"),
        1,
        2,
        "elif-else-adjacency",
    ),
    (
        "(under block (seq (kind function_definition) (kind function_definition)))",
        include_str!("fixtures/kp6_python/class_methods.py"),
        1,
        2,
        "method-in-class-adjacency",
    ),
    (
        "(under block (seq (kind expression_statement) (kind return_statement)))",
        include_str!("fixtures/kp6_python/docstring_first.py"),
        1,
        2,
        "docstring-as-first-statement",
    ),
    (
        "(under module (seq (kind import_statement) (kind import_statement)))",
        include_str!("fixtures/kp6_python/import_group.py"),
        2,
        3,
        "import-group-adjacency",
    ),
];

/// The bundled real-world-style corpus (the python e2e fixture repo).
const CORPUS: &str = "tests/fixtures/python";

/// The method-adjacency rule (CASES[2]'s), used for the corpus-wide
/// hand-computed leg.
const CORPUS_RULE: &str =
    "(under block (seq (kind function_definition) (kind function_definition)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.py"), source).expect("write snippet");
    let mut params = ScanParams::new(rule, tmp.path());
    params.strict_trivia = strict;
    let session = Session::new();
    session
        .scan(&params)
        .unwrap_or_else(|err| panic!("{rule}: {err}"))
        .matches
        .len() as u64
}

/// The curated ground truth, both modes: strict loses exactly the
/// comment-separated pairs. The import-group case is the multi-run
/// shape: three adjacent imports plus one behind a comment give strict 2
/// (two runs) and skip 3 (three runs over the four imports).
#[test]
fn kp6_python_curated_counts_match_the_measured_ground_truth() {
    for (rule, source, strict, skip, name) in CASES {
        assert_eq!(
            &scan_counts(rule, source, true),
            strict,
            "{name}: --strict-trivia count must equal the hand-computed truth"
        );
        assert_eq!(
            &scan_counts(rule, source, false),
            skip,
            "{name}: default (comment-skipping) count must equal the hand-computed truth"
        );
    }
}

/// Corpus-wide counts for the method-adjacency idiom, hand-computed from
/// the fixture repo's class bodies: `models.Order` carries three
/// adjacent methods plus `describe` behind a section comment (strict 2,
/// skip 3), `service.OrderStore` five methods with `save` behind one
/// (strict 3, skip 4), and `models.PaymentAttempt`'s `for_order` is a
/// `decorated_definition` child (not a `function_definition`), so it
/// contributes nothing — the decorator-wrapping subtlety the corpus
/// keeps visible. Total: **strict 5, skip 7**.
#[test]
fn kp6_python_corpus_wide_counts_are_hand_computed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    for (strict, expected) in [(true, 5_u64), (false, 7_u64)] {
        let mut params = ScanParams::new(CORPUS_RULE, &root);
        params.strict_trivia = strict;
        let session = Session::new();
        let report = session
            .scan(&params)
            .unwrap_or_else(|err| panic!("corpus scan: {err}"));
        assert_eq!(
            report.matches.len() as u64,
            expected,
            "method adjacency over the fixture corpus (strict={strict})"
        );
        // Canonical path order: matches arrive file-grouped, paths sorted.
        let mut paths: Vec<&str> = report.matches.iter().map(|m| m.path.as_str()).collect();
        paths.dedup();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "matches are grouped in path order");
    }
}

/// The measured divergence table over the real corpus — the leg the
/// ADR-0010 python entry stands on. Assertions are the invariant
/// (skipping comments can only add matches); the printed table is the
/// recorded outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_python_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-python adjacency semantics - fixture corpus ({CORPUS}):");
    for (rule, _, _, _, name) in CASES {
        let mut strict_params = ScanParams::new(*rule, &root);
        strict_params.strict_trivia = true;
        let mut skip_params = ScanParams::new(*rule, &root);
        skip_params.strict_trivia = false;
        let session = Session::new();
        let strict = session
            .scan(&strict_params)
            .unwrap_or_else(|err| panic!("{name}: {err}"))
            .matches
            .len() as u64;
        let skip = session
            .scan(&skip_params)
            .unwrap_or_else(|err| panic!("{name}: {err}"))
            .matches
            .len() as u64;
        assert!(skip >= strict, "{name}: skipping can only add matches");
        let loss = if skip > 0 {
            100.0 * (skip - strict) as f64 / skip as f64
        } else {
            0.0
        };
        println!(
            "  {:>34}  strict {:>5}  skip {:>5}  strict-loss {:.1}%",
            name, strict, skip, loss
        );
    }
}

/// The python trivia table's factual basis, as a fence: `comment` is a
/// named child of the innermost container (so skip-mode filtering sees
/// it), `line_continuation` never appears between statement siblings
/// (it parks inside `binary_operator`), and the module's statement
/// children are pure statements — no NEWLINE/INDENT/DEDENT named nodes
/// ever surface. If a grammar upgrade changes any of this, the trivia
/// table must be re-measured.
#[test]
fn python_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    let source = "import os\n# between imports\nimport sys\n\n\ndef f(x):\n    total = 1 + \\\n        2\n    return total\n";
    let registry = LangRegistry::new();
    let id = registry.for_extension("py").expect("python registered");
    let mut parser = registry.parser(id).expect("parser sets up");
    let tree = ffi_shim::parse_bytes(&mut parser, source.as_bytes(), None).expect("tree parses");
    assert!(!tree.root_node().has_error(), "probe source parses clean");

    fn named_kinds(node: Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .filter(|child| child.is_named())
            .map(|child| child.kind().to_owned())
            .collect()
    }

    let root = tree.root_node();
    // module: docstring-free here; children are statements + the comment.
    let module_kinds = named_kinds(root);
    assert_eq!(
        module_kinds,
        vec![
            "import_statement".to_owned(),
            "comment".to_owned(),
            "import_statement".to_owned(),
            "function_definition".to_owned(),
        ],
        "the module's named children are statements plus the comment — \
         no NEWLINE/INDENT/DEDENT ever surfaces as a named sibling"
    );

    // The line continuation parks inside binary_operator, between the
    // operands — it is never a statement-level sibling.
    let mut cursor = root.walk();
    let mut stack: Vec<Node<'_>> = root.children(&mut cursor).collect();
    let mut continuation_parents = Vec::new();
    while let Some(node) = stack.pop() {
        if node.kind() == "line_continuation" {
            continuation_parents.push(
                node.parent()
                    .map(|parent| parent.kind().to_owned())
                    .unwrap_or_default(),
            );
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    assert_eq!(
        continuation_parents.len(),
        1,
        "exactly one line_continuation exists"
    );
    assert_eq!(
        continuation_parents[0], "binary_operator",
        "line_continuation is interior to the expression, never a statement sibling"
    );
}
