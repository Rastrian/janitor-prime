# Eval 2 — Scope, Effort Re-estimation, and MVP Cut Review

- **Evaluator:** skeptical product engineering review (lane: EVAL-2)
- **Date:** 2026-09-13
- **Inputs:** `docs/planning/BRIEF.md` (authoritative spec); `docs/planning/PLAN.md` (incremental draft by the planning lane — read at 810 lines, then **re-read immediately before the verdict** at 819 lines after Appendices A.3/A.4 landed and the body was revised to match; the re-read produced finding §5.5.8, which the first read could not see). Section 3 below (independent effort anchors) was derived from the BRIEF alone, before reading the plan's own numbers, to avoid anchoring.
- **Unit of estimation:** focused working days of **one senior Rust engineer** (knows tree-sitter exists, has not shipped a tree-sitter product). Ranges are p50→p90. Calendar time ≈ working days × 1.3–1.5 for normal overhead.

---

## 1. Executive summary

Janitor Prime is three products fused into one binary: a transactional VFS (well-understood, low risk), a tree-sitter DSL refactoring engine (the real product, high risk, and where the schedule will actually be spent), and an OptMem-style persistent memory (medium risk, with one unbounded-scope ambiguity at its core: who writes the `nap()` summaries). My independent model puts the full BRIEF scope at **~42–70 focused working days** for a vertical slice + 2–3 languages, i.e. **2–3 calendar months solo** — not weeks.

The plan is fundamentally sound: its full-BRIEF estimate matches my independent model, its phase ordering follows every correct principle (kill-points first, vertical slice, apply-before-nap, AST-before-note), its kill-points have pre-designed fallbacks, and its MVP cut list makes the same cuts I would make independently. It is, however, **~15–20% optimistic inside v0.1**, concentrated in F4 (the matcher) and F6 (memory algorithms); it ships one unpriced content obligation (`GLOBAL.md`); it leaves a correctness spec gap (capture determinism) in its flagship ADR; **no phase tests the product's actual user** (nothing verifies that LLM agents can author DSL rules until after release); and the re-read surfaced one internal contradiction on the product's hottest path — §1.2.1 step 5 calls an `Arc::make_mut`-driven `Tree::edit` "cheap and safe" in exactly the case (readers holding clones) where the plan's own verified Appendix A.3.2 says a TSTree "must not be mutated while clones exist" (§5.5.8). Verdict and the required amendment list are in §5.6 and §VERDICT.

---

## 2. Review method and biases declared

- I estimated each module from the BRIEF text alone, listing hidden sub-scope the BRIEF implies but does not spell out (transactions, trivia semantics, pointer stability). Hidden sub-scope is where plans rot.
- I assume: no pre-existing code (repo is empty), no prior prototype, English docs, TDD per the BRIEF, `clippy -D warnings` and the no-`unwrap` rule enforced from F0. The no-panic rule is stricter than idiomatic Rust and carries a real, ongoing tax (~5–10% on every module that touches locks and C boundaries); I priced it in.
- I did **not** web-search: nothing in this review depends on external facts that aren't already constraints of the BRIEF or verifiable from the plan's own evidence trail. Crate version pinning is the PLAN's verification debt, assessed in §8.
- Known biases of this review, declared: (a) I am instructed to be skeptical — where the plan is honest and I have no counter-evidence, I say so rather than manufacturing disagreement; (b) the plan cites this eval throughout and adopted its calibration and risk list, so §5 deliberately hunts for what the plan got wrong *despite* the input — optimism in specific phases, kill-point gaps, and scope it invented that nobody priced.

---

## 3. Independent effort model (anchors from BRIEF, pre-plan)

### 3.1 Module 1 — VFS (`src/vfs`): **5–8 d**

Naive trait + `Os` + `Overlay` with the three invariants is 2–3 d. The hidden sub-scope:

- **Transactional semantics.** The BRIEF calls the overlay "transacional" and `apply` must "validar sintaxe … ANTES do flush". Multi-file diffs validated atomically imply commit/rollback boundaries, partial-failure semantics, and a defined flush ordering. Not spelled out anywhere; +2–3 d.
- **Path canonicalization edge cases** (symlinks, `./x` vs `x`, deleted-then-re-set files) and `stat` on virtual files (does a virtual file have a size/mtime? tree-sitter consumers will ask). +1 d.
- The four invariant tests the BRIEF mandates (anti-phantom, read-through, fallback, delete) are cheap and well-specified — this module is the best-specified part of the whole BRIEF. 1 d of that budget.

### 3.2 Module 2 — AST engine + DSL (`src/ast`): **20–35 d** — the schedule lives here

- **Tree-sitter fed from a mutating overlay (3–5 d POC; 8–12 d production).** Parse from `Overlay` bytes, incremental re-parse after edits, tree invalidation protocol, and the C-pointer lifetime design the BRIEF itself demands as deliverable #1. This is the named kill-point and it deserves every day of that range.
- **DSL front-end (3–5 d).** Whatever syntax the ADR picks, plus typed errors and good messages, because the DSL's users are LLM agents that will hallucinate syntax — error quality is product surface here, not polish.
- **Evaluator: `Under`/`Seq`/`Capture` (8–15 d).** A naive matcher is 3–5 d. The BRIEF's actual claims — `Under` as a state-machine pivot that "previne varreduras exponenciais", allocation-conscious traversal for "base de codigo multi-linguagem em grande escala" — are performance engineering with correctness teeth. Getting scoping right so the matcher is provably non-exponential on adversarial patterns is design work + benchmarks. 5–10 d beyond naive.
- **`Unify` isomorphism (4–7 d).** Naive recursive subtree equality is 2 d and nearly useless: it must be trivia-insensitive (whitespace/comments/parenthesization) or duplicate detection misses every real duplicate. That means span-skipping comparison and a decision about what "syntactic equivalence" ignores. Cheap to get wrong, moderate to get right.
- **Multi-language `NodeType` abstraction + grammars (1–2 d per language after the harness exists; 2–3 d for the abstraction itself).** Grammar node names are per-grammar strings; the cross-language story is a real ADR, not a type alias.

### 3.3 Module 3 — Memory (`src/memory`): **6–10 d** for the bounded version; one unbounded ambiguity

- LOG.txt append-only + `wake(budget)` with token budgeting: 2–3 d. Hidden sub-scope: "tokens" of which tokenizer? char/4 approximation vs a real tokenizer is an ADR, and the answer drifts per consuming model. Small but must be decided, not discovered.
- `note(pointer, content)` with `SyntaxPointer` anti-hallucination: 2–4 d. Validating a pointer against a live tree couples memory to the AST engine (ordering constraint!). Validating a commit hash drags in a git dependency.
- **`nap()` is the unbounded one.** A background routine that compresses the log into a binary tree of summaries — *written by whom?* OptMem delegates summarization to the LLM agent via its prompt; a static CLI has no LLM. The bounded designs are: (a) `nap` emits a summarization *protocol* the consuming agent fulfills on its next `wake` (2–3 d — my recommendation), or (b) a built-in heuristic compressor (research rabbit-hole, unbounded), or (c) call an LLM API from the tool (breaks the offline static-binary posture, adds key management). A plan that picks (b) or (c) without flagging the cost is rescoping, not planning.
- Global/Local layering (`$PWD/.janitor/`): 1 d.

### 3.4 Module 4 — CLI + orchestration (`src/cli`, `src/orchestrator`): **9–14 d**

- clap subcommands, serde JSON everywhere, stable error surface: 2–3 d. Versioned JSON envelope from day 1 (+0.5 d, saves an OSS-breaking-change later).
- `scan` with rayon over the tree: 2–3 d naive; correctness under concurrent overlay reads is part of the VFS cost, not double-counted here.
- **`apply --diff` with re-parse-before-flush: 4–6 d.** This is the integration heart — it welds VFS transactions, tree invalidation, and error-tolerant re-validation into one loop, and it is where the C-pointer design gets stress-tested. Cheap-looking in the BRIEF ("submete transformacoes ao buffer"), expensive in reality.

### 3.5 Cross-cutting: **2–3 d**

F0 scaffold, CI, clippy gates, no-`unwrap` lint discipline, docs skeleton. The no-panic rule's ongoing tax is priced per-module above, not here.

### 3.6 Total (BRIEF-as-written, vertical slice + 2–3 languages)

| Bucket | p50 | p90 |
|---|---|---|
| VFS | 5 | 8 |
| AST engine + DSL | 20 | 35 |
| Memory (bounded nap design) | 6 | 10 |
| CLI + orchestrator | 9 | 14 |
| Scaffold/CI | 2 | 3 |
| **Total** | **42** | **70** |

**≈ 2–3 calendar months solo.** Adding engineers does not compress the AST engine much — the matcher design is serial; parallelism helps around the edges (VFS/memory/CLI can overlap with the AST POC if, and only if, the interfaces are frozen early).

---

## 4. Ordering principles I judge plans against

1. **Kill-points precede everything they could invalidate.** The BRIEF's own example (incremental parse over a mutable overlay) invalidates the core architecture; it must run before any DSL/evaluator work. Same for the trivia/adjacency semantics of `Seq` (§6.2) — a one-afternoon experiment with 5 real-world patterns.
2. **The vertical slice comes breadth-first within the AST engine**: parse → match (`Under`/`Seq`/`Capture`) → report coordinates, in ONE language, before `Unify`, before multi-language, before LSP anything.
3. **`apply` lands before `nap`.** The refactor loop is the product; memory compression is downstream value.
4. **Memory's `note` depends on the AST engine** (SyntaxPointer validation needs a live tree). A plan that builds all of Module 3 before Module 2 has a hidden dependency inversion — only the LOG/`wake` half is independent.
5. **Nothing user-invisible is polished before something user-visible exists.** Perf campaigns, allocation heroics, and grammar breadth follow a working `scan`/`apply` loop, never precede it.
6. **The user is part of the loop.** If the product's user is an LLM agent, some cheap experiment must test the agent-facing surface (not just the engine) before it freezes.

---

## 5. Plan review (against PLAN.md; first read 810 lines, verdict-time re-read 819 lines)

> Circularity warning, declared up front: the plan cites this eval throughout and adopted its calibration, its risk list (my §6 → plan R1–R22), and its cut list. Agreement at this point is partially circular, so this section deliberately hunts for what the plan got wrong *despite* the input. Where the plan is simply right, I say so and move on.

### 5.1 Per-phase re-estimate (honest, no paper optimism)

The plan's §6 totals are arithmetically correct and its full-BRIEF range (43–70 d) matches my independent model (42–70 d). The distribution, however, is optimistic in exactly the two phases that carry the most novel code:

| Phase | Plan p50–p90 | My re-estimate | What the plan under-prices |
|---|---|---|---|
| F0 scaffold/CI | 2–3 | **2–4** | clap error render hook (JSON even for usage errors) is fiddlier than it looks; musl toolchain decision is a yak-shave magnet. |
| F1 kill-points | 3–5 | **3–6** | Corpus *authorship*: 3 MB file with known-good expectations (KP-1), 10 mangled files (KP-4), 5 curated patterns (KP-6) — spikes are cheap, their fixtures aren't. |
| F2 VFS | 4–6 | **5–7** | Flush ordering + partial-failure injection tests + proptest canonicalization; the txn layer is half the module and the BRIEF never mentions it. |
| F3 parse layer | 5–8 | **6–9** | "Zero heap allocations per visited node" (KP-8) is an iterative claim to make true, not a test you write once. Partially offset: A.3 landed favorable facts (`Tree: Send+Sync`, `Language::id_for_node_kind` exists) that simplify F3/F4 slightly. |
| F4 DSL+evaluator+scan | 8–12 | **10–16** | The matcher. ADR-0005's compiled-NFA design helps (and refactree's token-NFA is now a verified precedent, A.4), but capture semantics across multiple accepting paths need a defined priority (§5.5.1 — unspecified), the linearity bench needs its naive baseline authored, and DSL error-message quality is uncapped polish the plan itself flags. Biggest single gap. |
| F5 apply | 5–8 | **5–8** | Agree in shape — but the §5.5.8 soundness hole lives on this phase's hot path and must be resolved *before* F5, not discovered in it. |
| F6 memory | 5–8 | **6–10** | Three real algorithms hide here (block-tiling budget recall, content-hash re-anchor search, fixed-width TREE planner) plus **GLOBAL.md content authorship — unbudgeted anywhere** (§5.5.2). |
| F7 hardening | 3–5 | **3–6** | cargo-fuzz setup if kept; docs/SCHEMA.md + DSL.md done *well* is 1–2 d alone; plus the acceptance test I add in §5.6. |
| **v0.1 total** | **35–55** | **40–66** | ≈ +15–20%, concentrated in F4/F6. |
| Full BRIEF | 43–70 | **48–81** | |

Calendar: the plan says v0.1 ≈ 2.0–3.5 months solo; my re-estimate gives **≈ 2.5–4 months**. This is a good-faith estimate with moderate, locatable optimism — not fantasy, not honest-at-p50. The phases to watch in execution are F4 and F6; if either blows past p90, the cause will be in this table's right column.

### 5.2 Phase ordering — correct as written, with insertions required

F0 → F1(spikes) → F2(VFS) → F3(parse) → F4(scan) → F5(apply) → F6(memory) → F7(release) follows every principle in §4: kill-points precede what they could invalidate (KP-5 even runs in F0), the vertical slice is one language, `apply` precedes `nap`, `note` comes after the AST engine per the `memory → ast` dependency the plan itself diagrams. Dependency direction is acyclic and enforced. **No phase in F0–F7 needs to move.** What's missing are two kill-point insertions, one document move, one acceptance test, one v0.2 swap, and a set of spec/estimate corrections — listed in §5.6.

### 5.3 Kill-points — strong set, two genuine gaps

What's good: every KP names its hypothesis, experiment, cost, and a *pre-designed* fallback — the death-plan column is what makes these real kill-points rather than spikes-as-theater. KP-5 running in F0 (before the dependency set freezes) is exactly right, and A.3.8's ABI evidence (grammars 14–15 vs core range 13..=15) now makes its expected outcome "green". KP-2's `Tree: !Send` fallback being designed in advance removed the scariest tail risk — and A.3.1 has since resolved it favorably (`Tree: Send + Sync`), though the canary stays cheap insurance against minor-version churn (the plan's own new R22).

Gaps and critiques:

1. **Missing KP-9 — nobody tests the actual user.** The DSL's users are LLM agents; the product thesis is that agents can *author* structural rules from docs. No phase tests this before release. Cheap falsification: at F4 entry, hand the drafted `docs/DSL.md` to an off-the-shelf LLM, ask for 5 natural-language refactor specs, grade parseability + intent. If <4/5 succeed, the *surface syntax* needs iteration — which must happen before the evaluator freezes semantics in F4, not after F7 ships docs. ~0.5 d. The plan's R16 treats agent hallucination as an error-message problem; it might be a syntax-design problem, and only this experiment distinguishes them.
2. **Missing KP-10 — "cold parse per invocation is tolerable" is asserted, never measured.** ADR-0007 accepts full re-parse per invocation; R15 mitigates with "rayon". If a real 5k-file repo takes 90 s per `scan` and agents scan repeatedly, the v0.1 product thesis is damaged and the daemon moves *up* in priority — a plan-level consequence. One 0.5 d spike (parse a large real repo, 8 cores, report wall time) converts hope into a number. Belongs in F1.
3. **KP-4's death-plan is weak.** "Widen the gate to whole-file" would make `apply` unusable on real codebases — any pre-existing ERROR node anywhere (macro-heavy Rust, generated code) then blocks every edit. Better fallback: report localized *and* whole-file counts in the validation payload; whole-file stays a flag.
4. **KP-6's corpus is self-curated confirmation risk.** Five patterns authored by the same person who has an opinion about the answer. Source patterns from real refactoring commits, and include non-Rust files even in v0.1 — ADR-0010 decides semantics F9 will inherit.
5. **KP-8's fence re-arms too early.** The allocation-free guarantee is asserted in F3 against walk helpers, but the allocation-heavy code (the matcher) arrives in F4 — the CI fence must cover `exec.rs` or it guards the wrong thing.
6. **KP-1 measures one file size (3 MB) at 6 edit sizes.** Add a small-file leg: agents mostly make many small edits, where incremental bookkeeping can lose to a full re-parse; the "drop `Tree::edit`" decision should be per-size-class, not binary. More importantly, **KP-1 must add a readers-holding-clones leg** — see §5.5.8: the correctness question there is not "is incremental faster" but "is the edit path sound when the cached tree is shared", and KP-1 as scoped would not catch it.

### 5.4 Open questions — triage is right; two blockers hiding elsewhere; three now answered

Q1–Q7 as blockers: **agree with all seven as written.** Each has an owning experiment or a named stakeholder ask — this is what a correctly triaged blocker list looks like. Q6 (nap-as-synchronous-protocol deviating from the BRIEF's "background routine" wording) is the most important, and the plan treats it with exactly the right gravity: a deviation from the *authoritative spec* requiring written sign-off before F6.

Status after the verdict-time re-read (Appendix A.3 landed): **Q1, Q2 and Q5 are now answered by research** — Q1 expected-green (ABI 14–15 within core 0.27's 13..=15, A.3.8; keep the F0 pin-check as confirmation), Q2 yes (`Tree: Send + Sync`, A.3.1; keep the canary), Q5 bytes (`Point.column` in bytes, A.3.4). The live blockers reduce to **Q3** (incremental worth it — KP-1, now with the §5.5.8 correctness leg), **Q4** (trivia semantics — KP-6), **Q6** (nap sign-off — stakeholder), **Q7** (OptMem unlicensed ideas-only posture — stakeholder). That is a healthy blocker set for a plan at this stage: two technical, two asks-in-writing.

Additions to the blocker list:

- **Q8 (promoted from risk R15):** is cold-parse scan latency on real repos actually acceptable? Not answerable by opinion; owned by the KP-10 spike. Calibrates whether ADR-0007's daemon deferral survives contact with users.
- **Q9:** does v0.1 ship real content in `share/GLOBAL.md` (the BRIEF promises "generic AST lint heuristics" in the Global layer) or an explicitly minimal placeholder? Unbudgeted either way (§5.5.2). A 10-minute stakeholder ask, needed before F6.
- **Q10 (new, from §5.5.8):** is `ts_tree_copy`-then-`edit` contractually safe while the source tree has live readers, or must the edit path require exclusive ownership? The plan's A.3.2 and its §1.2.1 step 5 currently assert opposite answers. Resolvable in F1 (extend KP-1), blocks F5's design.

The plan's noise list is genuinely noise — nothing there can sink a phase. Agreed on every item.

### 5.5 New scope risks found in review (not in plan §8 R1–R22)

1. **Capture determinism across accepting paths — correctness spec gap in ADR-0005.** Failure memoization over `(child_index, pc)` is sound (reachability to `Match` doesn't depend on capture bindings), but when multiple accepting paths bind the same symbol differently, *which binding wins* is unspecified. Without a defined thread priority (leftmost-first, à la Pike VM), scan output can vary with rayon thread count and iteration order. For a product whose first hard rule is *deterministic* refactoring, this is a correctness bug class, not polish. The fix is cheap — specify the priority, add one adversarial test; unspecified is not.
2. **`GLOBAL.md` content authorship is unbudgeted scope.** The BRIEF's Global layer promises generic AST lint heuristics. The plan embeds the file via `include_str!` (F6.3) but no task authors the content, and no cut-list entry removes the promise. Empty file = broken promise shipped; real ruleset = 1–2+ days of content engineering plus tests. Price it or cut it explicitly (Q9).
3. **v0.1 grammar/executor skew on `unify`.** §1.4.1's grammar — and its own first example — includes `(unify …)`, but `Unify` executes in F8. Agents reading the docs *will* emit unify rules against v0.1. Stub it in F4 with a clean versioned error (`E_UNSUPPORTED_VERSION`) or omit it from the v0.1 grammar; neither is currently specified, and silent no-match is the worst outcome (the agent believes it searched).
4. **Report ordering stability.** Rayon-collected match reports need a defined canonical order (path, then span) or agent consumers see unstable diffs between identical runs — downstream logic built on diffing scans breaks nondeterministically. One sentence of spec + a sort; currently absent.
5. **dashmap × rayon deadlock footgun.** Both are in the dependency graph, and dashmap's documented rayon hazard (holding shard guards across a `par_iter` join) is precisely the shape of F3.4/F4.6 (concurrent cache inserts from workers). Needs a named rule ("collect, then insert", or entry-API only) plus a stress test; absent from R1–R22.
6. **`.janitor/STATE.json` concurrent writers.** `wake` reports linter status from cross-invocation state, and agents run tool calls concurrently on one repo. Read-modify-write without atomic tmp+rename corrupts or interleaves. Either atomic writes or drop the stateful field from v0.1 — small, real, unlisted.
7. **DSL docs freeze timing.** `docs/DSL.md` lands in F7, but the surface it documents freezes at F4. If agent-authorship feedback (KP-9) only arrives post-release, surface iteration after v0.1 means R12's schema-churn risk fires through the DSL's back door. The fix is sequencing (draft the docs at F4 entry), not effort.
8. **`Arc::make_mut` + `Tree::edit` aliasing hole on the apply hot path — found in the verdict-time re-read.** The plan's verified A.3.2 states `Tree::clone` = `ts_tree_copy` = a *shallow, refcount-incrementing alias*, and "a single TSTree must not be mutated while clones exist". Yet §1.2.1 step 5 (revised in the same update that landed A.3) says: *"Arc::make_mut(tree).edit(InputEdit) — make_mut shallow-copies if readers still hold the old tree: cheap and safe"*. Those two statements cannot both be true. When the `Arc<Tree>` refcount is >1, `make_mut`'s clone-on-write yields a new Rust handle to the *same underlying C tree*, and editing through it mutates shared state under live readers — precisely the forbidden operation. Depending on the exact C contract this is either silent corruption of concurrent readers or worse; and if the plan instead relies on the `ts_tree_copy`-then-edit idiom being sanctioned, A.3.2's own wording forbids it. Cheap, sound resolution: the incremental path requires `Arc::strong_count == 1` (exclusive — the common case in the stateless v0.1 CLI, where apply is its own invocation); any other count → full re-parse, no `Tree::edit`. Extend KP-1 with a shared-clone leg to pin the actual contract (Q10). Until then, "cheap and safe" must be deleted from the plan — it is the load-bearing "safe" in the product's safety story.

### 5.6 Required amendments (the reordering list)

1. **Insert KP-9 (LLM DSL-authorship gate) at F4 entry** — after the parser exists, before the evaluator freezes semantics; ~0.5 d.
2. **Insert KP-10 (real-repo scan-latency spike) into F1**; ~0.5 d; promotes R15 from accepted-wart to measured decision.
3. **Extend KP-1 with a readers-holding-clones leg and an exclusivity guard** on the `Tree::edit` fast path (§5.5.8); delete "cheap and safe" from §1.2.1 step 5; resolve Q10 in F1, before F5. ~0.25 d.
4. **Move `docs/DSL.md` drafting from F7 to F4** (it is the artifact KP-9 tests against; F7 keeps release polish only).
5. **Add an end-to-end agent-loop acceptance test to F7**: one scripted session through the real binary — `wake → scan → apply → note → wake` — asserting the JSON contract end to end. This, not the release checklist, is v0.1's definition of done; ~0.5–1 d.
6. **Swap F8 and F9 in v0.2** (python integration before `Unify`): the multi-language abstraction (ADR-0002) is an *architectural* promise whose first real test is cheap; `Unify` is feature depth. Validate architecture before adding features.
7. **Re-anchor KP-8's allocation fence onto the F4 matcher**, not just F3 walk helpers.
8. **Adopt the §5.1 re-estimate (v0.1: 40–66 d ≈ 2.5–4 months solo)**, or state explicitly which F4/F6 sub-scope is cut to defend 35–55 — the current p50s for those two phases assume zero iteration on capture semantics, recall tiling, and re-anchoring.
9. **Close the spec gaps in ADR-0005/§1.4.3 and the v0.1 grammar** (capture priority, canonical report ordering, `unify` stub or omission — §5.5.1/3/4) — text now, an afternoon total. Add the dashmap×rayon rule and STATE.json atomicity (§5.5.5/6) as named items, not folklore.

None of these rescopes the product. All of them are cheap relative to the cost of discovering the same facts in F4/F5/F6.

---

## 6. New scope risks (identified pre-plan, from the BRIEF itself)

> Status note: the planning lane folded this entire list into plan §8 as R1–R21 (plus a new R22 of its own on version churn), with mitigations I mostly endorse — e.g. `virtual_paths()` for the anti-phantom conflict, ADR-0012 span+hash pointers for dangling, the synchronous nap protocol. It is kept here as the record of what the BRIEF alone implied; the risks the plan *still* misses are in §5.5.

1. **`nap()` summarization ownership** (§3.3) — unbounded scope hiding behind one word; blocker-class open question, decidable in an afternoon. *(Plan resolved: ADR-0013, agent-owned, Q6 sign-off pending.)*
2. **`Seq` strict adjacency vs trivia** — sibling nodes in real code are separated by comments and formatting; byte-strict makes most real patterns unmatchable. *(Plan resolved: KP-6 + ADR-0010.)*
3. **`Unify` trivia sensitivity** — trivia-including isomorphism detects almost no real duplicates; needs an explicit equivalence definition. *(Plan resolved: ADR-0010 skeleton, corpus at F8.)*
4. **`SyntaxPointer` instability across edits** — node-id-based pointers dangle after any re-parse. Strongest pre-plan risk. *(Plan resolved: ADR-0012 span+content-hash+re-anchor.)*
5. **"Valida sintaxe resultante (re-parse)" is underspecified** — tree-sitter is error-tolerant; "valid" must mean "no ERROR/MISSING nodes in edited ranges". *(Plan resolved: §1.6 + KP-4; death-plan critiqued in §5.3.3.)*
6. **Anti-phantom invariant vs product need** — `read_dir` ignoring the buffer makes staged *new* files invisible to `scan`. *(Plan resolved: explicit `virtual_paths()` union.)*
7. **Static binary + git dependency** — commit-hash pointers vs libgit2/openssl in musl builds. *(Plan resolved: subprocess `git rev-parse`, ADR-0013.)*
8. **Cross-language "generic rules"** — per-grammar kind names make the BRIEF's "generic AST heuristics" a research-flavored promise. *(Plan constrained: minimal normalized vocabulary, v0.1; content still unbudgeted — §5.5.2.)*
9. **JSON schema churn vs LLM consumers** — the output format is the API for non-upgradable consumers. *(Plan resolved: versioned envelope from F0.)*
10. **Grammar license spread** — day-1 THIRD_PARTY manifest. *(Plan resolved: §10, cargo-about in CI; refactree verified MIT, A.4.)*
11. **Lock contention under rayon** — read guards held across parses would serialize `scan`. *(Plan resolved: `Arc<[u8]>` snapshots + guard discipline + F2.8 test.)*

---

## 7. Independent MVP cut list for v0.1 (my recommendation, pre-plan)

> The plan's §7 makes the same cuts — independently reached and then cross-checked against this list during the plan's writing. Differences found in review: the plan *includes* Commit-hash pointers in F6 (I had deferred them; the git-subprocess route is cheap enough that I withdraw the objection), and it leaves the `unify`-in-grammar question open (§5.5.3). Verdict on the plan's cut list: **correct and defensible** — see §8.

Principle: v0.1 must demonstrate the *unique* claim — deterministic structural refactor over a transactional overlay, consumable by an agent via JSON. Everything that serves humans (LSP), convenience (watch), or longevity (compression) waits.

**Cut from v0.1:**
- **LSP — cut entirely.** Biggest single correct cut. LSP serves IDE humans; the declared consumer is an LLM agent that consumes JSON. LSP adds server lifecycle, JSON-RPC framing, and a client compat matrix for a user the product doesn't have yet. (A.4's refactree confirms the shape: its LSP is a stdio server fed from its overlay — a fine F10 template, nothing v0.1 needs.)
- **`watch` mode — cut.** File-notify + debounce + incremental state is orthogonal to the value prop and multiplies test surface.
- **`nap()` fulfillment — cut from v0.1**, keep LOG + `wake` + `note` + the protocol/format design (ADR) so the log format doesn't need breaking later.
- **`Unify` — defer.** The scan→apply loop is complete without it; naive isomorphism is 2 d, but trivia-correct `Unify` is not v0.1-critical. (With the grammar-stub caveat of §5.5.3.)
- **Perf campaigns — cut** (keep allocation-conscious structures to avoid a rewrite; no benchmark-driven optimization before a user exists).
- **Grammar breadth — exactly ONE supported language in v0.1** (Rust: best grammar, dogfooding, in-repo corpus), with the multi-language `NodeType` ADR written as if for N. The first *second* language (v0.2) is the real test of that abstraction.

**Keep in v0.1:** full VFS with all four invariant tests; `Under`/`Seq`/`Capture` evaluator with defined trivia semantics; `scan` (rayon) + `apply` (re-parse validation, atomic flush); LOG + `wake` + `note`; versioned JSON envelope; CI + clippy + no-unwrap gates.

**Cut-to-zero test:** with only the "keep" list, can an agent still discover, stage, validate, and persist a refactor, and recall it next session? Yes — every cut survives it.

---

## 8. Findings against the plan as written

**What the plan gets right (and I will not manufacture disagreement with):**

1. **Estimates are honest at the full-BRIEF level** (43–70 d vs my 42–70 d) and the plan states its own optimism risks — rare and commendable. The v0.1-level optimism is real but moderate (§5.1).
2. **Phase ordering F0–F7 is correct.** Kill-points before architecture, KP-5 in F0, vertical slice in one language, apply-before-nap, AST-before-note (the dependency the plan diagrams itself).
3. **Kill-points have death-plans** — pre-designed fallbacks, including the `Tree: !Send` fallback that research has now rendered moot but was the right insurance when written.
4. **The MVP cut list is the right list** (§7 above): LSP, watch, web viewer, built-in summarizer, `Unify`, multi-language breadth, warm cache, tokio, gix — all correctly out; nothing load-bearing was cut. My two deltas: Commit pointers in F6 are acceptable (cheap via subprocess), and the `unify` grammar stub must be explicit (§5.5.3).
5. **Open-question triage is correctly done** — every blocker has an owner; the noise list is genuinely noise; and the re-read shows three blockers already retired by research (Q1/Q2/Q5, §5.4).
6. **The three BRIEF landmines with unbounded downside are all defused in writing**: nap ownership (ADR-0013 + Q6 sign-off), trivia semantics (KP-6 + ADR-0010), pointer stability (ADR-0012).
7. **The research appendices are load-bearing, not decorative**: A.3 turned ⏳ markers into verified facts (and the body was revised to match — I checked); A.4 mined refactree for two direct precedents (staged-overlay→validate→commit; token-NFA matching) that independently corroborate ADR-0005 and the apply loop's shape. This raises my confidence in F5's estimate specifically.

**What must change before execution** — the nine amendments in §5.6, summarized: two missing kill-points (agent-authorship, scan latency), the KP-1 aliasing leg + exclusivity guard (§5.5.8 — the one genuinely *dangerous* item, on the apply hot path), docs/DSL.md moved to F4, an agent-loop acceptance test as v0.1's definition of done, F8/F9 swapped in v0.2, the allocation fence re-anchored to the matcher, the F4/F6 estimates corrected (v0.1 → 40–66 d), and the afternoon of spec-gap closures in ADR-0005 (capture priority, canonical report order, `unify` stub, dashmap rule, STATE.json atomicity).

**Process observations, for the record:** (a) A.1's version table now carries an ABI evidence trail and the tree-sitter matrix is resolved by A.3.8; the remaining `[VERIFY]` markers are correctly execution-scoped (F0/F1) — this is what verification debt done right looks like; (b) the plan deviates from the BRIEF in exactly one place (nap "background" → synchronous) and flags it for written sign-off (Q6) — the correct way to deviate from an authoritative spec; (c) the body/appendix revision cycle had one miss — §1.2.1 step 5 was revised in the same edit that landed the fact contradicting it (§5.5.8) — which is precisely why the mandated verdict-time re-read exists, and why F1 spikes must re-verify load-bearing C-API contracts rather than trusting summaries of them.

---

## VERDICT

**aprovado-com-reordenacao** — approved conditional on the nine amendments of §5.6. The plan's architecture, cut list, and phase order are right; its blockers are real and owned; nothing requires rescoping the product. But it must not go to execution as-written, for one dangerous reason and eight cheap ones:

1. **[Blocking] Fix §1.2.1 step 5** — the `Arc::make_mut`/`Tree::edit` "cheap and safe" claim contradicts the plan's own verified A.3.2 (aliasing clone; "must not be mutated while clones exist"). Add the exclusivity guard (`Arc::strong_count == 1`) + the KP-1 shared-clone leg; resolve Q10 in F1, before F5 (§5.5.8).
2. **Insert KP-9** (LLM DSL-authorship gate) at F4 entry — the product's actual user is currently untested until after release (§5.3.1).
3. **Insert KP-10** (real-repo scan-latency spike) in F1 — converts ADR-0007's accepted wart into a measured decision (§5.3.2).
4. **Move `docs/DSL.md` drafting F7 → F4** (artifact for KP-9; prevents post-freeze surface churn, §5.5.7).
5. **Add the end-to-end agent-loop acceptance test** (`wake → scan → apply → note → wake` through the real binary) to F7 — v0.1's definition of done.
6. **Swap F8/F9 in v0.2** — validate the multi-language architecture (ADR-0002) with python before building `Unify` feature depth on a single-language foundation.
7. **Re-anchor KP-8's allocation fence onto the F4 matcher**, not the F3 walk helpers.
8. **Adopt the corrected estimates**: v0.1 = **40–66 focused days (~2.5–4 months solo)**, not 35–55 (~2.0–3.5); full BRIEF = 48–81. The delta is concentrated in F4 (10–16) and F6 (6–10) — or cut named sub-scope to defend the lower numbers (§5.1, §5.6.8).
9. **Close the ADR-0005/grammar spec gaps** in an afternoon of text: capture priority, canonical report order, `unify` stub-or-omit in v0.1, dashmap×rayon insertion rule, STATE.json atomic writes (§5.5.1/3/4/5/6).

With those nine landed, I would approve execution without further review. Estimate to plan against: **40–66 days to v0.1, not 35–55.**
