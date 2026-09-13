# KP-2 — `Send`/`Sync` contracts (regression fence)

| | |
|---|---|
| **Kill-point** | PLAN §5 KP-2 · §1.2 Rule T3 · risk R2 |
| **Status** | **CONFIRMED at F1 (2026-09-13)** — desk-resolved (A.3.1), fence armed |
| **Code** | `src/ast/send_assert.rs` |
| **Commit** | F1 work item 1 |

## Hypothesis under attack

`Tree`/`Parser` (and the traversal handles `Node<'_>`, `TreeCursor<'_>`) are
`Send`/`Sync`, and the rayon pattern the whole engine design assumes —
workers receive **owned** `Arc<[u8]>` + `Arc<Tree>`, borrow the root locally,
walk with pooled cursors, and return plain data — is workable as-is.

## What was run

1. **Compile-time canaries** (`_canaries`): `assert_send`/`assert_sync`
   instantiated for `Tree`, `Parser`, `TreeCursor<'static>`, `Node<'static>`
   (both traits where A.3.1 verifies them). The file compiling **is** the
   experiment passing.
2. **Rayon mini-scan** (test): one parse of a 3-function corpus, then 8
   `par_iter` workers each cloning the `Arc<Tree>`/`Arc<[u8]>`, filtering
   `function_item` children by `kind_id`, and collecting
   `MatchReport { kind_id, start/end byte, start/end point }` — plain values,
   no borrows out of the worker.

## Result

- Canaries compile green on tree-sitter `=0.27.0` — all four types are
  `Send + Sync`, exactly as verified against the upstream source (A.3.1,
  `unsafe impl`s at `binding_rust/lib.rs` 4139–4153 of the 0.27.0 crate).
- Mini-scan: all 8 workers return byte-identical reports (3 hits each,
  offsets in bounds) — concurrent reads through shared handles behave as the
  blessed pattern (A.3.2).

## What the fence protects

If a future tree-sitter release narrows any of these impls, `send_assert.rs`
stops compiling (`R2`) instead of the product silently changing its
threading contracts. The pre-designed fallback (parse and match fused into
one work item; per-worker cache merged at collection) stays documented in
PLAN §1.2 Rule T3 and is **not** wired up — it is not needed on 0.27.
