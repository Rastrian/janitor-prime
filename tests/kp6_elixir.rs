//! The three KP-6-elixir curated patterns as integration tests (F9d task 3;
//! the F1 spike method): every idiom hand-computed over curated snippets in
//! both trivia modes, plus the measured divergence table over the bundled
//! elixir corpus (`tests/fixtures/elixir/` — the same fixture repo the
//! elixir e2e acceptance drives) and the grammar-shape fence that pins the
//! elixir facts ADR-0010's table and the norm table's FUNCTION decision
//! stand on.
//!
//! Elixir grammar facts this corpus pins (verified against
//! tree-sitter-elixir 0.3.5's own tree output, 2026-09-15):
//!
//! - `comment` is the only comment-class named kind between siblings
//!   (45 named kinds; nothing else is comment-shaped), and it attaches
//!   to the innermost enclosing suite — the module's `do_block`, a
//!   def's `do_block`, or `source` at top level. The trivia table
//!   `["comment"]` is the measured entry.
//! - Named functions are **`call` nodes**: `def add(a, b), do: a + b`
//!   and `def add(a, b, c) do … end` measure identically — target
//!   `identifier` (`def`/`defp`), the head `add(a, b)` a nested `call`
//!   inside `arguments`, the body a `do_block` child whose statements
//!   are DIRECT children (no suite wrapper — `do_block` is both the
//!   expression and the suite; the norm table's BLOCK entry and the
//!   FUNCTION=call decision both stand on this).
//! - Module attributes (`@doc`/`@spec`/`@moduledoc`/`@unit`) are
//!   **`unary_operator`** nodes wrapping a `call` — named siblings of
//!   the def `call`s inside the module's `do_block`. They are
//!   content-bearing and deliberately NOT trivia.
//! - **Clause-head adjacency** (the brief's explicit question):
//!   successive `def` clauses of the SAME name are ordinary sibling
//!   `call`s — a comment between clause heads is what byte-strict
//!   loses (pattern 1's divergence); a comment between two DIFFERENT
//!   functions behaves the same way, so there is no same-name special
//!   case in the grammar to study — it is pure sibling adjacency.
//! - Assignment `a = b` parses as `binary_operator`, and a pipe chain
//!   `x |> f() |> g()` collapses to ONE `binary_operator` sibling —
//!   def bodies in the corpus never surface (call, call) adjacency at
//!   body level (recorded as the pattern-3-adjacent surprise the
//!   grammar-shape fence below pins).
//!
//! Measured outcome (fixture corpus, default skip mode vs
//! `--strict-trivia`): comment-skipping is load-bearing in elixir —
//! byte-strict loses 42.9% of def-clause adjacency pairs (4 vs 7).
//! ADR-0010's elixir entry stands; the trivia table stays
//! `["comment"]`.

#![cfg(feature = "lang-elixir")]

use std::path::Path;

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

/// (rule, curated snippet, strict, skip, name) — hand-computed ground
/// truth, straight from the tree shapes dumped while authoring the
/// corpus. Patterns 2 and 3 agree across modes (no comments inside the
/// chain); pattern 1 is the divergence the tier's debt note records.
const CASES: &[(&str, &str, u64, u64, &str)] = &[
    (
        "(under do_block (seq (kind call) (kind call)))",
        include_str!("fixtures/kp6_elixir/clause_heads.ex"),
        0,
        1,
        "comment-between-clause-heads",
    ),
    (
        "(under do_block (seq (kind unary_operator) (kind call)))",
        include_str!("fixtures/kp6_elixir/spec_above_def.ex"),
        1,
        1,
        "spec-above-def adjacency",
    ),
    (
        "(under do_block (seq (kind unary_operator) (kind unary_operator)))",
        include_str!("fixtures/kp6_elixir/attr_chain.ex"),
        2,
        2,
        "@moduledoc/@doc attribute chain",
    ),
];

/// The bundled elixir corpus (the elixir e2e fixture repo).
const CORPUS: &str = "tests/fixtures/elixir";

/// The def-clause adjacency rule (CASES[0]'s), used for the corpus-wide
/// hand-computed leg.
const CORPUS_RULE: &str = "(under do_block (seq (kind call) (kind call)))";

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.ex"), source).expect("write snippet");
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
/// comment-separated clause pair; the attribute chains lose nothing
/// because attributes are content siblings, not trivia.
#[test]
fn kp6_elixir_curated_counts_match_the_measured_ground_truth() {
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

/// Corpus-wide counts for the def-clause adjacency idiom, hand-computed
/// from the fixture repo's module bodies (assignment/pipe statements are
/// `binary_operator`s, so only module-level clause adjacency surfaces):
/// `Math` — [@moduledoc @doc @spec call, comment, call, call, call]:
/// strict 2 (normalize_value/2→scale, scale→totals), skip 3 (the
/// comment-separated normalize_value/1→/2 pair joins); `ReportWeb` —
/// [use-call, comment, call, call]: strict 1 (show→summary), skip 2
/// (use→show joins); `Catalog` — [@moduledoc @unit @doc call, comment,
/// call, call]: strict 1 (label→render), skip 2 (tagged_count→label
/// joins). Total: **strict 4, skip 7**.
#[test]
fn kp6_elixir_corpus_wide_counts_are_hand_computed() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    for (strict, expected) in [(true, 4_u64), (false, 7_u64)] {
        let mut params = ScanParams::new(CORPUS_RULE, &root);
        params.strict_trivia = strict;
        let session = Session::new();
        let report = session
            .scan(&params)
            .unwrap_or_else(|err| panic!("corpus scan: {err}"));
        assert_eq!(
            report.matches.len() as u64,
            expected,
            "def-clause adjacency over the fixture corpus (strict={strict})"
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
/// ADR-0010 elixir entry stands on. Assertions are the invariant
/// (skipping comments can only add matches); the printed table is the
/// recorded outcome quoted in the PLAN §5 corollary.
#[test]
fn kp6_elixir_corpus_divergence_table() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join(CORPUS);
    println!("KP-6-elixir adjacency semantics - fixture corpus ({CORPUS}):");
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

/// The elixir trivia table's factual basis, as a fence: `comment` is a
/// named sibling of the innermost suite; the module suite is the
/// defmodule `call`'s `do_block` with statements as DIRECT children
/// (no wrapper — unlike ruby's body_statement); both def shapes are
/// `call`; attributes are `unary_operator`s; assignment and pipe
/// chains are `binary_operator` siblings. If a grammar upgrade changes
/// any of this, the trivia table and the FUNCTION=call decision must
/// be re-measured.
#[test]
fn elixir_trivia_shapes_are_measured() {
    use janitor_prime::ast::ffi_shim::{self, Node};
    use janitor_prime::ast::lang::LangRegistry;

    let source = "defmodule Probe do\n  @spec add(integer(), integer()) :: integer()\n  def add(a, b), do: a + b\n\n  # between successive clauses of the same head\n  def add(a, b, c) do\n    a + b + c\n  end\nend\n";
    let registry = LangRegistry::new();
    let id = registry.for_extension("ex").expect("elixir registered");
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
    fn child_of_kind<'a>(node: Node<'a>, kind: &str) -> Option<Node<'a>> {
        let mut cursor = node.walk();
        node.children(&mut cursor)
            .find(|child| child.is_named() && child.kind() == kind)
    }

    // source > call(defmodule) > do_block: the module suite, with the
    // attributes, def clauses and comment as DIRECT named children —
    // unary_operator, unary_operator, call, comment, call in order.
    let root = tree.root_node();
    let module = child_of_kind(root, "call").expect("defmodule is a call");
    let suite = child_of_kind(module, "do_block").expect("module's do_block");
    assert_eq!(
        named_kinds(suite),
        vec![
            "unary_operator".to_owned(),
            "call".to_owned(),
            "comment".to_owned(),
            "call".to_owned(),
        ],
        "the module suite: @spec unary_operator, keyword-clause call, \
         comment, do-block-clause call — no suite wrapper",
    );

    // Both def shapes are `call`, and the head `add(a, b)` is a nested
    // call inside `arguments` (the FUNCTION=call decision's basis).
    let keyword_clause = child_of_kind(suite, "call").expect("first def clause");
    let head = child_of_kind(
        child_of_kind(keyword_clause, "arguments").expect("clause arguments"),
        "call",
    )
    .expect("the head add(a, b) is a call inside arguments");
    assert_eq!(
        child_of_kind(head, "arguments").map(|n| n.kind().to_owned()),
        Some("arguments".to_owned()),
        "the head carries its own parameter list"
    );

    // A do-block clause's body: statements are direct children of the
    // clause's do_block — `a + b + c` one binary_operator sibling.
    let do_clause = suite.named_child(3).expect("the do-block clause call");
    let body = child_of_kind(do_clause, "do_block").expect("clause's do_block");
    assert_eq!(
        named_kinds(body),
        vec!["binary_operator".to_owned()],
        "do_block is the suite itself: statements are its direct children"
    );

    // Assignment and pipe chains are binary_operator siblings, never
    // call pairs — def bodies surface no (call, call) adjacency in this
    // corpus shape (the corpus-wide hand count stands on it).
    let bodies = "defmodule Bodies do\n  def f(pair, xs) do\n    {a, b} = pair\n    g(a, b)\n  end\n\n  def h(xs) do\n    xs\n    |> Enum.map(&f/1)\n    |> Enum.sum()\n  end\nend\n";
    let tree = ffi_shim::parse_bytes(&mut parser, bodies.as_bytes(), None).expect("bodies parse");
    assert!(!tree.root_node().has_error());
    let module = child_of_kind(tree.root_node(), "call").expect("defmodule");
    let suite = child_of_kind(module, "do_block").expect("suite");
    let mut do_blocks = suite
        .named_children(&mut suite.walk())
        .filter(|child| child.kind() == "call")
        .filter_map(|call| child_of_kind(call, "do_block"))
        .collect::<Vec<_>>();
    assert_eq!(do_blocks.len(), 2, "one do_block per clause");
    assert_eq!(
        named_kinds(do_blocks.remove(0)),
        vec!["binary_operator".to_owned(), "call".to_owned()],
        "assignment is a binary_operator sibling before the call"
    );
    assert_eq!(
        named_kinds(do_blocks.remove(0)),
        vec!["binary_operator".to_owned()],
        "the whole pipe chain collapses to ONE binary_operator sibling"
    );

    // The adjacency caveat, measured: a comment between successive
    // clauses of the same head breaks the seq ONLY in strict mode —
    // the pattern-1 divergence, in one assertion pair.
    assert_eq!(scan_counts(CORPUS_RULE, source, true), 0);
    assert_eq!(scan_counts(CORPUS_RULE, source, false), 1);
}
