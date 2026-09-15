//! KP-8 fence (PLAN §5 KP-8; F3 walk leg at §3 F3.6, re-anchored onto the
//! F4 matcher by eval-2 amendment 7): the allocation budget of the
//! pooled-cursor traversal **and** the scoped-NFA matcher, asserted with a
//! counting global allocator.
//!
//! **Budget (measured 2026-09-13/14, rustc 1.98.0, tree-sitter 0.27.0):** a
//! steady-state traversal of a warmed `CursorPool` over a ~2.7k-node tree
//! performs **0** heap allocation events — cursors and stack storage are
//! recycled through the pool, and the result vector is pre-reserved. The
//! F4 leg holds the matcher to the same standard: warmed pools (node
//! buffer, DP frames, child cursors, scope cache) make a full match of the
//! same tree allocate **0** events beyond the pre-reserved result vector.
//! The assertions carry a small explicit allowance (8) rather than a hard
//! zero so stray harness noise on a busy CI runner cannot flake the fence;
//! the *meaningful* bound is structural: a per-node leak would allocate
//! thousands of events here, not single digits.
//!
//! `#[global_allocator]` requires an `unsafe impl GlobalAlloc` — the one
//! piece of unsafe in this repository, confined to this test binary and
//! required by the trait's contract; production code stays at zero unsafe
//! (Rule T6).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use janitor_prime::ast::ffi_shim::Tree;
use janitor_prime::ast::lang::{LangId, LangRegistry};
use janitor_prime::ast::parse::parse_snapshot;
use janitor_prime::ast::walk::CursorPool;

/// Allocation *events* (alloc/alloc_zeroed/realloc), not bytes — the fence
/// cares about counts per traversal, and a single count covers both a tiny
/// and a huge leak's first occurrence.
static ALLOC_EVENTS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !ptr.is_null() {
            ALLOC_EVENTS.fetch_add(1, Ordering::Relaxed);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The measured budget: 0 in a steady state; a small explicit allowance so
/// foreign thread noise on a shared runner cannot flake the fence (see the
/// module docs — the structural claim is "not per-node", by three orders
/// of magnitude).
const ALLOWANCE: usize = 8;

/// Deterministic synthetic Rust (~80 units; a few thousand nodes) — varied
/// item shapes so the tree has real width and depth, unlike a uniform fan.
fn corpus() -> Vec<u8> {
    let mut out = String::from("use std::collections::HashMap;\n\n");
    for i in 0..80_u32 {
        out.push_str(&format!(
            "/// Doc for unit {i}.\n\
             fn unit_{i}_alpha(count: u32) -> u32 {{\n\
             \x20   let base = {i} * count;\n\
             \x20   let acc = base + count * 2;\n\
             \x20   acc\n\
             }}\n\
             \n\
             fn unit_{i}_beta(flag: bool) -> bool {{\n\
             \x20   if flag && ({i} % 2 == 0) {{\n\
             \x20       true\n\
             \x20   }} else {{\n\
             \x20       false\n\
             \x20   }}\n\
             }}\n\
             \n\
             // free-floating comment between items in unit {i}\n\
             fn unit_{i}_gamma(n: u32) -> u32 {{\n\
             \x20   match n {{\n\
             \x20       0 => 0,\n\
             \x20       m if m % 3 == 0 => m * 3,\n\
             \x20       _ => {i} + 1,\n\
             \x20   }}\n\
             }}\n\n"
        ));
    }
    out.into_bytes()
}

#[cfg(feature = "lang-rust")]
fn parsed_corpus() -> Tree {
    let registry = LangRegistry::new();
    let id: LangId = registry
        .for_extension("rs")
        .expect("this test binary builds with lang-rust");
    let mut parser = registry.parser(id).expect("ABI-verified pair (A.3.8)");
    let bytes: std::sync::Arc<[u8]> = std::sync::Arc::from(corpus());
    parse_snapshot(&mut parser, &bytes).expect("corpus is valid rust")
}

/// The KP-8 F3 leg: after one warmup traversal fills the pool, repeated
/// full traversals must not allocate beyond the documented allowance —
/// zero per visited node beyond the (pre-reserved) result vector. The
/// window is repeated and the minimum taken so foreign allocations on
/// other threads of the test binary cannot mask a real budget.
#[test]
#[cfg(feature = "lang-rust")]
fn pooled_traversal_steady_state_allocates_nothing_per_node() {
    let tree = parsed_corpus();
    let root = tree.root_node();
    let mut pool = CursorPool::new();

    // Warmup: fills the pool's spares and stack storage to the tree's max
    // depth; also establishes the node count for the result vector.
    let nodes: Vec<(u16, usize)> = pool
        .dfs(root)
        .map(|n| (n.kind_id(), n.end_byte()))
        .collect();
    assert!(
        nodes.len() > 2_000,
        "corpus must be big enough to expose a per-node leak (got {})",
        nodes.len()
    );

    let mut sink: Vec<(u16, usize)> = Vec::with_capacity(nodes.len());
    let mut best = usize::MAX;
    for _ in 0..8 {
        sink.clear();
        ALLOC_EVENTS.store(0, Ordering::Relaxed);
        for node in pool.dfs(root) {
            sink.push((node.kind_id(), node.end_byte()));
        }
        best = best.min(ALLOC_EVENTS.load(Ordering::Relaxed));
        assert_eq!(sink, nodes, "every traversal yields the same preorder");
    }

    assert!(
        best <= ALLOWANCE,
        "steady-state traversal allocated {best} times (measured 0 locally; \
         budget {ALLOWANCE}, see module docs)"
    );
}

/// The KP-8 F4 leg (eval-2 amendment 7): the fence re-anchored onto the
/// matcher itself. After one warmup run fills the matcher's pools (node
/// buffer, depth-pooled DP frames, child cursors, scope cache), repeated
/// full matches of the same tree must allocate **nothing per visited
/// node** — the only growing allocations allowed are the *result vectors*
/// (the reports out-buffer, pre-reserved by warmup, and each report's
/// captures, which this capture-free pattern keeps at zero). A per-node
/// leak in the matcher would allocate thousands of events here, not
/// single digits.
#[test]
#[cfg(feature = "lang-rust")]
fn matcher_steady_state_allocates_nothing_per_node() {
    use janitor_prime::ast::compile::{TriviaPolicy, compile_rule};
    use janitor_prime::ast::dsl::parse_rule;
    use janitor_prime::ast::eval::{MatchReport, Matcher};

    let registry = LangRegistry::new();
    let id: LangId = registry
        .for_extension("rs")
        .expect("this test binary builds with lang-rust");
    let rule = parse_rule("(under block (seq (kind let_declaration) (kind let_declaration)))")
        .expect("fence rule parses");
    let program =
        compile_rule(&rule, &registry, id, TriviaPolicy::Skip).expect("fence rule compiles");
    let tree = parsed_corpus();
    let path = smol_path("fence.rs");

    let mut matcher = Matcher::new(&program, &tree, 0);
    let mut reports: Vec<MatchReport> = Vec::new();

    // Warmup: fills every pool to this tree's maxima and reserves the
    // reports vector to its steady size.
    matcher
        .matches(&path, &mut reports)
        .expect("warmup match runs");
    assert!(
        !reports.is_empty(),
        "the corpus must match the adjacency rule for the fence to bite"
    );
    let steady_len = reports.len();

    let mut best = usize::MAX;
    for _ in 0..8 {
        reports.clear();
        ALLOC_EVENTS.store(0, Ordering::Relaxed);
        matcher.matches(&path, &mut reports).expect("match runs");
        best = best.min(ALLOC_EVENTS.load(Ordering::Relaxed));
        assert_eq!(
            reports.len(),
            steady_len,
            "every run of the same tree yields the same match set"
        );
        assert!(
            reports.iter().all(|r| r.captures.is_empty()),
            "capture-free pattern: no per-report capture vectors"
        );
    }

    assert!(
        best <= ALLOWANCE,
        "steady-state matcher allocated {best} times across a whole run \
         (measured 0 locally; budget {ALLOWANCE}, see module docs)"
    );
}

/// An inline `SmolStr` path — short paths never touch the heap, keeping
/// per-report path clones out of the allocation budget.
#[cfg(feature = "lang-rust")]
fn smol_path(name: &str) -> smol_str::SmolStr {
    smol_str::SmolStr::from(name)
}

/// The KP-8 F8 leg: the Merkle fingerprint pass (F8, §1.4.3 item 6) holds
/// the same discipline as the matcher — its only growing allocations are
/// the *result arrays* (per-node digest vector, span index, and the index
/// arrays of the walk). Those grow amortized-logarithmically, so a build
/// over the ~2.7k-node corpus performs tens of allocation events, all of
/// them capacity doublings; a per-node allocation (a boxed something per
/// visited node) would perform thousands. The bound encodes exactly that
/// separation — three orders of magnitude, not single digits.
#[test]
#[cfg(feature = "lang-rust")]
fn fingerprint_pass_allocates_nothing_per_node() {
    use janitor_prime::ast::fingerprint::{FingerprintTable, trivia_kind_ids};

    let registry = LangRegistry::new();
    let id: LangId = registry
        .for_extension("rs")
        .expect("this test binary builds with lang-rust");
    let trivia = trivia_kind_ids(&registry, id);
    let tree = parsed_corpus();

    ALLOC_EVENTS.store(0, Ordering::Relaxed);
    let table = FingerprintTable::build(&tree, &corpus_arc(), &trivia);
    let events = ALLOC_EVENTS.load(Ordering::Relaxed);

    assert!(
        table.participating() > 2_000,
        "corpus must be big enough to expose a per-node leak (got {})",
        table.participating()
    );
    assert!(
        events <= 128,
        "fingerprint pass allocated {events} times across a whole build \
         (measured ~60 locally — capacity doublings only; budget 128, see \
         the module docs)"
    );

    // The result arrays are the allowance's intended contents: rebuilding
    // the same tree allocates the same order of events, not n more.
    ALLOC_EVENTS.store(0, Ordering::Relaxed);
    let again = FingerprintTable::build(&tree, &corpus_arc(), &trivia);
    let rebuild = ALLOC_EVENTS.load(Ordering::Relaxed);
    assert_eq!(again.digests(), table.digests(), "deterministic rebuild");
    assert!(rebuild <= 128, "rebuild allocated {rebuild} times");
}

/// The corpus bytes as the shared snapshot the fingerprint pass indexes
/// (Rule T2: offsets are only meaningful against these bytes).
#[cfg(feature = "lang-rust")]
fn corpus_arc() -> std::sync::Arc<[u8]> {
    std::sync::Arc::from(corpus())
}

/// The counting allocator is genuinely installed in this binary (startup
/// itself allocates), under every feature set — the F3 gate's
/// `--no-default-features` leg keeps this file compiling meaningfully.
#[test]
fn counting_allocator_is_installed() {
    assert!(
        ALLOC_EVENTS.load(Ordering::Relaxed) > 0,
        "startup must have allocated through the fence"
    );
}
