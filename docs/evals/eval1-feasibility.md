# Eval 1 — Feasibility & Design Attack (Janitor Prime)

- **Reviewer role:** hostile senior Rust systems reviewer (unsafe, FFI, lifetimes).
- **Authoritative spec:** `docs/planning/BRIEF.md`.
- **Document under review:** `docs/planning/PLAN.md` — absent at review start, read in full (all sections §0–§10 + Appendix A, 78 KB, final read after the planning lane finished) before this verdict.
- **Status:** FINAL.
- **Scope of attack (per commission):** tree-sitter C pointer lifecycle in Rust (Tree/Cursor/Node; parse over mutable VFS Overlay bytes; invalidation after edits; Send/Sync), VFS invariants (anti-phantom), DSL evaluator (Under/Seq/Capture/Unify — complexity, backtracking), rayon parallelism over the tree, crate choices/pins (verified against crates.io / docs.rs).
- **Ground rules:** surviving risks below are NEW — the plan's own §8 register is not repeated; §7 shows the coverage map proving the de-duplication.

## Method

1. Read BRIEF.md (authoritative).
2. Verify tree-sitter Rust binding facts against docs.rs (lifetimes, Send/Sync, `parse_with_options` contract, incremental `Tree::edit`) — both 0.26.13 and 0.27.0, since the pin choice was open.
3. Verify crate versions on crates.io API (incl. `rust_version` fields, release dates).
4. Attack each focus area at the BRIEF level (§1–§6), then re-read the finished PLAN.md and keep only what survives (§7).
5. VERDICT.

## 0. Verified facts (researched 2026-09-13)

Checked against docs.rs / crates.io API / GitHub. These facts are offered to the plan's Appendix A.3, which is currently `⏳ pending research agent`.

### Crate versions (crates.io API, 2026-09-13)

| Crate | Latest stable | Released | Notes |
|---|---|---|---|
| `tree-sitter` | **0.27.0** | 2026-08-30 | previous line: 0.26.13 (2026-08-23); **MSRV Rust 1.90.0** (verified: `rust_version: "1.90"` on the version endpoint) |
| `tree-sitter-language` | 0.1.8 | 2026-08-30 | `LanguageFn` pattern; carries `links = "tree-sitter-language"` |
| `tree-sitter-rust` | 0.24.2 | — | ABI-14 era |
| `tree-sitter-python` | 0.25.0 | 2025-09-11 | |
| `tree-sitter-javascript` | 0.25.0 | 2025-09-01 | |
| `tree-sitter-typescript` | 0.23.2 | 2024-11-11 | bundles **two** grammars (typescript + tsx) in one crate |
| `tree-sitter-go` | 0.25.0 | — | |
| `tree-sitter-java` | 0.23.5 | 2024-12-21 | |
| `clap` | 4.6.6 | — | |
| `rayon` | 1.12.0 | — | |
| `serde` / `serde_json` | 1.0.229 / 1.0.151 | — | |
| `thiserror` | 2.0.20 | — | |
| `tokio` | 1.53.1 | — | |

The plan's Appendix A.1 table matches these exactly on every crate I checked independently — its version research is sound. The grammar ecosystem **lags the core** (0.23–0.25 vs 0.27.0), which the plan handles as its §8-R1/KP-5.

### API facts (docs.rs, 0.26.13 and 0.27.0 both checked)

- **`Tree` owns its C tree and does NOT borrow or retain the source bytes.** Nodes store byte offsets/points only; text is reconstructed by the caller via `Node::utf8_text<'a>(&self, source: &'a [u8]) -> Result<&'a str, Utf8Error>`. The Rust type system gives **zero protection** against "tree outlives the bytes it was parsed from" — the central unsafe-adjacent hazard of the design lives in *our* code, not tree-sitter's.
- **Send/Sync matrix (verified per version):** `Language`: Send+Sync (explicit unsafe impls). `Parser`: Send+Sync (non-wasm). `Tree`: **Send and Sync** (both versions). `Node<'tree>`: **Send+Sync in 0.26.13 and 0.27.0** (unconditional impls, both versions; historically contested in [issue #1241](https://github.com/tree-sitter/tree-sitter/issues/1241) — pre-0.26 blog posts claiming `Node: !Send` are stale). `TreeCursor`: not verified → `[VERIFY: docs.rs/tree-sitter/<pin>/tree_sitter/struct.TreeCursor.html]` before shipping one across threads.
- **Callback parse API** (`parse_with_options`, same shape in 0.26.13 and 0.27.0):
  ```rust
  pub fn parse_with_options<T: AsRef<[u8]>, F: FnMut(usize, Point) -> T>(
      &mut self, callback: &mut F, old_tree: Option<&Tree>,
      options: Option<ParseOptions<'_>>,
  ) -> Option<Tree>
  ```
  The callback receives `(byte_offset, point)` and **must return a chunk starting exactly at that requested byte offset**; an empty slice means EOF. Violation is unchecked in the Rust wrapper → **silently wrong trees**. The old HRTB signature `FnMut(usize, Option<Point>) -> &[u8]` from pre-0.26 articles no longer exists; sketches copied from old blog posts will not compile against either pin.
- **Incremental protocol:** `Tree::edit(&InputEdit)` must be applied to the *old* tree (bytes + points, old-end and new-end) *before* re-parsing with it; `changed_ranges` is only meaningful old-edited vs new. `Parser::parse` returns `None` without a language; after callback-induced failure, `Parser::reset()` is required for reuse.
- **`Node::id()`:** unique within a tree; stable across incremental re-parses **only for nodes actually reused** ("not marked changed" does not guarantee reuse). **No id→Node lookup API exists** — ids are for comparison, not resolution.
- **`Tree: Clone` duplicates the tree** (docs: cloning duplicates; C `ts_tree_copy`) — consistent with the plan's Rule T1 treating `Clone` as an O(tree) deep copy.
- **`Language` gating:** `set_language` fails with `LanguageError` on incompatible grammars; `Language::abi_version()` / `is_parseable()` enable a startup gate.
- **ABI landscape:** core 0.20–0.24 speaks ABI 13–14; core 0.25+ speaks ABI 15 ([emacs-devel 2025-04](https://lists.gnu.org/r/emacs-devel/2025-04/msg00508.html), [tree-sitter discussion #1768](https://github.com/tree-sitter/tree-sitter/discussions/1768)). Grammars 0.23.x (typescript, java) are ABI-14-era; loading them on 0.26/0.27 core must be proven by the plan's F0 pin-check — agreed, that is exactly KP-5.

## 1–6. BRIEF-level attack → plan absorption map

I attacked the BRIEF's design first (lifecycle, VFS, DSL, rayon, pins), then read the finished PLAN.md. The plan — visibly shaped by eval-2 and anticipating this review ("This is the section eval-1 will attack", §1.2) — already absorbs most of it. Per the no-repetition ground rule, each item below collapses to where the plan handles it; only survivors are argued in §7.

| # | My attack (summary) | Plan's absorption |
|---|---|---|
| R1 | Tree/bytes decoupling → stale-coordinate phantom matches; path-keyed parse cache next to mutable bytes is a bug generator | **Absorbed** — Rule T2 (`ParseEntry{gen, bytes: Arc<[u8]>, tree}` bytes+tree die together), T4 generation invalidation, T4a copied-value results |
| R2 | Torn parses via callback re-locking; or writer starvation under whole-parse read guards | **Absorbed** — §1.1 guard discipline (clone-an-Arc-only) + immutable-snapshot callbacks; §8-R14 + F2.8 concurrency test |
| R3 | Multi-edit offset composition; validate-then-rollback needs overlay transaction | **Absorbed** — ADR-0014 sorted/non-overlapping + `expect_sha`; §1.2.1 batch splice; `stage/commit/rollback` |
| R4 | `Seq(Vec<NodeId>)` unresolvable (no id→Node API); `'tree` lifetime infection | **Absorbed** — ADR-0004 pattern-element ids; Rule T3 plain-data `MatchReport` |
| R5 | `Arc<RwLock<Tree>>` caches + unbounded growth | **Partially absorbed** — ADR-0007 accepts cold cache/latency (§8-R15), but *memory* and the *zero-hit-rate* observation survive → **F1 (§7)** |
| R6 | BRIEF's delete test is unsatisfiable without tombstones (disk resurrects) | **Absorbed** — ADR-0009 `BufferEntry::Tombstone`; the plan found the same spec bug independently |
| R7 | Anti-phantom makes apply-created virtual files invisible to scan | **Absorbed** — `virtual_paths()` union in the scan walk (§1.3, §8-R8) |
| R8 | "Transacional" overlay had no transaction; flush partial failure unhandled | **Absorbed** — `stage/commit/rollback`, tmp+rename per file, `E_FLUSH_PARTIAL` |
| R9 | `dyn ProjectFs` needs `Send + Sync` supertraits | **Absorbed** — `pub trait ProjectFs: Send + Sync` (§1.3) |
| R10 | Binary files, UTF-8 validation, coordinate units | **Partially absorbed** — `SourceDecoding` error, KP-3 pins point units; binary/pathological-file *policy* survives → **F6 (§7)** |
| R11 | `Seq` adjacency undefined vs comments/anonymous nodes | **Absorbed** — ADR-0010 named-sibling + comment-skip default, pinned by KP-6 before the evaluator exists |
| R12 | `Under` bounds scope, not algorithmic complexity; needs index + memoization | **Absorbed** — ADR-0005 compiled NFA with `(child_index, pc)` failure memo; KP-7 linearity bench with naive baseline |
| R13 | `Unify` pairwise quadratic without hash-consing | **Absorbed** (design) — Merkle fingerprints O(n)/parse; but the *equivalence definition* has a hole → **F2 (§7)** |
| R14 | Cross-language `NodeType` silent-empty failure | **Absorbed** — per-language kind tables, `skipped_for_language` reporting; contradiction survives → **F4 (§7)** |
| R15 | Match-set semantics (nesting/overlap/order/determinism) undefined | **NOT absorbed** → **F3 (§7)** |
| R16 | Global map lock serializes rayon scan | **Absorbed** — snapshot-then-parse, phase-split; §8-R14 |
| R17 | Parser per-task lifecycle; no intra-tree parallelism; 5 MB files dominate tail | **Absorbed** (parser pool per worker; file-granularity); size caps survive inside F6 |
| R18 | rayon vs background `nap` thread starvation | **Dissolved** — nap is a synchronous protocol in v0.1 (ADR-0013) |
| R19 | Core/grammar skew must be spiked in F0 | **Absorbed** — KP-5/F0.6 pin-check + pre-designed fallback pin |
| R20 | `tree-sitter-language` `links` collision = hard cargo error | **NOT absorbed** (minor) → **F7 (§7)** |
| R21 | tokio unjustified | **Absorbed** — ADR-0008 |
| R22 | Grammars compile C/C++ via cc; CI toolchain; typescript = 2 grammars/crate | **Absorbed** — musl CI job (§8-R18); feature-per-language registry |
| — | SyntaxPointer with raw node ids dangles (memory module) | **Absorbed** — ADR-0012 span+content-hash+re-anchor |

## 7. Surviving findings (new, post-plan-read — the plan does not list these)

### F1 — `ParseCache` has zero hit rate in v0.1 and retains O(repo) memory
v0.1's `scan` takes a single `--rule`, runs in a cold process ("one process per CLI invocation", §1.0), and each file is parsed exactly once per invocation. Under that usage pattern the in-process dashmap cache **never serves a hit** — it is pure cost: every `ParseEntry` (an `Arc<[u8]>` **plus** an `Arc<Tree>`, and tree memory is a multiple of source size) is retained until process exit. A full-repo scan therefore holds O(repo) RAM with zero benefit, which turns §8-R15's "slow scan" wart into a potential OOM on the BRIEF's exact target ("base de codigo multi-linguagem em grande escala"). Either evict-as-you-go (streaming aggregation, entries dropped after the file's report is collected — the cache only pays off with multiple rules or warm processes, both post-v0.1), or list the memory bound as a §8 risk with the daemon (F10) as the fix. As written, the cache is complexity and RAM spent buying nothing.

### F2 — `Unify`'s Merkle equivalence drops anonymous operators: `a + b` ≡ `a - b`
ADR-0010/§1.4.3.6 defines the fingerprint as "named-node skeleton with leaf token texts, ignoring comment kinds and **all non-named (trivia) nodes**". In tree-sitter grammars, **binary operators and separators are anonymous tokens** (`binary_expression` = named children + an anonymous `+`/`-`/`*` field; `,` vs `;`, `.` vs `?.`). Excluding *comments* from isomorphism is right; excluding *all* anonymous nodes conflates semantically different code: `price + tax` and `price - tax` hash identically, as do `foo(a, b)` and `foo(a; b)`-shaped separator variants. The plan's own §8-R20 covers random 128-bit hash *collisions* (S2·L1) — this is a systematic semantic hole in the default equivalence (S4·L4 whenever F8's duplicate detection meets expressions). Fix the ADR now: exclude only comment/whitespace-class anonymous kinds per grammar (the KP-5 kind dump can classify them), and include operator/separator tokens in the hash; add an F8 test asserting `x + y` and `x - y` are NOT duplicates. `--strict-trivia` (byte-strict) masking the issue is not a fix — the default is the product.

### F3 — Match-set semantics undefined: nesting, overlap, dedup, order
The plan specifies the `MatchReport` shape and sorted/non-overlapping **edits**, but never the **match set**: can matches nest (an `Under` pivot inside another match's span)? Do overlapping matches from different pivots both report? Is the output canonically ordered? For an LLM consumer, non-deterministic or under-specified match sets are indistinguishable from bugs, and "deterministic refactoring" is the product's name. One ADR paragraph fixes it: e.g. all local maxima per pivot, overlaps allowed but flagged, canonical sort by `(start_byte, end_byte, rule_id)`, stable across rayon worker counts (a `par_iter().collect()` then sort — and a test asserting identical output under `RAYON_NUM_THREADS=1` vs `8`). While at it, pin the `--include-virtual` default (currently unspecified in §1.6).

### F4 — `UnknownKind` vs `skipped_for_language` is self-contradictory, and kind tables are corpus-biased
§1.4.2: unresolved kind ⇒ "the file is skipped and counted". F4.3: "`UnknownKind` typed error (no silent skips — reported per file)". Compile-time error or scan-time skip — the plan specifies both behaviors for the same condition. Pick one (recommendation: rule compile fails loudly only if the kind is unknown in **every** enabled language; otherwise per-file skip is counted). Separately: kind tables are "built lazily by walking one tree per language (first scan)" — whichever file is scanned first defines the known-kind universe; kinds absent from that file make valid rules skip later files (visible in the report, but still wrong). State the bias and the mitigation (seed the table from a bundled corpus fixture per grammar, e.g. the grammar's own test fixtures).

### F5 — `Arc::make_mut` in the apply hot path relies on an *asserted*, not *designed*, ownership invariant
§1.2.1 step 5: "`T_old.edit` … `Arc::make_mut` — refcount must be 1; if not, the entry is exclusively owned by the apply path **by construction**". The construction is not written down: the `ParseCache` dashmap holds a strong `Arc<Tree>` to the same entry unless apply **removes** the entry (takes ownership) before editing. If any other strong reference exists — the cache itself, a concurrent validation worker — `make_mut` silently deep-copies the tree at C level: correct output, but exactly the O(tree) hidden cost Rule T1/§8-R19 exists to ban, on the hottest path, wearing a different hat. One sentence in the plan fixes it: "apply `remove`s the entry from the cache and takes ownership before `Tree::edit`; the new entry is inserted after re-parse" (and an F5 test asserting no `ts_tree_copy` on the apply path, e.g. via the counting allocator).

### F6 — No binary/pathological-file policy in the scan walk
Nothing skips binary files, and nothing caps pathological inputs. tree-sitter will happily "parse" a 40 MB minified blob or a `.png` renamed `.rs` into a mountain of ERROR nodes — wasted parse, garbage matches, and (with F1's retention) the bytes + tree held in RAM for the process lifetime. Cheap policy, needs to exist in F4's walk: NUL-byte sniff on first 8 KB, size cap with a per-file skip report field (`"skipped": {"reason": "binary" | "size", …}`) so agents see what was excluded. Related residual from R10/R17: `set_string`'s UTF-8 validation must produce a typed error, and `utf8_text`'s `Err(Utf8Error)` path must be handled in the DSL's text-touching ops (Unify leaf hashing included).

### F7 — Minor: `tree-sitter-language` `links` collision; MSRV metadata
(a) `tree-sitter-language` declares `links = "tree-sitter-language"`: any future dependency dragging a semver-incompatible second version of it into the graph is a hard cargo build error, not a warning. Today all grammar crates ride `^0.1.x` and unify; the landmine is for future grammar forks/vendorings — one line in ADR-0015.
(b) Grammar crates compile C (some C++) via `cc` — covered by the musl CI job, but the cross-compilation prerequisites should be listed in the F0 CI notes.

### F8 — Factual corrections the plan should fold into Appendix A.3 (it is `⏳`; these are verified)
1. **`Node<'tree>` IS Send+Sync** in 0.26.13 and 0.27.0 (unconditional impls, docs.rs both versions). §1.2 Rule T3 asserts "not `Send` (⏳ verify)" — stale; the conservative rule ("nodes never cross threads") can stand, but the recorded fact is wrong and KP-2's fallback is moot on current pins.
2. **`Tree` IS Sync** (both versions) — the commented-out `assert_sync::<Tree>()` canary can be enabled.
3. `parse_with_options` exists with the `T: AsRef<[u8]>` callback shape in **both** 0.26.13 and 0.27.0 (§1.2.1's sketches compile against either); the chunk-offset contract (silently wrong trees on violation) is as §0 states.
4. `Tree: Clone` = duplication (deep copy semantics confirmed by docs) — Rule T1's premise verified.
5. **MSRV conflict — the plan's `rust-version = "1.85"` is wrong**: tree-sitter 0.27.0 declares `rust_version = "1.90"` (crates.io version endpoint, verified). clap 4.6.6's 1.85 floor is not the binding constraint. A declared 1.85 with a 1.90 dependency lies to `cargo msrv`/CI matrixes; either bump to 1.90 or (cleaner, given 0.27.0 is ~2 weeks old and its own tracker admits rough edges, #5013) pin `tree-sitter = "=0.26.13"` per KP-5's fallback and re-derive the declared MSRV from the actual dependency set.

## 8. Assessment

The plan is unusually defensible for this genre. The load-bearing choices are right where it matters most for a hostile unsafe/FFI review: **no `unsafe` of our own** (Rule T6) with the C lifecycle discipline encoded as testable rules (T1–T5); **bytes and tree die together in one versioned entry** (T2/T4) instead of trusting offsets across a mutable buffer; **snapshot-then-parse** instead of parsing under live locks; **plain serializable results crossing thread boundaries**; generation-based invalidation instead of mtime heuristics; a compiled NFA with failure memoization behind the BRIEF's anti-backtracking claim (with a falsifiability bench, not a promise); tombstones fixing a genuine spec bug in the BRIEF's delete semantics; and kill-points ordered *before* the phases they can invalidate. The effort estimate cross-checks against an independent model, and the MVP cut has a real cut-to-zero test.

What survives attack (§7) is real but tractable: one equivalence-definition bug that would corrupt F8's flagship feature (F2), one memory-behavior hole in the only hot path v0.1 has (F1), one hot-path ownership invariant asserted rather than designed (F5), one output-contract gap (F3), two spec ambiguities (F4, F6), one wrong metadata claim with a build-infra consequence (F8.5), and minor landmines (F7). None of them invalidates the architecture; all of them are cheap to fix *in the document now* and expensive to discover in F3–F8.

## VERDICT

**GO-with-conditions.** The architecture is sound and the plan is honest about its own falsifiability. Conditions, all resolvable by editing PLAN.md before F0:

1. **(F8.5, blocking for the frozen Cargo.toml)** Reconcile `rust-version` with the dependency set: either declare 1.90+ with `tree-sitter = "=0.27.0"`, or execute the KP-5 fallback now (pin `=0.26.13`, re-derive MSRV) — 1.85 + 0.27.0 is provably inconsistent.
2. **(F2)** Amend ADR-0010: `Unify`/Merkle equivalence must hash semantically-significant anonymous tokens (operators, separators) and exclude only comment/whitespace-class kinds; add the `x + y` ≠ `x - y` test to F8's acceptance.
3. **(F1)** Decide and document ParseCache retention for v0.1: evict-as-you-go (or drop the cache entirely until multi-rule/warm processes exist), or add an explicit O(repo)-RAM risk row to §8 with the F10 daemon as the named fix.
4. **(F5)** Replace "exclusively owned by construction" with the constructed rule: apply *removes* the `ParseEntry` from the cache and takes ownership before `Tree::edit`/`Arc::make_mut`; add the no-`ts_tree_copy`-on-apply-path assertion to F5's tests.
5. **(F3)** Define match-set semantics (nesting, overlap, dedup, canonical order, rayon-determinism test) in an ADR and `docs/SCHEMA.md`; pin the `--include-virtual` default.
6. **(F4)** Resolve `UnknownKind` (compile-time error) vs `skipped_for_language` (per-file skip) into one documented behavior; note the kind-table corpus bias and seed tables from per-grammar fixture corpora.
7. **(F6)** Add the binary-sniff/size-cap skip policy to F4's walk with a skip-report field; state UTF-8 validation behavior for `set_string` and the `Utf8Error` path in text-touching DSL ops.
8. **(F8.1–F8.4)** Populate Appendix A.3 with the verified facts in §0 here — including the correction that `Node` **is** Send+Sync and `Tree` is Sync on both pin lines — and enable the `Sync` canary.
9. **(F7)** One line in ADR-0015 recording the `tree-sitter-language` `links`-collision constraint for future grammar additions.

With those nine edits absorbed, I have no remaining objection to proceeding to F0. The sharpest compliment I can pay the plan as a hostile reviewer: most of what I prepared against the BRIEF was already dead on arrival in §1–§2, and what survived is specification debt, not architecture debt.

---

*Sources consulted (2026-09-13): [docs.rs tree-sitter Parser/Tree/Node, versions 0.26.13 and 0.27.0](https://docs.rs/tree-sitter/latest/tree_sitter/struct.Parser.html); [crates.io API: tree-sitter, tree-sitter 0.27.0, tree-sitter-language, tree-sitter-{rust,python,javascript,typescript,go,java}](https://crates.io/api/v1/crates/tree-sitter); [tree-sitter issue #1241 (Node Send/Sync)](https://github.com/tree-sitter/tree-sitter/issues/1241); [tree-sitter issue #359 (thread safety)](https://github.com/tree-sitter/tree-sitter/issues/359); [tree-sitter discussion #1768 (grammar versioning/ABI)](https://github.com/tree-sitter/tree-sitter/discussions/1768); [emacs-devel 2025-04 (ABI 13–15 mapping)](https://lists.gnu.org/r/emacs-devel/2025-04/msg00508.html); [tree-sitter releases (#5013 note on 0.26.0)](https://github.com/tree-sitter/tree-sitter/releases).*
