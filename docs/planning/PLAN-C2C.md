# Janitor Prime — C2C-informed memory plan (docs-only)

Produced 2026-09-18 on branch `docs/c2c-memory-plan`, one session after
v0.3.1 (84c428d). Sources read in order: PLAN-v0.3.md (whole), PLAN.md
§1.5 + ADR-0013/0017/0018, then the live code (`src/memory/{wake,nap,
tree_store,log,context,pointer,environment}.rs`, `src/cli.rs:64-190`) and
the live stores — including the author's real 439-fact migrated store.
Every claim about current code cites a file:line read this session; every
timing is a measurement run this session (provenance in §5). Effort units
are PLAN-v0.3 B11's agent-days: **S** ≤1, **M** 2–4, **L** 1–2 weeks.

## 1. Goal

The ICLR 2026 paper "Cache-to-Cache" (arXiv 2510.03215) shows LLMs
exchanging designed KV-cache instead of text gain +3.1–5.4% accuracy at
2.5× lower latency. Janitor will import **none of its machinery** — no
KV, no latents, no ML, no new deps. What transfers are four system
principles about *what a fixed-size context slot should carry and who
vouches for it*, and each maps onto a measured weakness of janitor's
own memory layer: recall that ignores the task, a quality metric that
counts bytes, read-time work that write-time already could have paid,
and undifferentiated trust across facts of wildly different origin.

| Paper finding | Principle | Janitor today (measured) | Item |
|---|---|---|---|
| Enriching only the right layers (top-K) beats enriching all; enriching the wrong ones *drops* accuracy | **P1 — selection > coverage** | `wake` recall is position-only: `tile()` picks by id-range age + `PRESENT_BIAS_ALPHA` (`src/memory/wake.rs:244-277`); no query input exists (`src/cli.rs:137-142` has only `--budget`) | **C1** `wake --task` ranked recall |
| Same cache size, semantically enriched: 58.42% → 62.34% (oracle experiment; measured via effective rank) | **P2 — density as the metric, not bytes** | Store quality gate is `len ≤ 256 && non-empty` (`src/memory/tree_store.rs:391-414`); the real store's TREE levels 32–256 are 14-byte placeholders (`"Fatos 0-32: "`) that passed the gate — budget spent on zero information | **C2** deterministic novelty score + nap density floor |
| 2.5× speedup from 90 ms fusion replacing 1312 ms decode — cost moved out of the hot read path | **P3 — pay at write-time, read cheap** | One `wake` at 5k facts opens TREE files **1,241×** (`TreeStore::summary` re-reads the whole level per lookup, `src/memory/tree_store.rs:243-257`); 977 ms at 5k, 2.9 s at 20k facts | **C3** wake I/O hygiene (and a measured *decline* of a disk index) |
| A bad sharer actively misleads the receiver — provenance is not neutral | **P4 — provenance as recall weight** | 432/439 facts in the real store are `[optmem #N]` migration imports, 131 truncated mid-thought with `…`; `FactRecord` has one flag, `architectural_lock` (`src/memory/log.rs:54-58`) — all facts recall at equal weight | **C4** agent-asserted confidence, down-ranked at wake |

The paper's numbers are motivation, not forecasts: janitor's payload is
text and its consumer varies, so no accuracy claim is made here — only
mechanism, with testable acceptance per item.
