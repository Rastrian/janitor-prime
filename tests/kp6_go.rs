//! The five KP-6-go curated patterns as integration tests (F9b task 2;
//! the F1/F9a spike method replicated for go — PLAN §5 KP-6, memo
//! corollary): every idiom once clean and once comment-separated,
//! hand-computed strict/skip counts over curated snippets, plus the
//! measured divergence table over the bundled real-world-style go module
//! (`tests/fixtures/go/` — the same fixture repo the go e2e acceptance
//! drives).
//!
//! Go grammar facts this corpus pins (verified against tree-sitter-go
//! 0.25.0's own tree output, 2026-09-15):
//!
//! - `comment` is the **only** named trivia kind — line comments and
//!   block comments are the same kind, unlike rust's two — so go's
//!   trivia table is exactly `["comment"]` (ADR-0010).
//! - Comments attach to the innermost enclosing container and can sit
//!   between named siblings at every scope the matcher enters:
//!   `source_file` (doc comment above a func), `statement_list` (section
//!   comments between statements), `field_declaration_list` (field-group
//!   notes inside structs), `import_spec_list` (section comments inside
//!   a grouped import block) — the rust/python finding, repeated.
//! - **Same-line comments are legal go between declaration children**:
//!   `func (c T) /* note */ Name()` puts a named `comment` between the
//!   receiver `parameter_list` and the `field_identifier` name, and
//!   `if ok /* why */ {` puts one between the condition `identifier` and
//!   the `block`. Semicolon insertion only fires at line ends, so the
//!   comment never breaks the *statement* — but it does break byte-strict
//!   adjacency among the declaration's children. See
//!   `go_trivia_shapes_are_measured`.
//! - What a comment can never do (go's real immunity): sit between the
//!   `if`'s init statement and its condition (the `;` is on the same
//!   line *inside* the grammar's if chain — a comment there is still
//!   legal, in fact; the impossibility is only between `}` and `else`,
//!   where semicolon insertion ends the statement). The else-if chain
//!   (`} else if …`) is therefore comment-immune *between blocks*: the
//!   nested `if_statement` follows the anonymous `else` on the same
//!   line with no named sibling possible in between.
//!
//! Measured outcome (fixture module, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in go too —
//! byte-strict loses 50% of file-level function-adjacency pairs (2 vs
//! 4, the doc-comment-above-func idiom) and every curated
//! comment-separated pair is strict-invisible (all five CASES diverge
//! on their snippets; the module itself exercises the function-pair and
//! import-group idioms, and its method's same-line comment position is
//! measured in the shapes fence). ADR-0010's go entry stands; the go
//! trivia table stays `["comment"]`.

#![cfg(feature = "lang-go")]

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
        "(under source_file (seq (kind function_declaration) (kind function_declaration)))",
        include_str!("fixtures/kp6_go/doc_func_pair.go"),
        1,
        2,
        "doc-comment-above-func",
    ),
    (
        "(under statement_list (seq (kind if_statement) (kind if_statement)))",
        include_str!("fixtures/kp6_go/if_init_chain.go"),
        1,
        2,
        "if-init-sibling-chain",
    ),
    (
        "(under field_declaration_list (seq (kind field_declaration) (kind field_declaration)))",
        include_str!("fixtures/kp6_go/struct_tag_fields.go"),
        2,
        3,
        "struct-tag-adjacent-field",
    ),
    (
        "(under method_declaration (seq (kind parameter_list) (kind field_identifier)))",
        include_str!("fixtures/kp6_go/method_receiver.go"),
        1,
        2,
        "method-receiver-name-adjacency",
    ),
    (
        "(under import_spec_list (seq (kind import_spec) (kind import_spec)))",
        include_str!("fixtures/kp6_go/import_group.go"),
        2,
        3,
        "grouped-import-block",
    ),
];

/// The bundled real-world-style corpus (the go e2e fixture module).
const CORPUS: &str = "tests/fixtures/go";

/// The function-adjacency rule (CASES[0]'s), used for the corpus-wide
/// hand-computed leg.
const CORPUS_RULE: &str =
    "(under source_file (seq (kind function_declaration) (kind function_declaration)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.go"), source).expect("write snippet");
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
/// comment-separated pairs. The struct-tag and import-group cases are
/// the multi-run shape: four members with one dividing comment give
/// strict 2 (two runs) and skip 3 (three runs).
#[test]
fn kp6_go_curated_counts_match_the_measured_ground_truth() {
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

/// Corpus-wide counts for the function-adjacency idiom, hand-computed
/// from the fixture module's files: `shapes/scale.go` carries
/// NormalizeCode/squashSuffix adjacent plus ExpandCode behind its doc
/// comment (strict 1, skip 2), `shapes/fold.go` the same shape for
/// FoldCode/foldAll/Describe (strict 1, skip 2), and `main.go` a single
/// func (no pairs). Version.Apply is a `method_declaration`, not a
/// `function_declaration`, so it contributes nothing to this rule — the
/// declaration-kind distinction the corpus keeps visible. Total:
/// **strict 2, skip 4**.
#[test]
fn kp6_go_corpus_wide_counts_are_hand_computed() {
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
            "function adjacency over the fixture module (strict={strict})"
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
/// ADR-0010 go entry stands on. Assertions are the invariant (skipping
/// comments can only add matches); the printed table is the recorded
/// outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_go_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-go adjacency semantics - fixture module ({CORPUS}):");
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

/// The go trivia table's factual basis, as a fence: `comment` is the
/// single comment-class named kind (line and block comments alike), it
/// is a named child of the innermost container at every scope measured
/// (source_file, statement_list, field_declaration_list,
/// import_spec_list), and — go's distinctive fact — a same-line block
/// comment is a legal named sibling between a method's receiver and its
/// name and between an if's condition and its block. If a grammar
/// upgrade changes any of this, the trivia table must be re-measured.
#[test]
fn go_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    let source = "package a\n\nimport (\n\t\"fmt\"\n\t// note\n\t\"strings\"\n)\n\ntype T struct {\n\tA int\n\t// group\n\tB int\n}\n\nfunc f(ok bool) {\n\tif ok /* why */ {\n\t\t_ = 1\n\t}\n}\n\ntype C int\n\nfunc (c C) /* role */ Name() {}\n";
    let registry = LangRegistry::new();
    let id = registry.for_extension("go").expect("go registered");
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

    // Every comment-class named kind in the whole tree is `comment` —
    // line comments (`// note`), block comments (`/* why */`, `/* role
    // */`) alike. That is the entire factual basis of the one-entry
    // trivia table.
    let mut comment_kinds: Vec<String> = Vec::new();
    let mut stack = vec![tree.root_node()];
    while let Some(node) = stack.pop() {
        if node.is_named() && node.kind().contains("comment") {
            comment_kinds.push(node.kind().to_owned());
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    assert_eq!(
        comment_kinds,
        vec!["comment"; 4],
        "line and block comments are all the one `comment` kind"
    );

    // import_spec_list: the section comment is a named child between
    // the import_specs.
    let import_list = {
        let mut found = None;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "import_spec_list" {
                found = Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        found.expect("import group present")
    };
    assert_eq!(
        named_kinds(import_list),
        vec![
            "import_spec".to_owned(),
            "comment".to_owned(),
            "import_spec".to_owned(),
        ],
        "the grouped-import section comment is a named sibling"
    );

    // if_statement: the same-line block comment between condition and
    // block is a named sibling — go's distinctive same-line fact.
    let if_stmt = {
        let mut found = None;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "if_statement" {
                found = Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        found.expect("if statement present")
    };
    assert_eq!(
        named_kinds(if_stmt),
        vec![
            "identifier".to_owned(),
            "comment".to_owned(),
            "block".to_owned(),
        ],
        "if ok /* why */ — the comment parks between condition and block"
    );

    // method_declaration: receiver parameter_list, the same-line
    // comment, then the field_identifier name.
    let method = {
        let mut found = None;
        let mut stack = vec![tree.root_node()];
        while let Some(node) = stack.pop() {
            if node.kind() == "method_declaration" {
                found = Some(node);
            }
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                stack.push(child);
            }
        }
        found.expect("method present")
    };
    assert_eq!(
        named_kinds(method),
        vec![
            "parameter_list".to_owned(),
            "comment".to_owned(),
            "field_identifier".to_owned(),
            "parameter_list".to_owned(),
            "block".to_owned(),
        ],
        "func (c C) /* role */ Name() — receiver, comment, name are siblings"
    );
}
