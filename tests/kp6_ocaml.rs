//! The KP-6-ocaml curated patterns as integration tests (F9c task 2,
//! ocaml leg; three patterns per the brief, the third measured in BOTH
//! grammars — module-item adjacency in `.ml` structure bodies and
//! signature-item adjacency in `.mli` compilation units): hand-computed
//! strict/skip counts over curated snippets, the corpus-wide leg over
//! the bundled fixture repo (`tests/fixtures/ocaml/` — the same repo the
//! ocaml e2e acceptance drives), and the grammar-shape fence.
//!
//! OCaml grammar facts this corpus pins (verified against
//! tree-sitter-ocaml 0.26.0's own tree output, 2026-09-15):
//!
//! - `comment` is the only comment-class named kind between siblings,
//!   and it attaches to the innermost container — `compilation_unit`
//!   (file level, both grammars), `structure` (module bodies) or
//!   `signature` (sig bodies). The trivia table `["comment"]` is the
//!   measured entry, for both LangIds.
//! - **let..in chains are NOT sibling lists** (the recorded surprise):
//!   `let a = … in let b = … in body` parses as right-nested
//!   `let_expression` nodes — each `in` wraps the rest of the chain as
//!   its body. Successive let-bindings are parent-and-child, never
//!   siblings, in either trivia mode; only file-level and module-body
//!   lets are true siblings. Adjacency rules over `let_binding`s must
//!   scope to `compilation_unit`/`structure`, never `let_expression`.
//! - The grammar's extras are `comment`, `attribute` and
//!   `line_number_directive`. `attribute` (`[@…]` annotations) is a
//!   named extra that can float between items like ruby's heredoc_body —
//!   it is deliberately NOT trivia (annotations are content), the same
//!   experimental-tier caveat, recorded in the norm-table header.
//! - Interface items: `val x : t` is a `value_specification`, a true
//!   sibling inside `compilation_unit` (and inside `signature` for
//!   `module X : sig … end` bodies).
//!
//! Measured outcome (fixture corpus, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in ocaml too —
//! byte-strict loses 20.0% of top-level binding pairs (4 vs 5) and 33.3%
//! of val-specification pairs (2 vs 3); the curated comment-separated
//! pairs are strict-invisible. ADR-0010's ocaml entries stand; both
//! trivia tables stay `["comment"]`.

#![cfg(feature = "lang-ocaml")]

use std::path::Path;

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

/// (rule, curated snippet, lang filter, strict, skip, name) — hand-computed
/// ground truth, straight from the tree shapes dumped while authoring the
/// corpus. Pattern 1's zero row is the point: let..in chains nest, so no
/// sibling adjacency exists to lose.
const CASES: &[(&str, &str, &str, u64, u64, &str)] = &[
    (
        "(under let_expression (seq (kind value_definition) (kind value_definition)))",
        include_str!("fixtures/kp6_ocaml/let_in_chain.ml"),
        "ocaml",
        0,
        0,
        "let..in chains nest, never sibling",
    ),
    (
        "(under compilation_unit (seq (kind value_definition) (kind value_definition)))",
        include_str!("fixtures/kp6_ocaml/comment_bindings.ml"),
        "ocaml",
        1,
        2,
        "comment-between-bindings (.ml)",
    ),
    (
        "(under structure (seq (kind value_definition) (kind value_definition)))",
        include_str!("fixtures/kp6_ocaml/module_items.ml"),
        "ocaml",
        1,
        2,
        "module-item adjacency (.ml)",
    ),
    (
        "(under compilation_unit (seq (kind value_specification) (kind value_specification)))",
        include_str!("fixtures/kp6_ocaml/signature_items.mli"),
        "ocaml_interface",
        0,
        1,
        "signature-item adjacency (.mli)",
    ),
];

/// The bundled ocaml corpus (the ocaml e2e fixture repo).
const CORPUS: &str = "tests/fixtures/ocaml";

fn scan_counts(rule: &str, source: &str, name: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join(name), source).expect("write snippet");
    let mut params = ScanParams::new(rule, tmp.path());
    params.strict_trivia = strict;
    let session = Session::new();
    session
        .scan(&params)
        .unwrap_or_else(|err| panic!("{rule}: {err}"))
        .matches
        .len() as u64
}

fn corpus_counts(rule: &str, lang: &str, strict: bool) -> u64 {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    let mut params = ScanParams::new(rule, &root);
    params.strict_trivia = strict;
    params.lang_filter = Some(lang.to_owned());
    let session = Session::new();
    session
        .scan(&params)
        .unwrap_or_else(|err| panic!("corpus scan: {err}"))
        .matches
        .len() as u64
}

/// The curated ground truth, both modes: strict loses exactly the
/// comment-separated pairs; pattern 1 loses nothing because let..in
/// chains never had sibling structure to lose.
#[test]
fn kp6_ocaml_curated_counts_match_the_measured_ground_truth() {
    for (rule, source, lang, strict, skip, label) in CASES {
        // The snippet's extension routes it to its own grammar (.ml vs
        // .mli — the two-LangId mapping is load-bearing for the counts).
        let file = if *lang == "ocaml" {
            "curated.ml"
        } else {
            "curated.mli"
        };
        assert_eq!(
            &scan_counts(rule, source, file, true),
            strict,
            "{label}: --strict-trivia count must equal the hand-computed truth"
        );
        assert_eq!(
            &scan_counts(rule, source, file, false),
            skip,
            "{label}: default (comment-skipping) count must equal the hand-computed truth"
        );
    }
}

/// The curated snippets must parse under their OWN grammar: .ml files
/// under ocaml, .mli under ocaml_interface — the two-LangId mapping is
/// load-bearing for the counts above (a .mli parsed as .ml would produce
/// a different sibling structure entirely).
#[test]
fn kp6_ocaml_curated_files_route_to_their_own_grammars() {
    for (_, source, lang, _, _, label) in CASES {
        let ext = if *lang == "ocaml" { "x.ml" } else { "x.mli" };
        let tmp = tempfile::tempdir().expect("tempdir");
        std::fs::write(tmp.path().join(ext), source).expect("write snippet");
        let mut params = ScanParams::new("_", tmp.path());
        params.lang_filter = Some(lang.to_string());
        let session = Session::new();
        let report = session
            .scan(&params)
            .unwrap_or_else(|err| panic!("{label}: {err}"));
        assert_eq!(
            report.stats.langs.get(*lang).copied(),
            Some(1),
            "{label}: the file counts as exactly one {lang} file"
        );
    }
}

/// Corpus-wide counts, hand-computed from the fixture repo:
/// `store.ml`'s unit children are the entry `type_definition`, three
/// bindings with the comment before `render`, plus `total` (strict 2,
/// skip 3 — `let () =` in report.ml is also a value_definition, adding
/// one more adjacent pair there: strict 2, skip 2). Total: **strict 4,
/// skip 5**. The val-specification rule over `store.mli`'s four `val`s
/// with the comment before `entry_key`: **strict 2, skip 3**.
#[test]
fn kp6_ocaml_corpus_wide_counts_are_hand_computed() {
    for (rule, lang, strict, expected) in [
        (
            "(under compilation_unit (seq (kind value_definition) (kind value_definition)))",
            "ocaml",
            true,
            4_u64,
        ),
        (
            "(under compilation_unit (seq (kind value_definition) (kind value_definition)))",
            "ocaml",
            false,
            5_u64,
        ),
        (
            "(under compilation_unit (seq (kind value_specification) (kind value_specification)))",
            "ocaml_interface",
            true,
            2_u64,
        ),
        (
            "(under compilation_unit (seq (kind value_specification) (kind value_specification)))",
            "ocaml_interface",
            false,
            3_u64,
        ),
    ] {
        assert_eq!(
            corpus_counts(rule, lang, strict),
            expected,
            "corpus-wide count (lang={lang}, strict={strict})"
        );
    }
    // Canonical path order over the unfiltered corpus: matches arrive
    // file-grouped, paths sorted.
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    let params = ScanParams::new(
        "(under let_binding (seq (capture @fn value_name) parameter))",
        &root,
    );
    let session = Session::new();
    let report = session.scan(&params).expect("corpus scan");
    let mut paths: Vec<&str> = report.matches.iter().map(|m| m.path.as_str()).collect();
    paths.dedup();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "matches are grouped in path order");
}

/// The measured divergence table over the real corpus — the leg the
/// ADR-0010 ocaml entries stand on. Assertions are the invariant
/// (skipping comments can only add matches); the printed table is the
/// recorded outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_ocaml_corpus_divergence_table() {
    println!("KP-6-ocaml adjacency semantics - fixture corpus ({CORPUS}):");
    for (rule, _, lang, _, _, name) in CASES {
        let strict = corpus_counts(rule, lang, true);
        let skip = corpus_counts(rule, lang, false);
        assert!(skip >= strict, "{name}: skipping can only add matches");
        let loss = if skip > 0 {
            100.0 * (skip - strict) as f64 / skip as f64
        } else {
            0.0
        };
        println!(
            "  {:>36}  strict {:>5}  skip {:>5}  strict-loss {:.1}%",
            name, strict, skip, loss
        );
    }
}

/// The ocaml trivia tables' factual basis, as a fence: `comment` is a
/// named sibling inside compilation_unit (both grammars); the let..in
/// nesting shape (value_definition followed by let_expression — the
/// surprise, with a seq over two value_definitions matching nothing);
/// and the interface grammar's value_specification siblings. If a
/// grammar upgrade changes any of this, the trivia tables must be
/// re-measured.
#[test]
fn ocaml_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim;
    use janitor_prime::ast::lang::LangRegistry;

    fn named_kinds(node: janitor_prime::ast::ffi_shim::Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .filter(|child| child.is_named())
            .map(|child| child.kind().to_owned())
            .collect()
    }

    let registry = LangRegistry::new();
    let ml = registry.for_extension("ml").expect("ocaml registered");
    let mli = registry
        .for_extension("mli")
        .expect("ocaml_interface registered");

    // .ml: file-level bindings are true siblings with the comment
    // between them; the let..in chain nests right-associatively.
    let source = "let one = 1\n\n(* between *)\nlet chain x =\n  let a = x + 1 in\n  let b = a * 2 in\n  a + b\n";
    let mut parser = registry.parser(ml).expect("parser sets up");
    let tree = ffi_shim::parse_bytes(&mut parser, source.as_bytes(), None).expect("tree parses");
    assert!(!tree.root_node().has_error(), "probe source parses clean");
    let root = tree.root_node();
    assert_eq!(root.kind(), "compilation_unit");
    assert_eq!(
        named_kinds(root),
        vec![
            "value_definition".to_owned(),
            "comment".to_owned(),
            "value_definition".to_owned(),
        ],
        "file-level bindings are true siblings, comment between"
    );
    let chain = root
        .named_child(2)
        .expect("the chain definition")
        .named_child(0)
        .expect("its let_binding");
    let last = u32::try_from(chain.named_child_count()).expect("count fits") - 1;
    let body = chain.named_child(last).expect("the binding's body");
    assert_eq!(body.kind(), "let_expression");
    assert_eq!(
        named_kinds(body),
        vec!["value_definition".to_owned(), "let_expression".to_owned(),],
        "let..in nests right-associatively: the first binding's sibling \
         is the REST of the chain, not the next binding"
    );
    // The measured consequence: a seq over two value_definitions inside
    // let_expression matches nothing, in either mode.
    let spanning = "(under let_expression (seq (kind value_definition) (kind value_definition)))";
    assert_eq!(scan_counts(spanning, source, "x.ml", true), 0);
    assert_eq!(scan_counts(spanning, source, "x.ml", false), 0);
    // …while the actual shape (binding, then the nested chain) matches.
    let real = "(under let_expression (seq (kind value_definition) (kind let_expression)))";
    assert_eq!(scan_counts(real, source, "x.ml", true), 1);
    assert_eq!(scan_counts(real, source, "x.ml", false), 1);

    // .mli: val specifications are true siblings of the compilation
    // unit, comment between them.
    let iface = "val render : int -> string\n\n(* between specs *)\nval tally : int -> int\n";
    let mut parser = registry.parser(mli).expect("parser sets up");
    let tree = ffi_shim::parse_bytes(&mut parser, iface.as_bytes(), None).expect("iface parses");
    assert!(!tree.root_node().has_error());
    assert_eq!(
        named_kinds(tree.root_node()),
        vec![
            "value_specification".to_owned(),
            "comment".to_owned(),
            "value_specification".to_owned(),
        ],
        "interface items are true siblings, comment between"
    );
}
