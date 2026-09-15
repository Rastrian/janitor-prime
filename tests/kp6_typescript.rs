//! The five KP-6-typescript curated patterns as integration tests (F9b
//! task 2; the F1/F9a spike method replicated for typescript — PLAN §5
//! KP-6, memo corollary): every idiom once clean and once
//! comment-separated, hand-computed strict/skip counts over curated
//! snippets, plus the measured divergence table over the bundled
//! real-world-style module (`tests/fixtures/typescript/` — the same
//! fixture repo the typescript e2e acceptance drives; two `.ts` files
//! and one `.tsx`, so both grammars of the crate are in the measured
//! set).
//!
//! TypeScript grammar facts this corpus pins (verified against
//! tree-sitter-typescript 0.23.2's own tree output, 2026-09-15):
//!
//! - Trivia is **two** kinds, not one: `comment` (line, block, JSDoc —
//!   all the same kind) AND `html_comment`. The grammar accepts
//!   Annex-B HTML-like comments (`<!-- … -->`) in plain `.ts` as named
//!   children between statement siblings, and the tsx grammar
//!   additionally parks them between `jsx_text` siblings inside JSX
//!   trees. The v0.1 seeded table said `["comment"]`; the measured table
//!   is `["comment", "html_comment"]` for both registry entries — a
//!   seeded-table correction recorded in PLAN §3.
//! - `hash_bang_line` is a named child of `program` but only ever at
//!   index 0 — it can never sit between pattern siblings, so it stays
//!   OUT of the trivia table (ADR-0010 scopes Seq skipping to
//!   comment-class kinds *between siblings*); fenced in
//!   `typescript_trivia_shapes_are_measured`.
//! - Separator style never matters: interface members separated by `;`
//!   or by newline are equally adjacent in both modes — both separators
//!   are anonymous tokens, never in the named sibling list.
//! - Comments attach to the innermost container and can sit between
//!   named siblings at every scope measured: `program` (JSDoc above a
//!   function), `interface_body` (member-group notes),
//!   `export_statement` (between decorator and class), `arguments`
//!   (between two arrow callbacks), `variable_declarator` (between name
//!   and type annotation) — the rust/python/go finding, repeated.
//!
//! Measured outcome (fixture module, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in typescript
//! too — byte-strict loses 50% of exported-declaration adjacency pairs
//! (2 vs 4) and every curated comment-separated pair is
//! strict-invisible. ADR-0010's typescript entry stands with the
//! two-kind trivia table above.

#![cfg(feature = "lang-typescript")]

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
        "(under program (seq (kind function_declaration) (kind function_declaration)))",
        include_str!("fixtures/kp6_typescript/jsdoc_function.ts"),
        1,
        2,
        "jsdoc-above-function",
    ),
    (
        "(under interface_body (seq (kind property_signature) (kind property_signature)))",
        include_str!("fixtures/kp6_typescript/interface_members.ts"),
        3,
        4,
        "interface-member-adjacency",
    ),
    (
        "(under export_statement (seq (kind decorator) (kind class_declaration)))",
        include_str!("fixtures/kp6_typescript/decorator_class.ts"),
        1,
        2,
        "decorator-class-adjacency",
    ),
    (
        "(under arguments (seq (kind arrow_function) (kind arrow_function)))",
        include_str!("fixtures/kp6_typescript/arrow_args.ts"),
        1,
        2,
        "arrow-argument-chain",
    ),
    (
        "(under variable_declarator (seq (kind identifier) (kind type_annotation)))",
        include_str!("fixtures/kp6_typescript/type_annotations.ts"),
        1,
        2,
        "type-annotation-adjacency",
    ),
];

/// The bundled real-world-style corpus (the typescript e2e fixture
/// module: src/format.ts, src/report.ts, src/widget.tsx).
const CORPUS: &str = "tests/fixtures/typescript";

/// The exported-declaration adjacency rule (the JSDoc idiom at module
/// scale, where top-level functions live inside `export_statement`s),
/// used for the corpus-wide hand-computed leg.
const CORPUS_RULE: &str = "(under program (seq (kind export_statement) (kind export_statement)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.ts"), source).expect("write snippet");
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
/// comment-separated pairs. The interface case is the multi-run shape:
/// five members with one dividing comment give strict 3 (three runs)
/// and skip 4 (four runs) — with `;`-separated and newline-separated
/// members contributing identically (both separators are anonymous).
#[test]
fn kp6_typescript_curated_counts_match_the_measured_ground_truth() {
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

/// Corpus-wide counts for the exported-declaration adjacency idiom,
/// hand-computed from the fixture module's files: `src/format.ts`
/// carries normalizeCode/expandCode adjacent plus formatLine behind its
/// comment (strict 1, skip 2), `src/report.ts` the interface plus
/// rowFor behind a divider comment plus rowsFor (strict 1, skip 2), and
/// `src/widget.tsx` a single export (no pairs — parsed under the tsx
/// grammar, same rule, same behavior). Total: **strict 2, skip 4**.
#[test]
fn kp6_typescript_corpus_wide_counts_are_hand_computed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    for (strict, expected) in [(true, 2_u64), (false, 4_u64)] {
        let mut params = ScanParams::new(CORPUS_RULE, &root);
        params.strict_trivia = strict;
        let session = Session::new();
        let report = session
            .scan(&params)
            .unwrap_or_else(|err| panic!("corpus scan: {err}"));
        assert_eq!(
            report.matches.len() as u64,
            expected,
            "export adjacency over the fixture module (strict={strict})"
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
/// ADR-0010 typescript entry stands on. Assertions are the invariant
/// (skipping comments can only add matches); the printed table is the
/// recorded outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_typescript_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-typescript adjacency semantics - fixture module ({CORPUS}):");
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

/// The trivia tables' factual basis, as a fence, across BOTH registry
/// entries: `hash_bang_line` is a named child of program but only at
/// index 0 (never between siblings — out of the table); `html_comment`
/// is a legal named sibling between statements in plain `.ts` (Annex-B
/// HTML-like comments) and between `jsx_text` siblings under the tsx
/// grammar (in the table); `{/* … */}` inside JSX is a plain `comment`
/// inside a `jsx_expression`; interface separators are anonymous. If a
/// grammar upgrade changes any of this, the trivia tables must be
/// re-measured.
#[test]
fn typescript_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    fn named_kinds(node: Node<'_>) -> Vec<String> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .filter(|child| child.is_named())
            .map(|child| child.kind().to_owned())
            .collect()
    }

    fn find_kind<'t>(root: Node<'t>, kind: &str) -> Option<Node<'t>> {
        let mut stack = vec![root];
        while let Some(node) = stack.pop() {
            if node.kind() == kind {
                return Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        None
    }

    let registry = LangRegistry::new();
    let ts = registry.for_extension("ts").expect("typescript");
    let tsx = registry.for_extension("tsx").expect("tsx");

    // --- plain .ts: hashbang at index 0, imports after it; Annex-B
    // html_comment between two declarations.
    let ts_source = "#!/usr/bin/env tsx\nimport { a } from \"b\";\nimport { c } from \"d\";\nconst one = 1;\n<!-- legacy note -->\nconst two = 2;\n";
    let mut parser = registry.parser(ts).expect("ts parser");
    let tree = ffi_shim::parse_bytes(&mut parser, ts_source.as_bytes(), None).expect("tree");
    assert!(!tree.root_node().has_error(), "probe parses clean");
    assert_eq!(
        named_kinds(tree.root_node()),
        vec![
            "hash_bang_line".to_owned(),
            "import_statement".to_owned(),
            "import_statement".to_owned(),
            "lexical_declaration".to_owned(),
            "html_comment".to_owned(),
            "lexical_declaration".to_owned(),
        ],
        "hashbang only ever leads (never between siblings); the Annex-B \
         html_comment sits BETWEEN the two declarations — comment-class"
    );

    // --- plain .ts: `;` and newline separators are both anonymous.
    let iface_source = "interface I {\n  a: number;\n  b: string\n}\n";
    let tree = ffi_shim::parse_bytes(&mut parser, iface_source.as_bytes(), None).expect("tree");
    assert!(!tree.root_node().has_error());
    let body = find_kind(tree.root_node(), "interface_body").expect("body");
    assert_eq!(
        named_kinds(body),
        vec![
            "property_signature".to_owned(),
            "property_signature".to_owned()
        ],
        "no separator node ever surfaces: `;` and newline are equally invisible"
    );

    // --- tsx: html_comment between jsx_text siblings; {/* … */} is a
    // plain comment inside a jsx_expression.
    let tsx_source = "const A = () => (\n  <p>\n    one\n    <!-- a note -->\n    two\n  </p>\n);\nconst B = () => (\n  <p>\n    {/* braced */}\n    three\n  </p>\n);\n";
    let mut tsx_parser = registry.parser(tsx).expect("tsx parser");
    let tree = ffi_shim::parse_bytes(&mut tsx_parser, tsx_source.as_bytes(), None).expect("tree");
    assert!(!tree.root_node().has_error(), "tsx probe parses clean");
    // Anchor on the html_comment itself (the tree carries two jsx
    // elements; this one is provably the sibling-relevant one).
    let html_comment = find_kind(tree.root_node(), "html_comment").expect("html comment");
    let element = html_comment.parent().expect("inside a jsx element");
    assert_eq!(element.kind(), "jsx_element");
    assert_eq!(
        named_kinds(element),
        vec![
            "jsx_opening_element".to_owned(),
            "jsx_text".to_owned(),
            "html_comment".to_owned(),
            "jsx_text".to_owned(),
            "jsx_closing_element".to_owned(),
        ],
        "html_comment is a named sibling between jsx_text — in the tsx table"
    );
    let braced = find_kind(tree.root_node(), "jsx_expression").expect("jsx expression");
    assert_eq!(
        named_kinds(braced),
        vec!["comment".to_owned()],
        "the braced /* … */ form is a plain `comment` inside the expression"
    );

    // --- the routing fact the tsx entry exists for: the same JSX under
    // the plain typescript grammar is ERROR-laden.
    let jsx_only = "const A = () => <p>hi</p>;\n";
    let tree = ffi_shim::parse_bytes(&mut parser, jsx_only.as_bytes(), None).expect("tree");
    assert!(
        tree.root_node().has_error(),
        "JSX under the typescript grammar must error — .tsx cannot route there"
    );
}
