//! KP-7 (PLAN §5 KP-7, ADR-0005, §3 F4.9): the scoped NFA's linearity, made
//! falsifiable — next to a deliberately naive backtracking baseline that
//! documents the explosion we avoid.
//!
//! **The adversarial shape.** An alternation-heavy pattern — `(seq
//! (any-of _ _) …)` repeated *k* times — over a wide scope (a body with *n*
//! statements). Every `(any-of _ _)` has two branches that both consume
//! one child, so an accepting run of length *k* has **2^k distinct
//! accepting paths**. The scoped matcher settles each `(child_index, pc)`
//! state exactly once (the DP memo — ADR-0005's mechanical guarantee), so
//! its cost is `O(n × k)`. The naive baseline explores every path: `2^k`
//! per accepting start. Depth *k* is the exponential axis; width *n* the
//! linear one; the deep-tree leg adds nesting depth.
//!
//! **Families:**
//! - `alternation/scoped`  — k ∈ {8, 12, 16, 20, 24}, n = 128: must scale
//!   ~linearly in k (the claim).
//! - `alternation/naive`   — k ∈ {8, 12, 16} only (the run completes; k=24
//!   would be ~2^24 ≈ 17M paths per start × 105 starts — the documented
//!   explosion, not something CI should pay for).
//! - `linearity/scoped`    — k = 12, n ∈ {256, 512, 1024, 2048}: per-child
//!   throughput must stay flat (linear total).
//! - `nested/scoped`       — a d-deep wildcard-under chain over a
//!   deep/wide tree: the `(node, entry)` scope cache keeps re-entered
//!   scopes O(1) (the O(Σ scopes) bound).
//! - `filter-text/scoped`  — v0.3 B1: match + the post-match leaf-text
//!   filter over the `linearity` shape (n ∈ {256, 512, 1024, 2048}) —
//!   the filter adds O(matches × clauses), never NFA work, so per-
//!   element cost stays flat in n.
//!
//! **p50/p90:** run with `KP7_SUMMARY=1 cargo bench --bench
//! matcher_linearity -- --quick` (or `cargo bench --bench
//! matcher_linearity` normally) — the summary loop measures full-match
//! wall times per family×size and prints p50/p90. The measured numbers go
//! into PLAN §5's KP-7 row (append-only) and the PR body, never a CI gate
//! threshold.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(clippy::expect_used)] // bench harness: measurement script, not the product

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use smol_str::SmolStr;

use janitor_prime::ast::compile::{Instr, TriviaPolicy, compile_rule};
use janitor_prime::ast::dsl::parse_rule;
use janitor_prime::ast::eval::Matcher;
use janitor_prime::ast::lang::LangRegistry;
use janitor_prime::ast::parse::parse_snapshot;

/// A body with `n` sibling statements: the wide scope. Nested braces add
/// depth `d` inside every sibling — the wide/deep adversarial tree. Each
/// nesting level's block carries a `let` plus the nested block as its tail
/// expression (two named children per scope), and the innermost block has
/// the two `let`s the nested rule matches.
fn adversarial_source(n: usize, d: usize) -> String {
    let mut out = String::from("fn adversarial() {\n");
    for i in 0..n {
        out.push_str("    ");
        for j in 0..d {
            out.push_str(&format!("{{ let x{i}_{j} = {j}; "));
        }
        out.push_str("let final_a = 1; let final_b = 2;");
        for _ in 0..d {
            out.push_str(" }");
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// `(under block (seq (any-of _ _) …))` with `k` alternation elements —
/// 2^k accepting paths per run, the exponential axis.
fn alternation_rule(k: usize) -> String {
    let elems: Vec<String> = (0..k).map(|_| "(any-of _ _)".to_owned()).collect();
    format!("(under block (seq {}))", elems.join(" "))
}

/// A `d`-deep wildcard-under chain — every node is a scope, and the
/// `(node, entry)` cache is the only thing keeping re-entry linear (each
/// scope's pre-pass tests every child against every `Under` instruction).
fn nested_rule(d: usize) -> String {
    let mut rule = String::new();
    for _ in 0..d {
        rule.push_str("(under _ ");
    }
    rule.push_str("(under block (seq _ _))");
    for _ in 0..d {
        rule.push(')');
    }
    rule
}

struct Rig {
    program: std::sync::Arc<janitor_prime::ast::compile::Program>,
    tree: janitor_prime::ast::ffi_shim::Tree,
    bytes: std::sync::Arc<[u8]>,
}

fn rig(rule: &str, n: usize, d: usize) -> Rig {
    let registry = LangRegistry::new();
    let lang = registry
        .for_extension("rs")
        .expect("the bench builds with lang-rust");
    let parsed = parse_rule(rule).expect("bench rule parses");
    let program =
        compile_rule(&parsed, &registry, lang, TriviaPolicy::Skip).expect("bench rule compiles");
    let mut parser = registry.parser(lang).expect("parser");
    let source = adversarial_source(n, d);
    let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(source.into_bytes());
    let tree = parse_snapshot(&mut parser, &bytes).expect("adversarial source parses");
    Rig {
        program: std::sync::Arc::new(program),
        tree,
        bytes,
    }
}

impl Rig {
    /// One full scoped match; returns the match count (kept so a
    /// semantics regression cannot silently bench an empty match set).
    fn run(&self) -> usize {
        let mut out = Vec::new();
        let mut matcher = Matcher::new(&self.program, &self.tree, 0);
        matcher
            .matches(&SmolStr::from("bench.rs"), &mut out)
            .expect("match runs");
        out.len()
    }

    /// Match plus the v0.3 B1 post-match text filter over the same run —
    /// the family that pins "filters are O(matches × clauses) on top of
    /// the linear match, never inside the NFA" (docs/DSL.md §3).
    fn run_filtered(&self) -> usize {
        let mut out = Vec::new();
        let mut matcher = Matcher::new(&self.program, &self.tree, 0);
        matcher
            .matches(&SmolStr::from("bench.rs"), &mut out)
            .expect("match runs");
        janitor_prime::ast::eval::filter_text(&self.program, &self.tree, &self.bytes, &mut out);
        out.len()
    }
}

/// The deliberately naive baseline: recursive path enumeration over the
/// SAME compiled program — `Split` tries both arms, no memo, each
/// `(i, pc)` visited once per path through it. Counting accepting starts
/// (not paths) keeps the result comparable; the cost is the path count.
/// `fuel` bounds runaway exploration so the bench always terminates.
fn naive_accepting_starts(program: &[Instr], kinds: &[u16], fuel: &mut u64) -> usize {
    fn accept(program: &[Instr], kinds: &[u16], i: usize, pc: usize, fuel: &mut u64) -> bool {
        *fuel += 1;
        if *fuel > 200_000_000 {
            return false; // explosion guard: measurement, not hang
        }
        match program[pc] {
            Instr::Match => true,
            Instr::Jmp(t) => accept(program, kinds, i, t as usize, fuel),
            Instr::Split(a, b) => {
                accept(program, kinds, i, a as usize, fuel)
                    || accept(program, kinds, i, b as usize, fuel)
            }
            Instr::CapStart(_) | Instr::CapEnd(_) => accept(program, kinds, i, pc + 1, fuel),
            Instr::Kind(k) => {
                i < kinds.len() && kinds[i] == k && accept(program, kinds, i + 1, pc + 1, fuel)
            }
            Instr::Any => i < kinds.len() && accept(program, kinds, i + 1, pc + 1, fuel),
            // A nested scope needs the child's own children — the naive
            // baseline treats it as "kind matches, inner accepted"
            // optimistically for bench purposes; the flat adversarial
            // pattern contains none, so this arm never fires here.
            Instr::Under(kind, _, _) => {
                i < kinds.len()
                    && kind.is_none_or(|k| kinds[i] == k)
                    && accept(program, kinds, i + 1, pc + 1, fuel)
            }
        }
    }
    let mut starts = 0;
    for s in 0..kinds.len() {
        if accept(program, kinds, s, 0, fuel) {
            starts += 1;
        }
    }
    starts
}

/// The scope's children kinds for the naive baseline: the fn body block's
/// named children (the same scope the scoped matcher runs over).
fn body_children_kinds(tree: &janitor_prime::ast::ffi_shim::Tree) -> Vec<u16> {
    use janitor_prime::ast::ffi_shim::Node;
    fn find_body(node: Node<'_>) -> Option<Node<'_>> {
        if node.kind() == "block" && node.parent().is_some_and(|p| p.kind() == "function_item") {
            return Some(node);
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            if let Some(found) = find_body(child) {
                return Some(found);
            }
        }
        None
    }
    let body = find_body(tree.root_node()).expect("adversarial source has a fn body");
    let mut cursor = body.walk();
    body.named_children(&mut cursor)
        .map(|c| c.kind_id())
        .collect()
}

fn bench_alternation(c: &mut Criterion) {
    let mut group = c.benchmark_group("kp7/alternation");
    let n = 128;
    for k in [8_usize, 12, 16, 20, 24] {
        let rig = rig(&alternation_rule(k), n, 1);
        let matches = rig.run();
        assert!(matches > 0, "the adversarial pattern must match");
        group.bench_with_input(BenchmarkId::new("scoped", k), &k, |b, _| {
            b.iter(|| rig.run())
        });
    }
    for k in [8_usize, 12, 16] {
        let rig = rig(&alternation_rule(k), n, 1);
        let program: Vec<Instr> = rig.program.instrs().to_vec();
        let kinds = body_children_kinds(&rig.tree);
        let mut fuel = 0_u64;
        let starts = naive_accepting_starts(&program, &kinds, &mut fuel);
        assert!(starts > 0, "naive agrees on acceptability");
        group.bench_with_input(BenchmarkId::new("naive", k), &k, |b, _| {
            b.iter(|| {
                let mut fuel = 0_u64;
                naive_accepting_starts(&program, &kinds, &mut fuel)
            })
        });
    }
    group.finish();
}

fn bench_linearity(c: &mut Criterion) {
    let mut group = c.benchmark_group("kp7/linearity");
    for n in [256_usize, 512, 1024, 2048] {
        let rig = rig(&alternation_rule(12), n, 1);
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("scoped", n), &n, |b, _| {
            b.iter(|| rig.run())
        });
    }
    group.finish();
}

fn bench_nested(c: &mut Criterion) {
    let mut group = c.benchmark_group("kp7/nested");
    for d in [2_usize, 4, 8, 16] {
        // Rule depth d needs tree depth ≥ d (18 leaves margin); 32 sibling
        // statements give the cache re-entry pressure across scopes.
        let rig = rig(&nested_rule(d), 32, 18);
        let matches = rig.run();
        assert!(matches > 0, "the nested chain must reach a match");
        group.throughput(Throughput::Elements((32 * 18) as u64));
        group.bench_with_input(BenchmarkId::new("scoped", d), &d, |b, _| {
            b.iter(|| rig.run())
        });
    }
    group.finish();
}

/// v0.3 B1: the leaf-text filter pass over the SAME wide-scope runs as
/// `kp7/linearity` — every let captured, a text clause keeping only the
/// innermost `final_a` bindings. The claim the family falsifies: the
/// filter's cost is O(matches × clauses) *on top of* the linear match, so
/// per-element throughput stays flat as n grows (the filter never touches
/// the NFA, its DP, or the tree walk the matcher already paid for).
fn bench_filter_text(c: &mut Criterion) {
    let mut group = c.benchmark_group("b1/filter-text");
    let rule = "(under block (seq (capture @l let_declaration)) \
                (filters (text @l \"final_a\")))";
    for n in [256_usize, 512, 1024, 2048] {
        let rig = rig(rule, n, 1);
        let unfiltered = rig.run();
        let filtered = rig.run_filtered();
        assert_eq!(filtered, n, "exactly one final_a per sibling statement");
        assert!(
            unfiltered > filtered,
            "the clause must filter (kept {filtered} of {unfiltered})"
        );
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(BenchmarkId::new("match+filter", n), &n, |b, _| {
            b.iter(|| rig.run_filtered())
        });
    }
    group.finish();
}

/// Manual p50/p90 summary (the numbers the PLAN row carries). Criterion
/// owns the statistical reporting for the benches above; this loop prints
/// straightforward full-match p50/p90 per family×size so the scaling is
/// readable at a glance.
fn summary() {
    let summarize = |family: &str, label: String, mut f: Box<dyn FnMut() -> usize>| {
        let samples = 30;
        let mut times: Vec<f64> = Vec::with_capacity(samples);
        f(); // warmup
        for _ in 0..samples {
            let started = std::time::Instant::now();
            let count = f();
            let elapsed = started.elapsed();
            if count == 0 {
                eprintln!("{family}/{label}: WARNING no matches");
            }
            times.push(elapsed.as_secs_f64() * 1e6); // µs
        }
        times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let p50 = times[samples / 2];
        let p90 = times[(samples * 9) / 10];
        println!("{family:>18}/{label:<8} p50 {p50:10.1} µs   p90 {p90:10.1} µs");
    };

    for k in [8_usize, 12, 16, 20, 24] {
        let rig = rig(&alternation_rule(k), 128, 1);
        summarize(
            "alternation/scoped",
            format!("k{k}"),
            Box::new(move || rig.run()),
        );
    }
    for k in [8_usize, 12, 16] {
        let rig = rig(&alternation_rule(k), 128, 1);
        let program: Vec<Instr> = rig.program.instrs().to_vec();
        let kinds = body_children_kinds(&rig.tree);
        summarize(
            "alternation/naive",
            format!("k{k}"),
            Box::new(move || {
                let mut fuel = 0_u64;
                naive_accepting_starts(&program, &kinds, &mut fuel)
            }),
        );
    }
    for n in [256_usize, 512, 1024, 2048] {
        let rig = rig(&alternation_rule(12), n, 1);
        summarize(
            "linearity/scoped",
            format!("n{n}"),
            Box::new(move || rig.run()),
        );
    }
    for d in [2_usize, 4, 8, 16] {
        let rig = rig(&nested_rule(d), 32, 18);
        summarize(
            "nested/scoped",
            format!("d{d}"),
            Box::new(move || rig.run()),
        );
    }
    let rule = "(under block (seq (capture @l let_declaration)) \
                (filters (text @l \"final_a\")))";
    for n in [256_usize, 512, 1024, 2048] {
        let rig = rig(rule, n, 1);
        summarize(
            "filter-text/scoped",
            format!("n{n}"),
            Box::new(move || rig.run_filtered()),
        );
    }
}

fn kp7(c: &mut Criterion) {
    if std::env::var("KP7_SUMMARY").is_ok() {
        summary();
        return;
    }
    bench_alternation(c);
    bench_linearity(c);
    bench_nested(c);
    bench_filter_text(c);
}

criterion_group!(benches, kp7);
criterion_main!(benches);
