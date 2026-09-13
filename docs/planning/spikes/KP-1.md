# KP-1 — Incremental re-parse vs. full re-parse (measurement)

| | |
|---|---|
| **Kill-point** | PLAN §5 KP-1 · §1.2.1 · risk R3 · Q1 |
| **Status** | **MEASURED at F1 (2026-09-13) — incremental WINS; keep the fast path** |
| **Code** | `src/spikes/incremental_reparse.rs` |
| **Data** | `benches/spike-kp1.json` |
| **Commit** | F1 work item 3 |

## Hypothesis under attack

Incremental `Tree::edit` + re-parse over an overlay snapshot beats a full
re-parse and is byte-correct — and the edit path is sound **only** under
exclusive ownership (`Arc::strong_count == 1` guard, §1.2.1 step 5).

## Setup

- Corpus: 3,146,176 B deterministic synthetic Rust (3,993 units of fns,
  struct+impl, match, if/else, doc/free comments), parses clean.
- Six edit sizes: tiny rename (+8 B), small statement (+32 B), medium fn
  (+331 B), large block (+1,094 B), xlarge block (+16,480 B), whole block
  (+131,158 B). Sizes 3–6 share one anchor so block size is the only
  variable.
- Path A: full re-parse of the new bytes. Path B: exclusively owned old
  tree → `Tree::edit` → `parse_with_options(old_tree)` — the §1.2.1
  steps 5–6 choreography (the untimed per-sample parse of the old bytes is
  the ParseCache stand-in; in production `T_old` comes from the cache).
- 20 samples per path per size after 3 warmups, separate parsers per path,
  **release** profile, p50/p90 reported.
- Machine: Intel Xeon E3-1270 @ 3.40 GHz, 8 cores, 32 GB RAM, Linux x86_64.

## Results (p50; full ≈ 506–579 ms across sizes)

| edit | delta | full | incremental | speedup |
|---|---|---|---|---|
| tiny-rename | +8 B | 569.4 ms | 85.4 ms | **6.66×** |
| small-statement | +32 B | 505.7 ms | 83.3 ms | **6.07×** |
| medium-fn | +331 B | 506.8 ms | 74.2 ms | **6.83×** |
| large-block | +1,094 B | 510.5 ms | 69.1 ms | **7.39×** |
| xlarge-block | +16,480 B | 511.2 ms | 85.0 ms | **6.02×** |
| whole-block | +131,158 B | 578.6 ms | 96.7 ms | **5.98×** |

Correctness: for every size, both paths agree on root `kind_id`, root
`byte_range`, and `has_error` (all clean), and — stronger, informational —
the **whole-tree S-expressions are identical** (`sexp_equal: true` across
the board): incremental node reuse landed on exactly the tree a fresh parse
builds. No size threshold needed: incremental wins even for a 128 KB
whole-block insertion (~6×), because tree-sitter reuses every subtree left
of the edit.

## Shared-clone leg (eval-2 amendment 1)

`edit_exclusive` refuses to edit while another `Arc<Tree>` handle is alive:
typed `SpikeError::SharedTree { count: 2 }`, `Arc::make_mut` never reached
(strong_count stays 2 — no hidden `ts_tree_copy`), zero observable mutation
on both handles; the identical edit succeeds once the reader is dropped.
Pinned as a fast CI test.

## Verdict

**KP-1 lives.** Keep `Tree::edit` + `parse_with_options(old_tree)` as the
apply fast path (F5.5 needs no size cutoff). The exclusivity guard is the
only sound way in — it is now spike-tested, and F5's counting-allocator
assertion re-arms it on the production path. Reproduce:
`cargo test --release --features spikes -- --ignored measurement`.
