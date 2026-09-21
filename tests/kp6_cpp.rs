//! The seven KP-6-cpp curated patterns as integration tests (C1 of
//! docs/PROMOTION.md; the F9a/F9b method replicated for C++ — PLAN §5
//! KP-6, memo corollary): every idiom once clean and once
//! comment-separated, hand-computed strict/skip counts over curated
//! snippets, plus the measured divergence table over the bundled
//! real-world-style cpp module (`tests/fixtures/cpp/` — the same module
//! the cpp e2e acceptance and determinism leg drive).
//!
//! C++ grammar facts this corpus pins (verified against tree-sitter-cpp
//! 0.23.4's own tree output, 2026-09-20):
//!
//! - `comment` is the **only** comment-class named kind — and it is
//!   **one node per `//` line**: a six-line header comment is six
//!   sibling `comment` nodes, not one. Trivia-skipping handles this
//!   uniformly; byte-strict adjacency sees every one of them.
//! - Comments attach to the innermost enclosing container and sit
//!   between named siblings at every scope the matcher enters:
//!   `translation_unit` (doc comment above a function, section comment
//!   inside an include group), `declaration_list` (the namespace body —
//!   cpp wraps namespace contents in a `declaration_list` node, unlike
//!   go's direct source_file children), `field_declaration_list` (class
//!   body), `enumerator_list`, `compound_statement` — the rust/python/go
//!   finding, repeated per-scope.
//! - **`access_specifier` (`public:`/`private:`) is a named, non-trivia
//!   sibling** inside `field_declaration_list` — it breaks byte-strict
//!   field adjacency exactly like a comment would, and it is content,
//!   not trivia. The class-member case's public section keeps it at
//!   index 0 so it never interferes with the measured pair.
//! - **Bodies make the kind**: a function WITH a body is
//!   `function_definition` (free, out-of-class qualified, in-class
//!   inline, and template bodies alike — the template wrapper is
//!   `template_declaration > function_definition`); a function
//!   DECLARATION (header prototype or in-class method decl) is a plain
//!   `declaration`/`field_declaration`. The def-adjacency idiom counts
//!   only bodies — the declaration-kind distinction the corpus keeps
//!   visible.
//! - An out-of-class method definition `float DelayLine::Process(…)`
//!   names itself through `function_declarator > qualified_identifier >
//!   [namespace_identifier, ::, identifier]` — the class part parses as
//!   a namespace identifier and the method name is a plain `identifier`
//!   (not a `field_identifier`; that kind is the in-class declarator's).
//!
//! Measured outcome (fixture module, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in C++ too —
//! byte-strict loses 40% of file-level function-adjacency pairs (3 vs
//! 5, the doc-comment-above-function idiom) and every curated
//! comment-separated pair is strict-invisible. ADR-0010's cpp entry
//! stands; the cpp trivia table stays `["comment"]`.

#![cfg(feature = "lang-cpp")]

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
        "(under translation_unit (seq (kind function_definition) (kind function_definition)))",
        include_str!("fixtures/kp6_cpp/doc_func_pair.cpp"),
        1,
        2,
        "doc-comment-above-function",
    ),
    (
        "(under translation_unit (seq (kind function_definition) (kind function_definition)))",
        include_str!("fixtures/kp6_cpp/method_out_of_class.cpp"),
        1,
        2,
        "out-of-class-method-adjacency",
    ),
    (
        "(under field_declaration_list (seq (kind field_declaration) (kind field_declaration)))",
        include_str!("fixtures/kp6_cpp/class_member_fields.cpp"),
        2,
        3,
        "class-member-adjacency",
    ),
    (
        "(under compound_statement (seq (kind declaration) (kind declaration)))",
        include_str!("fixtures/kp6_cpp/local_decls.cpp"),
        1,
        2,
        "local-declaration-adjacency",
    ),
    (
        "(under enumerator_list (seq (kind enumerator) (kind enumerator)))",
        include_str!("fixtures/kp6_cpp/enum_members.cpp"),
        2,
        3,
        "enumerator-adjacency",
    ),
    (
        "(under translation_unit (seq (kind preproc_include) (kind preproc_include)))",
        include_str!("fixtures/kp6_cpp/include_group.cpp"),
        2,
        3,
        "include-group-adjacency",
    ),
    (
        "(under declaration_list (seq (kind function_definition) (kind function_definition)))",
        include_str!("fixtures/kp6_cpp/namespace_funcs.cpp"),
        1,
        2,
        "namespace-body-adjacency",
    ),
];

/// The bundled real-world-style corpus (the cpp e2e fixture module).
const CORPUS: &str = "tests/fixtures/cpp";

/// The function-adjacency rule (CASES[0]'s), used for the corpus-wide
/// hand-computed leg.
const CORPUS_RULE: &str =
    "(under translation_unit (seq (kind function_definition) (kind function_definition)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.cpp"), source).expect("write snippet");
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
/// comment-separated pairs. The member/enumerator/include cases are the
/// multi-run shape: four members with one dividing comment give strict
/// 2 (two runs) and skip 3 (three runs).
#[test]
fn kp6_cpp_curated_counts_match_the_measured_ground_truth() {
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

/// C1's fence: the corpus holds at least the five measured fixtures and
/// every one parses CLEAN under the cpp grammar (zero ERROR/MISSING —
/// the same fence the registry fixtures carry). A broken fixture or a
/// grammar regression fails here, not in a scan count.
#[test]
fn kp6_cpp_corpus_holds_five_clean_fixtures() {
    use janitor_prime::ast::ffi_shim;
    use janitor_prime::ast::lang::LangRegistry;

    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/kp6_cpp");
    let mut files: Vec<_> = std::fs::read_dir(&root)
        .expect("kp6_cpp fixture dir")
        .map(|entry| entry.expect("entry").path())
        .collect();
    files.sort();
    assert!(
        files.len() >= 5,
        "C1: at least five curated fixtures, found {}",
        files.len()
    );
    assert!(
        files
            .iter()
            .all(|p| p.extension().is_some_and(|e| e == "cpp")),
        "the corpus is cpp-only"
    );

    let registry = LangRegistry::new();
    let id = registry.for_extension("cpp").expect("cpp registered");
    let mut parser = registry.parser(id).expect("parser sets up");
    for file in &files {
        let source = std::fs::read(file).expect("fixture read");
        let tree = ffi_shim::parse_bytes(&mut parser, &source, None).expect("tree parses");
        assert!(
            !tree.root_node().has_error(),
            "{}: curated fixtures parse clean",
            file.display()
        );
    }
}

/// Corpus-wide counts for the function-adjacency idiom, hand-computed
/// from the fixture module's trees: `dsp/delay.cpp` carries the
/// ctor/Process/Reset definitions adjacent plus MixTaps behind its doc
/// comment (strict 2, skip 3), `main.cpp` has NormalizeGain/SweepGain
/// split by SweepGain's doc comment with main after (strict 1, skip 2),
/// and `dsp/delay.h` contributes nothing — its functions live inside
/// the namespace's `declaration_list` (MixTaps is a body-less
/// `declaration`, Clamp's body sits under `template_declaration`), not
/// at translation_unit level. Total: **strict 3, skip 5**.
#[test]
fn kp6_cpp_corpus_wide_counts_are_hand_computed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    for (strict, expected) in [(true, 3_u64), (false, 5_u64)] {
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
/// ADR-0010 cpp entry stands on. Assertions are the invariant (skipping
/// comments can only add matches, and every curated comment-separated
/// idiom stays strict-lossy — the sensitivity pin: a wrong trivia kind
/// would collapse skip onto strict and this fails); the printed table
/// is the recorded outcome quoted in the promotion report.
#[test]
fn kp6_cpp_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-cpp adjacency semantics - fixture module ({CORPUS}):");
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
        // The corpus module exercises the two def-idiom rules directly
        // (out-of-class methods in delay.cpp, free functions in
        // main.cpp) — their corpus counts must diverge the same way the
        // curated snippets do.
        if *rule == CORPUS_RULE {
            assert_eq!(
                (strict, skip),
                (3, 5),
                "{name}: the hand-computed module totals"
            );
        }
    }
}

/// The cpp trivia table's factual basis, as a fence: `comment` is the
/// single comment-class named kind and it appears as one node PER LINE,
/// it is a named child of the innermost container at every scope
/// measured (translation_unit, declaration_list, field_declaration_list,
/// enumerator_list, compound_statement), `access_specifier` is a named
/// content sibling (never trivia), and the namespace body is a
/// `declaration_list` (functions are ITS children, not the namespace's).
/// If a grammar upgrade changes any of this, the trivia table must be
/// re-measured.
#[test]
fn cpp_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    let source = "#include <cstdint>\n// one\n// two\n#include \"a.h\"\n\nnamespace n {\n// section\nint f() { return 1; }\nint g() { return 2; }\n}  // namespace n\n";
    let registry = LangRegistry::new();
    let id = registry.for_extension("cpp").expect("cpp registered");
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
    fn find<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        if node.kind() == kind {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if let Some(hit) = find(child, kind) {
                return Some(hit);
            }
        }
        None
    }

    let root = tree.root_node();
    // Every comment-class named kind in the whole tree is `comment` —
    // and the two-line `// one // two` group is two sibling nodes.
    let mut comment_kinds: Vec<String> = Vec::new();
    let mut stack = vec![root];
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
        "line comments are one `comment` node per line, the only comment kind"
    );

    // translation_unit: the per-line comments sit between the includes,
    // and the trailing close-brace comment parks at file level, AFTER
    // the namespace (measured — it is not inside the declaration_list).
    assert_eq!(
        named_kinds(root),
        vec![
            "preproc_include".to_owned(),
            "comment".to_owned(),
            "comment".to_owned(),
            "preproc_include".to_owned(),
            "namespace_definition".to_owned(),
            "comment".to_owned(),
        ],
        "include-group section comments are named siblings at file level"
    );

    // The namespace body is a declaration_list whose children are the
    // section comment and the two function definitions.
    let namespace = find(root, "namespace_definition").expect("namespace present");
    assert_eq!(
        named_kinds(namespace),
        vec![
            "namespace_identifier".to_owned(),
            "declaration_list".to_owned(),
        ],
        "namespace_definition wraps its body in a declaration_list"
    );
    let body = find(namespace, "declaration_list").expect("body present");
    assert_eq!(
        named_kinds(body),
        vec![
            "comment".to_owned(),
            "function_definition".to_owned(),
            "function_definition".to_owned(),
        ],
        "the section comment and the defs are declaration_list siblings"
    );
}
