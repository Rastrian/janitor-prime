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

## 2. Proposed items

### C1. `wake --task "<query>"` — ranked recall under budget *(code)*
- **Problem (P1):** `wake` decides what a session sees from position and
  byte cost alone. A task about "daemon socket timeouts" and a task about
  "DSL trivia semantics" receive byte-identical payloads. The paper's
  sharpest result is that *misdirected* enrichment actively hurts: filling
  the slot with mediocre-relevance material is worse than leaving budget
  unused.
- **Evidence:** `wake()` is a pure function of `(facts, store, budget)`
  (`src/memory/wake.rs:128-224`); `tile()` selects by id-range age and
  `PRESENT_BIAS_ALPHA = 8` (`wake.rs:55`, `wake.rs:244-277`); `WakeArgs`
  offers only `--budget` (`src/cli.rs:137-142`). On the real 439-fact
  store, the default-budget payload is 214 summaries + **11 raw facts
  (2.5%)** — the tiler's choice, blind to content. No text analysis
  exists anywhere in the recall path.
- **Proposal:** an optional `--task` flag switching `wake` to a ranked
  pass, all deterministic std-only statistics (same family as B1's
  byte-level predicates — no regex, no deps):
  1. **Score** every fact by store-local TF-IDF: tokens = lowercase
     alphanumeric runs ≥ 2 chars; `score(q, f) = Σ_{t∈q} idf(t) · tf(t, f)`
     with `idf(t) = ln(1 + N/df(t))` over the store's own facts;
     `"scorer": "tfidf-store"` declares it in-band (the `estimator`
     precedent, ADR-0011).
  2. **Serve** top-scoring facts raw, score desc, ties → newer id first
     (present-bias preserved), while the budget allows. Locked facts stay
     pinned: always raw, never dropped — the §1.5.2 invariant survives
     ranking.
  3. **Fill** leftover budget with stored summaries over *unserved*
     ranges via the existing tiling machinery (gate unchanged:
     `E_NAP_PENDING` for summaries the layout needs).
  4. **Report** append-only keys in the wake envelope (janitor.v1,
     SCHEMA.md §`wake`): `"task": {"query": "...", "scorer":
     "tfidf-store", "served_scored": n, "dropped_scored": m}`;
     `truncated: true` gains the meaning "score-positive facts were
     dropped for budget". Without `--task`, the payload is byte-identical
     to today (golden regression).
- **Sizing:** M (2–4 agent-days: scorer + ranked pass + SCHEMA/DSL docs
  + golden/property tests).
- **Acceptance (testable):** fixture of 40 facts where 10 contain the
  marker cluster "daemon socket" in the *oldest* half; `wake --task
  "daemon socket errors" --budget 1500` serves ≥ 9 of the 10 raw, while
  budget-matched no-task wake serves ≤ 3 (it buys summaries of old
  blocks); two runs byte-identical; every lock present exactly once,
  raw; no-`--task` payload unchanged vs v0.3.1 golden.
- **KP (if C1 grows to L):** on the real 439-fact store, 10 hand-labeled
  probe queries (relevant fact ids chosen by the labeling agent, not the
  scorer): ranked wake serves more labeled facts than position wake at
  equal budget on ≥ 7 of 10 — else the ranked pass is not earning its
  contract surface and C1 is killed (the no-task path remains).

### C2. Deterministic novelty score + nap density floor *(code)*
- **Problem (P2):** the system's only quality metric is bytes. `nap`
  accepts any non-empty one-liner, so a fatigued agent (the real one,
  2026-09-15) settled every level-32+ slot with the placeholder
  `"Fatos 0-32: "` — 14 bytes, zero information. `wake` then served 12
  such summaries at budget 400: `awake: true`, estimate 384/400, budget
  fully spent on nothing. The paper's oracle experiment is the same
  lesson inverted: equal bytes, unequal semantic payload — and they
  *measure* the difference instead of assuming it.
- **Evidence:** `validate_summary_text` checks empty/newline/NUL/≤256
  (`src/memory/tree_store.rs:391-414`) — the whole quality gate. Real
  store TREE/32–256: all placeholder records (13–15 bytes each, measured
  this session); levels 2–16 are mechanical concatenations cut mid-word
  at the byte ceiling. `note` has no redundancy signal either
  (`src/memory/log.rs:110-138`).
- **Proposal:** a windowed novelty statistic, computed from LOG at
  command time (nothing persisted; LOG stays append-only):
  - `novelty(fact_i) = 1 − max_{j ∈ window64(i)} Jaccard(W(i), W(j))`
    where `W` is the fact's set of lowercase word tokens and
    `window64(i)` the 64 preceding facts — deterministic, linear, no
    deps. (Full pairwise is O(n²) — declined, §3 anti-backlog.)
  - **Write-time surfacing (P3-shaped):** `note` report gains
    `"density": {"novelty": 0.83, "window": 64}` — the agent sees
    redundancy *when it can still act on it*; `nap` requests list each
    source fact's novelty so the summarizer knows which facts carry new
    information.
  - **Floor, not judge:** `nap --text` refuses the degenerate case with
    a typed `E_NAP_LOW_DENSITY` (summary ≤ 3 distinct words while its
    block covers ≥ 8 facts — the placeholder signature), and *warns*
    (payload warning, accepted) below a 0.25 distinct-word ratio vs the
    source. The binary never scores *meaning*; it floors *emptiness*.
- **Sizing:** M (2–3 agent-days: statistic + two payload surfaces + one
  typed error + tests/docs).
- **Acceptance (testable):** (1) submitting `"Fatos 0-32: "` over a
  32-fact fixture → `E_NAP_LOW_DENSITY` with `details.distinct_words ≤
  3`; (2) a genuine ≥ 10-distinct-word summary always passes (pinned —
  the agent loop must converge by construction); (3) 10 near-duplicate
  notes then 1 novel one → duplicates score < 0.2, the novel one > 0.9,
  identical across runs; (4) `note` reports `density.novelty ∈ [0,1]`.
- **Risk flagged here, argued in §4:** boilerplate (`[optmem #N]`
  prefixes, dates) inflates overlap — mitigation is a deterministic
  bracket-prefix strip before tokenization, not regex.

### C3. Wake read-path hygiene — each TREE level loaded once *(code)*
- **Problem (P3):** the paper's 2.5× comes from paying cost at
  write-time so the read path is cheap. Janitor's nap half already does
  this (summaries are precomputed — ADR-0013); the wake half re-pays on
  every session. `TreeStore::summary()` calls `self.load(size)` — a
  full-file read + decode — **per lookup** (`src/memory/tree_store.rs:
  243-257` → `load` at `202-239`), and the tiler looks up per block
  (`wake.rs:271`, `290`, `345`, `390`). `BlockPlan::rank` likewise
  rescans the level's slots per call (`tree_store.rs:161-171`).
- **Evidence (measured this session, settled synthetic stores):** one
  `wake --budget 8192` at 5,000 facts opens TREE files **1,241×**
  (strace: 620× `TREE/8`, 312× `TREE/16`, 156× `TREE/32`, …) — the
  160 KB level-8 file is re-read ~620 times, ≈ 99 MB of redundant reads.
  Wall time: 439 facts → 139 ms; 5k → 977 ms; 20k → 2.89 s. Startup is
  4 ms; the cost is pure wake.
- **Proposal:** batch-load. `wake()` reads each existing level file
  exactly once into a `HashMap<level, Vec<String>>` (a `load_levels`
  batch on `TreeStore`), precomputes ranks per level in the same pass,
  and every `summary()`/`rank()` hit resolves in memory. Output
  semantics unchanged — this is pure I/O shape. No new files, no disk
  cache, no contract growth beyond internals.
- **Sizing:** S (≤1 agent-day).
- **Acceptance (testable):** (1) payload byte-identical pre/post on the
  property-suite corpora (the existing wake tests are the fence);
  (2) structural: a unit test over `load_levels` asserts one wake loads
  each present level exactly once (count == distinct levels in the
  layout, ≤ ⌈log₂ n⌉); (3) wall-time targets at 20k facts: < 700 ms
  (from 2.89 s), measured on the same synthetic generator this session
  used (kept as a `tests/` fixture generator, not a bench).
- **Explicitly declined inside P3:** the write-time *materialized index*
  (term frequencies / retrieval keys persisted at `note` time) — after
  C3 the read path is one O(n) LOG parse + ≤ log₂ n file reads, and C1's
  scoring is O(total bytes); a persisted index would add an
  invalidation surface (every append) to save < 100 ms. ADR-0018's
  disk-parse-cache rejection is the standing precedent (§3 anti-backlog).

### C4. `note --confidence low` — agent-asserted provenance, down-ranked at wake *(code)*
- **Problem (P4):** in the paper, a bad sharer *misleads* the receiver —
  provenance is not neutral, it is load-bearing weight. Janitor's real
  store is 432/439 `[optmem #N]` migration imports (131 truncated
  mid-thought with `…`), and every one recalls at the same weight as a
  hand-verified, pointer-anchored fact. `wake` cannot express "trust
  this less".
- **Evidence:** `MemoryFlag` has exactly one variant, `ArchitecturalLock`
  (`src/memory/log.rs:54-58`); `FactRecord` carries no other metadata
  (`log.rs:61-76`); `NoteArgs` offers `--lock` only (`src/cli.rs:153-177`).
- **Proposal:** an optional agent-supplied field, never binary-invented
  (the B10 anti-hallucination stance — the *agent* vouches or demotes):
  - Storage: `note --confidence low` appends `"confidence": "low"` as a
    new **optional field** on the fact record (`#[serde(default)]`),
    *not* a new `MemoryFlag` variant — serde ignores unknown fields by
    default, so a v0.3.1 binary reading a tagged log still parses every
    line, whereas an unknown *enum variant* in `flags` would read as
    corruption and hit `LogUnrecoverable` (`log.rs:210-213`). This
    asymmetry is why the field is the right shape.
  - Wake: in C1's ranked pass, ties break to higher confidence; under
    budget pressure, `low` facts are dropped before normal ones (after
    score ordering). In the no-task path, `low` facts are demoted one
    split level earlier — same tiling, cheaper seat. Never deleted;
    append-only stands.
  - Metadata-only: no semantic judgment, no expiry, no auto-detection
    of migration prefixes (that would be the binary inventing facts).
- **Sizing:** S (≤1 agent-day; the wake half rides C1's ordering code).
- **Acceptance (testable):** (1) `note --confidence low` round-trips
  through LOG and the report; (2) a pre-field v0.3.1-format LOG parses
  identically with and without the C4 change (golden, synthetic fixture);
  (3) tie-break: two equal-score facts under a budget that fits one →
  the normal-confidence fact survives, `truncated: true` reports the
  drop; (4) `low` facts still serve when budget allows — down-weight,
  not filter (property test).
- **B10 collision:** B10 (session-zero bootstrap ritual, unexecuted)
  stays B10 — but its recipe grows one step if C4 lands: tag migration
  imports with `--confidence low` while seeding. C4 without B10 is still
  useful (any session can tag going forward); B10 without C4 cannot
  express the tag. No part of B9 is touched by any C-item.
