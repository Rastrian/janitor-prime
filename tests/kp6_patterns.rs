//! The five KP-6 curated patterns as integration tests, with the spike's
//! measured ground truth (PLAN §3 F4.8; memo
//! `docs/planning/spikes/KP-6.md`, `benches/spike-kp6-outcome.json`):
//! every idiom once clean and once comment-separated, hand-computed
//! strict/skip counts. The evaluator must reproduce the spike's numbers
//! exactly — this is the ADR-0010 semantics made falsifiable end to end.

use janitor_prime::orchestrator::Session;
use janitor_prime::scan::ScanParams;

/// (pattern, container snippet) pairs straight from the KP-6 spike — the
/// same five real-world idioms, verbatim sources.
const CASES: &[(&str, &str, u64, u64, &str)] = &[
    (
        "(under block (seq (kind let_declaration) (kind let_declaration)))",
        include_str!("fixtures/let_pair.rs"),
        1,
        2,
        "let-statement-adjacency",
    ),
    (
        "(under match_block (seq (kind match_arm) (kind match_arm)))",
        include_str!("fixtures/match_arms.rs"),
        1,
        2,
        "match-arm-adjacency",
    ),
    (
        "(under source_file (seq (kind attribute_item) (kind function_item)))",
        include_str!("fixtures/attr_fn.rs"),
        0,
        1,
        "attribute-on-item",
    ),
    (
        "(under field_expression (seq (kind call_expression) (kind field_identifier)))",
        include_str!("fixtures/chain.rs"),
        2,
        3,
        "method-chain-link",
    ),
    (
        "(under if_expression (seq _ (kind block) (kind else_clause)))",
        include_str!("fixtures/if_else.rs"),
        1,
        2,
        "if-else-adjacency",
    ),
];

fn scan_counts(rule: &str, source: &str, strict: bool) -> u64 {
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::write(tmp.path().join("curated.rs"), source).expect("write snippet");
    let mut params = ScanParams::new(rule, tmp.path());
    params.strict_trivia = strict;
    let session = Session::new();
    session
        .scan(&params)
        .unwrap_or_else(|err| panic!("{rule}: {err}"))
        .matches
        .len() as u64
}

/// The spike's curated ground truth, both modes: strict loses exactly the
/// comment-separated pairs; attribute-on-item is strict-invisible (the
/// deciding evidence for ADR-0010's default).
#[test]
fn kp6_curated_counts_match_the_measured_ground_truth() {
    for (rule, source, strict, skip, name) in CASES {
        assert_eq!(
            &scan_counts(rule, source, true),
            strict,
            "{name}: --strict-trivia count must equal the spike's measurement"
        );
        assert_eq!(
            &scan_counts(rule, source, false),
            skip,
            "{name}: default (comment-skipping) count must equal the spike's measurement"
        );
    }
}

/// One pattern over the whole corpus directory: the let-adjacency idiom
/// also occurs inside `if_else.rs` (`let a = …; let b = …;` — adjacent,
/// comment-free), so the corpus-wide counts are its own file's 1/2 plus
/// if_else's 1/1: **strict 2, skip 3**. Hand-computed, not derived from the
/// implementation — cross-file aggregation with canonical path order.
#[test]
fn kp6_corpus_wide_counts_are_hand_computed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let names = [
        "let_pair.rs",
        "match_arms.rs",
        "attr_fn.rs",
        "chain.rs",
        "if_else.rs",
    ];
    for (name, case) in names.iter().zip(CASES.iter()) {
        std::fs::write(tmp.path().join(name), case.1).expect("write snippet");
    }
    for (strict, expected) in [(true, 2_u64), (false, 3_u64)] {
        let mut params = ScanParams::new(CASES[0].0, tmp.path());
        params.strict_trivia = strict;
        let session = Session::new();
        let report = session
            .scan(&params)
            .unwrap_or_else(|err| panic!("corpus scan: {err}"));
        assert_eq!(
            report.matches.len() as u64,
            expected,
            "let-adjacency over the five-file corpus (strict={strict})"
        );
        // Canonical path order: matches arrive file-grouped, paths sorted.
        let mut paths: Vec<&str> = report.matches.iter().map(|m| m.path.as_str()).collect();
        paths.dedup();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(paths, sorted, "matches are grouped in path order");
    }
}
