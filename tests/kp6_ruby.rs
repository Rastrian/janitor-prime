//! The three KP-6-ruby curated patterns as integration tests (F9c task 2;
//! the F1 spike method, lighter than python's five-pattern corpus per the
//! F9c brief but measured, not skipped): every idiom hand-computed over
//! curated snippets in both trivia modes, plus the measured divergence
//! table over the bundled ruby corpus (`tests/fixtures/ruby/` — the same
//! fixture repo the ruby e2e acceptance drives) and the grammar-shape
//! fence that pins the ruby facts ADR-0010's table stands on.
//!
//! Ruby grammar facts this corpus pins (verified against
//! tree-sitter-ruby 0.23.1's own tree output, 2026-09-15):
//!
//! - `comment` is the only comment-class named kind between siblings,
//!   and it attaches to the innermost enclosing body — `body_statement`
//!   (class and method bodies alike) or `program` at top level. The
//!   trivia table `["comment"]` is the measured entry.
//! - `attr_reader`/`attr_accessor` have **no dedicated kind**: each is a
//!   plain `call` (identifier + argument_list), a named sibling of
//!   `method` inside `body_statement` — attr→def chains are
//!   `(seq (kind call) (kind method))`.
//! - Blocks are **children, never siblings**: `items.each do … end`
//!   attaches a `do_block` child (wrapping a `body_statement`) to its
//!   `call`, and `{ … }` braces attach a `block` child (wrapping a
//!   `block_body`). No `seq` over statement siblings can ever see a
//!   block in either trivia mode — recorded as the pattern-3 surprise.
//! - **HEREDOC** (the brief's explicit question): the grammar splits a
//!   heredoc in two — `heredoc_beginning` (`<<~SQL`) is the assignment's
//!   right-hand side, while the body text floats as a named `heredoc_body`
//!   *extra* that parks as a **sibling between statements** (carrying
//!   `heredoc_content` + `heredoc_end`). It is deliberately **NOT trivia**:
//!   it carries the string's actual content, and ADR-0010 scopes Seq
//!   skipping to comment-class kinds — putting it in the table would also
//!   make Unify fingerprints ignore heredoc text (false duplicates).
//!   Consequence, fenced below: a `seq` spanning a heredoc-using
//!   statement and the next statement matches in **neither** mode. That
//!   adjacency caveat is the experimental-tier debt recorded in the norm
//!   table header and docs/SCHEMA.md.
//!
//! Measured outcome (fixture corpus, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in ruby too —
//! byte-strict loses 28.6% of method-adjacency pairs (5 vs 7), and the
//! curated attr→def chain is strict-invisible. ADR-0010's ruby entry
//! stands; the ruby trivia table stays `["comment"]`.

#![cfg(feature = "lang-ruby")]

use std::path::Path;

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

/// (rule, curated snippet, strict, skip, name) — hand-computed ground
/// truth, straight from the tree shapes dumped while authoring the
/// corpus. Pattern 3's zero row is the point: blocks are structurally
/// children of their call, so no sibling adjacency exists to lose.
const CASES: &[(&str, &str, u64, u64, &str)] = &[
    (
        "(under body_statement (seq (kind method) (kind method)))",
        include_str!("fixtures/kp6_ruby/methods_comment.rb"),
        1,
        2,
        "comment-between-methods",
    ),
    (
        "(under body_statement (seq (kind call) (kind method)))",
        include_str!("fixtures/kp6_ruby/attr_def_chain.rb"),
        0,
        1,
        "attr-to-def sibling chain",
    ),
    (
        "(under body_statement (seq (kind call) (kind do_block)))",
        include_str!("fixtures/kp6_ruby/block_shapes.rb"),
        0,
        0,
        "do-block is a child, never a sibling",
    ),
];

/// The bundled ruby corpus (the ruby e2e fixture repo).
const CORPUS: &str = "tests/fixtures/ruby";

/// The method-adjacency rule (CASES[0]'s), used for the corpus-wide
/// hand-computed leg.
const CORPUS_RULE: &str = "(under body_statement (seq (kind method) (kind method)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.rb"), source).expect("write snippet");
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
/// comment-separated pairs; pattern 3 loses nothing because there is
/// nothing to lose — blocks never were siblings.
#[test]
fn kp6_ruby_curated_counts_match_the_measured_ground_truth() {
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
/// the fixture repo's class bodies: `User` has four methods with the
/// comment before `tagged?` breaking one pair (strict 2, skip 3),
/// `Orders` three methods with the comment before `labels` (strict 1,
/// skip 2), `Catalog`'s `initialize`/`by_tag`/`banner` fully adjacent and
/// `format_line` behind the bare `private` — which parses as an
/// `identifier` statement, not a call and not trivia, so it breaks that
/// pair in BOTH modes (strict 2, skip 2). Total: **strict 5, skip 7**.
#[test]
fn kp6_ruby_corpus_wide_counts_are_hand_computed() {
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
/// ADR-0010 ruby entry stands on. Assertions are the invariant (skipping
/// comments can only add matches); the printed table is the recorded
/// outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_ruby_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-ruby adjacency semantics - fixture corpus ({CORPUS}):");
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

/// The ruby trivia table's factual basis, as a fence: `comment` is a
/// named sibling of the innermost body; both block forms attach as the
/// `block`-field child of their call (`do_block` wrapping
/// `body_statement`, `block` wrapping `block_body`); and the heredoc
/// split — `heredoc_beginning` as the assignment's right side,
/// `heredoc_body` parking between statements — which is why a seq
/// spanning a heredoc matches in neither mode. If a grammar upgrade
/// changes any of this, the trivia table must be re-measured.
#[test]
fn ruby_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    let source = "def render\n  text = <<~SQL\n    select 1\n  SQL\n  text\nend\n";
    let registry = LangRegistry::new();
    let id = registry.for_extension("rb").expect("ruby registered");
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

    // method > body_statement: [assignment, heredoc_body, identifier] —
    // the heredoc body parks BETWEEN statements as a named extra.
    let root = tree.root_node();
    let method = root
        .named_child(0)
        .expect("program's first child is the method");
    assert_eq!(method.kind(), "method");
    let body = method
        .named_child(1)
        .expect("method's children: name, body_statement");
    assert_eq!(body.kind(), "body_statement");
    assert_eq!(
        named_kinds(body),
        vec![
            "assignment".to_owned(),
            "heredoc_body".to_owned(),
            "identifier".to_owned(),
        ],
        "heredoc_body is a named sibling between the assignment and the \
         next statement — content-bearing, so deliberately not trivia"
    );

    // The heredoc split: beginning under the assignment, content+end
    // inside the floating body.
    let assignment = body.named_child(0).expect("assignment");
    assert_eq!(
        assignment.named_child(1).map(|n| n.kind().to_owned()),
        Some("heredoc_beginning".to_owned()),
        "<<~SQL is the assignment's right-hand side"
    );
    let heredoc_body = body.named_child(1).expect("heredoc_body");
    assert_eq!(
        named_kinds(heredoc_body),
        vec!["heredoc_content".to_owned(), "heredoc_end".to_owned()],
        "the body text and terminator live in the floating node"
    );

    // Both block forms are children of their call (field block), never
    // siblings — and their interiors differ: do_block > body_statement,
    // block > block_body. (Navigation by kind: a parameterized method's
    // named children are name, method_parameters, body_statement.)
    let blocks =
        "def sweep(items)\n  items.each do |x|\n    puts x\n  end\n  items.map { |y| y }\nend\n";
    let tree = ffi_shim::parse_bytes(&mut parser, blocks.as_bytes(), None).expect("blocks parse");
    assert!(!tree.root_node().has_error());
    let method = tree.root_node().named_child(0).expect("the method");
    let mut body = None;
    let mut cursor = method.walk();
    for child in method.children(&mut cursor) {
        if child.is_named() && child.kind() == "body_statement" {
            body = Some(child);
        }
    }
    let body = body.expect("sweep's body_statement");
    assert_eq!(
        named_kinds(body),
        vec!["call".to_owned(), "call".to_owned()],
        "statement siblings are the two calls alone — blocks never surface"
    );
    let do_call = body.named_child(0).expect("first call");
    assert_eq!(do_call.kind(), "call");
    // A receiver call's named children are receiver, method, block —
    // find the block by kind rather than by position.
    fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|child| child.is_named() && child.kind() == kind)
    }
    let inner = child_of_kind(do_call, "do_block").expect("call's do_block child");
    assert_eq!(
        child_of_kind(inner, "body_statement").map(|n| n.kind().to_owned()),
        Some("body_statement".to_owned()),
        "do..end wraps a body_statement"
    );
    let brace = body.named_child(1).expect("second call");
    let inner = child_of_kind(brace, "block").expect("call's block child");
    assert_eq!(
        child_of_kind(inner, "block_body").map(|n| n.kind().to_owned()),
        Some("block_body".to_owned()),
        "braces wrap a block_body — a different kind from do..end's"
    );

    // A bare visibility modifier (`private`) parses as a plain
    // `identifier` statement — not a call, not trivia — so it breaks
    // method adjacency in BOTH modes (the corpus's Catalog case).
    let modifier = "class C\n  def a\n  end\n  private\n  def b\n  end\nend\n";
    assert_eq!(scan_counts(CORPUS_RULE, modifier, true), 0);
    assert_eq!(scan_counts(CORPUS_RULE, modifier, false), 0);

    // The adjacency caveat, measured: spanning a heredoc breaks the seq
    // in BOTH modes (heredoc_body is not trivia), while the same rule
    // over a heredoc-free body matches.
    let spanning = "(under body_statement (seq (kind assignment) (kind identifier)))";
    assert_eq!(scan_counts(spanning, source, true), 0);
    assert_eq!(scan_counts(spanning, source, false), 0);
    let plain = "def render\n  text = 1\n  text\nend\n";
    assert_eq!(scan_counts(spanning, plain, true), 1);
    assert_eq!(scan_counts(spanning, plain, false), 1);
}
