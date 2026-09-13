# Janitor Prime — Implementation Plan

| | |
|---|---|
| **Status** | Complete draft v2 — all research lanes merged (plan-only lane) |
| **Authoritative spec** | `docs/planning/BRIEF.md` |
| **Companion reviews** | `docs/evals/eval1-feasibility.md` (hostile Rust review), `docs/evals/eval2-scope.md` (effort re-estimation), `docs/evals/eval3-strategy.md` (build vs. leverage) |
| **Language** | English (future OSS repo) |
| **Mode** | PLAN ONLY — no implementation code in this document beyond signature/struct sketches; no cargo runs; no commits. |

All external facts (crate versions, tree-sitter API, OptMem, refactree) were verified against primary sources on 2026-09-13 — Appendix A carries the evidence trail. Remaining `[VERIFY]` markers are items that can only be settled by *execution* (F0/F1), not research.

**Section status** — §0 ✅ · §1 ✅ · §2 ✅ · §3 ✅ · §4 ✅ · §5 ✅ · §6 ✅ · §7 ✅ · §8 ✅ · §9 ✅ · §10 ✅ · Appendix A ✅ (A.5 filled at F0 execution, 2026-09-13)

---

## 0. Executive summary

Janitor Prime is a single statically-linked Rust binary (`janitor`) that gives autonomous LLM agents three fused capabilities over a target codebase, with 100% JSON-structured I/O:

1. **`src/vfs`** — a transactional virtual filesystem: an `Overlay` over `Os` that simulates edits in memory, with a strict anti-phantom invariant (`read_dir` never leaks virtual files) and commit/rollback boundaries around multi-file changes.
2. **`src/ast`** — a structural search/rewrite engine over tree-sitter, driven by a small s-expression DSL whose value is **explicit scope semantics with a proven linearity bound** (KP-7): the internal algebra (`Under` / `Seq` / `Capture` / `Unify`) compiles to a scoped state machine — the point is the `Under` scope restriction and strict adjacency, not backtracking rescue (tree-sitter matching is already compiled and linear; eval-3 C6). All parsing reads bytes **from the Overlay**, never directly from disk.
3. **`src/memory`** — an OptMem-inspired persistent memory: append-only log, `wake(budget)` with OptMem's block-tiling recall (newest raw, oldest coarsened), `note` with anti-hallucination `SyntaxPointer`s (span + content-hash, re-anchorable), and a `nap` compression protocol whose summaries are written by the consuming agent — the tool only sequences and stores them, exactly like OptMem. No LLM call ever happens inside the binary.

Four hard product rules from the BRIEF shape everything: zero-cost concurrency (`Arc`/`RwLock`/rayon), typed errors only (`thiserror`, no `unwrap`/`panic` in production code — enforced by clippy gates), 100% JSON output with a versioned envelope, and allocation-conscious tree traversal (kind-id integer matching, `Arc<[u8]>` snapshots, pooled cursors — all three mechanisms verified against the current tree-sitter API, Appendix A.3).

**Strategy in one paragraph.** Kill-point experiments come first (F1: incremental parse over a mutable overlay; `Send`/`Sync` canaries; `Seq` trivia semantics), because any of them can invalidate the core architecture — three of the (now eleven) kill-points were already resolved by desk research (Appendix A.3) and survive only as regression fences. Then the vertical slice — VFS → parse → match → `scan` → `apply` — in exactly one language (Rust), then memory accumulation (`LOG`/`wake`/`note`/`nap`-protocol), and only then the differentiating-but-deferrable pieces (`Unify`, multi-language breadth). v0.1 is the agent loop: discover → stage → validate → flush → remember. LSP, watch mode, web viewer, and built-in summarization are explicitly out (§7). Estimated effort for v0.1: **~40–66 focused engineer-days (p50–p90)**, i.e. **~2.5–4 calendar months solo** (§6 — eval-2's re-estimate adopted after reconciliation; full BRIEF 48–81 days incl. multi-language).

---

## 1. Architectural design

### 1.0 System overview

```
                       ┌─────────────────────────────────────────────────┐
                       │                 janitor (bin)                   │
                       │  clap  →  orchestrator::Session  →  output      │
                       │         (versioned JSON envelope)               │
                       └────────┬───────────────┬──────────────┬─────────┘
                                │               │              │
                    ┌───────────▼─────┐ ┌───────▼────────┐ ┌────▼─────────────┐
                    │  src/vfs        │ │  src/ast       │ │  src/memory      │
                    │  ProjectFs      │ │  dsl (s-expr)  │ │  LOG.txt (JSONL) │
                    │  Os / Overlay   │ │  compile→exec  │ │  TREE/ (fixed)   │
                    │  txn/rollback   │ │  tree-sitter   │ │  ContextManager  │
                    └───────┬─────────┘ └───────┬────────┘ └────┬─────────────┘
                            │  Arc<[u8]> snapshots │  ParseEntry │
                    ┌───────▼────────────────────▼───────────────▼────────────┐
                    │  rayon worker pool (scan/apply; per-file owned snapshots)│
                    └──────────────────────────────────────────────────────────┘
```

One binary, one process per CLI invocation (stateless across invocations in v0.1; parse cache is in-process — see ADR-0007). The `orchestrator::Session` is the composition root that owns the `Overlay`, the `ParseCache`, the language registry, and the `ContextManager`, and is the *only* place where the three domains meet.

Module dependency direction (acyclic, enforced):

```
cli → orchestrator → { vfs, ast, memory }
                     ast → vfs        (parser reads Overlay bytes)
                     memory → ast     (SyntaxPointer validation needs a live Tree)
```

Note the `memory → ast` dependency: `note`'s anti-hallucination validation requires parsing the referenced file first. This forces the build order in §3 (AST engine before `note`), as flagged in eval-2 §4.4.

### 1.1 Concurrency & ownership model (global rules)

- **Sharing:** `Arc` everywhere ownership crosses module or thread boundaries. The `Overlay` is `Arc<Overlay>` shared by the session and every rayon worker.
- **Locking:** one `RwLock<HashMap<PathBuf, BufferEntry>>` inside `Overlay`. **Guard discipline:** read guards are held only to look up and `clone` an `Arc` — never across a parse or a traversal. This is what makes the `Arc<[u8]>` buffer entries worth their indirection: snapshotting is O(1) and contention-free, so `scan` with N rayon threads never serializes on the lock (eval-2 §6.11).
- **Parallelism:** rayon for `scan` (embarrassingly parallel over files) and `apply` validation (parallel over edited files). Work items carry **owned** data (`Arc<[u8]>` snapshot, `Arc<Tree>`), never borrowed `Node`s — see §1.2.
- **No async runtime in v0.1.** `tokio` is not justified yet: the CLI is short-lived, the nap protocol is synchronous by design (§1.5.4), and LSP (the only real async consumer) is post-v0.1 (ADR-0008). A static musl binary with no runtime is smaller, simpler to audit, and one less moving part for CI.
- **Panic policy:** `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]` in production modules from F0 onward (with `#[allow]` scoped to tests). FFI boundaries convert non-Rust failures into typed errors.

### 1.2 tree-sitter C pointer lifecycle in Rust (the core safety design)

This is the section eval-1 will attack. Every binding fact below is **verified against the tree-sitter 0.27.0 source and docs.rs** (Appendix A.3); the design turns each fact into an enforceable rule.

**Verified binding facts this design rests on** (sources in A.3):
- `Tree` is `Send + Sync` (unconditional `unsafe impl`s in `lib.rs` 0.27.0); `Parser`, `Node<'_>`, `TreeCursor<'_>`, `Language` likewise (`Parser`'s impls non-wasm).
- `Tree`'s `Clone` calls `ts_tree_copy` — a **shallow, refcount-incrementing copy ("very fast")**; a single `TSTree` must not be *mutated* while clones exist; concurrent **reads** through shared handles are the blessed pattern.
- `Node<'tree>` is `Copy`, carries `PhantomData<&'tree ()>`, and `kind()` returns `&'tree str` (borrowed, no allocation); `kind_id() -> u16`.
- `Node::children(&mut TreeCursor)` **takes a caller-owned reusable cursor** — the API itself prescribes cursor pooling.
- `TreeCursor::reset(node)` / `reset_to(cursor)` exist for cursor reuse.
- Parsing with a callback is `parse_with_options(callback, old_tree, options)` in 0.27 — `parse_with` was deprecated in 0.25 and **removed in 0.26**. Callback: `FnMut(usize, Point) -> T: AsRef<[u8]>`, receives `(byte_offset, position)`, returns a chunk starting at that offset; empty slice = EOF. `ParseOptions.progress_callback` can abort a parse via `ControlFlow::Break`.
- `InputEdit { start_byte, old_end_byte, new_end_byte, start_position, old_end_position, new_end_position }`; `Point.column` is measured in **bytes** (zero-based rows/columns; standardized in 0.25).
- `child_count()` returns `u32` in 0.27 (breaking change from `usize` in 0.26).
- `Language::id_for_node_kind(kind, named) -> u16` maps kind names to ids; **0 = not found**.
- Error detection: `node.has_error()` (subtree), `node.is_error()` (kind `"ERROR"` / `"_ERROR"`, ids at the top of the `u16` range), `node.is_missing()`.

**Ownership map.**

| C object | Rust wrapper | Owner | Lifetime relation |
|---|---|---|---|
| `TSParser` | `Parser` | one per language per worker (pool) | none |
| `TSTree` (refcounted in C) | `Tree` | `ParseCache` entry, `Arc<Tree>` | root of all borrows |
| `TSNode` (raw inner ptr, no ownership) | `Node<'tree>` | borrowed, `Copy` | `'tree` ties it to the `Tree`; **never stored beyond the borrow** |
| `TSTreeCursor` (C-managed stack) | `TreeCursor<'tree>` | traversal-local; pooled per worker | borrows tree; `reset()`/`reset_to()` to reuse |

**Rule T1 — clones are cheap; *editing a shared tree* is the hazard, and exclusivity is enforced, not assumed.** `Tree::clone` is a shallow refcount copy, so sharing is `Arc<Tree>` for ownership clarity (one drop point, one cache identity) — no newtype needed. `Tree::edit` requires `&mut Tree`, reachable through `Arc<Tree>` only via `Arc::make_mut` — but `make_mut`'s clone-on-write does **not** isolate the C tree: because `ts_tree_copy` is a refcount-incrementing alias, the cloned handle wraps the *same* `TSTree`, and A.3.2 forbids mutating a tree while clones exist. Sharing for concurrent **reads** remains the blessed pattern; the edit path therefore requires exclusivity by discipline: `apply` removes the `ParseEntry` from the cache, takes ownership, and asserts `Arc::strong_count(&tree) == 1` before `Tree::edit`, failing to a typed error if any other handle exists (§1.2.1 step 5; KP-1's shared-clone leg pins it). Rule: nothing outside that guarded path ever calls `Tree::edit`.

**Rule T2 — bytes and tree die together.** tree-sitter trees store byte/point offsets into a *source* they do not own. Our cache entry is:

```rust
// sketch — src/ast/cache.rs
struct ParseEntry {
    gen: Generation,          // overlay generation this tree was parsed at
    bytes: Arc<[u8]>,         // the exact snapshot the tree indexes — kept alive here
    tree: Arc<Tree>,          // offsets in `tree` are only meaningful against `bytes`
    error_count: u32,         // precomputed at parse time (ERROR/MISSING count)
}
```

A `Node<'t>` handed out by a cache entry is only ever interpreted against `entry.bytes`. Because both live in the same entry, there is no window where offsets dangle. This *requires* the `Arc<[u8]>` snapshot design in the VFS (§1.3) — the two designs lock together.

**Rule T3 — `Node` never crosses the work item, never outlives its `Arc<Tree>`.** Even though `Node<'_>` is (surprisingly) `Send + Sync` in 0.27, we keep the strict discipline: rayon workers receive **owned** `Arc<[u8]>` + `Arc<Tree>`, borrow `tree.root_node()` locally, walk with pooled cursors, and return **plain serializable results** (`MatchReport { path, start_byte, end_byte, start_point, end_point, kind_id, captures }`) — values, no borrows. Kind *names* are resolved at serialization time via the per-language id→name table, so the hot loop never touches `&str` lifetimes. A compile-time canary in `src/ast/send_assert.rs` pins the `Send`/`Sync` impls as a **regression fence** (if a future tree-sitter release narrows them, the build breaks loudly rather than the product silently changing threading contracts):

```rust
// sketch — regression fence, not speculation: verified true for 0.27.0
fn assert_send<T: Send>() {}
fn assert_sync<T: Sync>() {}
fn _canaries() {
    assert_send::<tree_sitter::Tree>();  assert_sync::<tree_sitter::Tree>();
    assert_send::<tree_sitter::Parser>();
    assert_send::<tree_sitter::TreeCursor<'static>>();
}
```

**Fallback if a future release breaks this** (designed in advance, not needed for 0.27): parsing happens on the rayon worker that will do the matching (parse and match fused in one work item); the cache becomes per-worker with a merge at collection.

**Rule T4 — invalidation is generation-based, never heuristic.** Every overlay mutation bumps a per-path `Generation` counter (and a global one). A cache entry is valid iff `entry.gen == overlay.current_gen(path)` **and** `entry.bytes` content-hash matches (cheap belt-and-suspenders: compare the `Arc` pointer identity first, hash second). No mtime guessing, no partial trust of stale offsets. After `apply` edits a file, the old entry is not mutated; a new entry is built via `Tree::edit` + re-parse (§1.2.1). refactree hit the same trap and clears its cache when overlay content changes "so overlay content is not masked" (A.4) — our generation keys make that masking impossible by construction.

**Rule T4a — stale `Node`s from a pre-edit tree are impossible to misuse.** Any API that returns match coordinates returns *copied values*, not `Node` handles. The only long-lived handles live inside version-pinned `ParseEntry`s; the evaluator gets its nodes exclusively from the current entry for the file being scanned.

**Rule T5 — cursors are pooled, exactly as the API intends.** `Node::walk()` may heap-allocate the cursor's internal stack; `Node::children(&mut cursor)` and `TreeCursor::reset()` exist precisely so callers reuse cursors. The evaluator's traversal keeps an explicit `Vec<TreeCursor>` stack (one cursor per depth level, `reset()` on reuse) — this is the concrete mechanism behind the BRIEF's "optimize allocations in tree traversal", and KP-8's counting-allocator test enforces it.

**Rule T6 — no `unsafe` in janitor code.** All C pointer management stays inside the upstream `tree-sitter` crate. Our unsafe surface is zero; the risk is transferred to *usage discipline* (rules T1–T5), which tests and clippy enforce.

#### 1.2.1 Edit → incremental re-parse choreography (the `apply` hot path)

```
1. Load ParseEntry(gen=N, bytes=B_old, tree=T_old) for path P.
2. Validate the Edit payload against B_old (offsets in range; expected old bytes
   match; edits sorted, non-overlapping — else typed error E_EDIT_MISMATCH /
   E_EDIT_OVERLAP; catches agents acting on a stale view).
3. Compute B_new = B_old with edits spliced DESCENDING by offset (later edits
   first, so earlier offsets stay valid — refactree's proven ordering, A.4);
   overlay.stage(P, B_new) — gen → N+1. (The overlay keeps the previous Arc
   for rollback until commit.)
4. Compute InputEdit per edit: start/old_end/new_end bytes + Points derived
   from B_old / B_new by prefix newline counting. Point.column is in BYTES
   (verified, A.3) — so column = byte offset minus last line-start byte offset.
5. Exclusivity guard, then edit (eval-2 amendment 1): apply REMOVES the
   ParseEntry from the cache and takes ownership BEFORE editing; assert
   Arc::strong_count(&tree) == 1 at the edit point — if not, fail to a
   typed error and never proceed (Tree::clone is a shallow
   refcount-incrementing alias, so make_mut does NOT isolate the C tree;
   A.3.2 forbids mutating a TSTree while clones exist). Only then
   Tree::edit(InputEdit) repositions offsets to B_new coordinates
   (Rule T1). A fresh entry is inserted after re-parse (step 6).
6. parser.parse_with_options(callback_over(B_new), Some(&edited_tree), None)
   → T_new. The callback serves chunks from the Arc<[u8]> snapshot
   (|off, _| &bytes[min(off, len)..], empty slice = EOF) — this is how
   "the parser reads bytes primarily from vfs::Overlay" is implemented
   with no lock held during the parse. (progress_callback is available for
   future cancellation budgets; unused in v0.1.)
7. Validation gate (§1.6): count ERROR/MISSING nodes in T_new restricted to
   edited byte ranges. Zero → commit (flush to Os, drop rollback Arc).
   Non-zero → rollback B_old into the overlay (gen → N+2), return
   E_PARSE_INVALID with the offending node coordinates as JSON.
```

Full re-parse (no old tree) is the same minus steps 4–5. KP-1 measures whether steps 5–6 actually beat full re-parse on a 3 MB file at 6 edit sizes; if not, we drop `Tree::edit` and always full-parse (simpler, one less correctness surface) — the architecture above does not change, only the fast path.

### 1.3 Module design — `src/vfs`

```rust
// sketches only — real signatures fixed in F2 with tests
pub trait ProjectFs: Send + Sync {
    fn read_dir(&self, path: &Path) -> VfsResult<ReadDirSnapshot>;
    fn read_file(&self, path: &Path) -> VfsResult<Arc<[u8]>>;   // Arc: zero-copy snapshot
    fn stat(&self, path: &Path) -> VfsResult<Stat>;
}

pub struct Os;   // thin proxy over std::fs, error → VfsError mapping

pub struct Overlay {
    base_fs: Arc<dyn ProjectFs>,
    buffer: RwLock<HashMap<VfsPath, BufferEntry>>,   // interior mutability
    gens:   RwLock<HashMap<VfsPath, Generation>>,    // invalidation feed (T4)
}

enum BufferEntry {
    File(Arc<[u8]>),   // staged/edited content — Arc so snapshots are O(1)
    Tombstone,         // delete() shadows the base file (see invariant 3)
}
```

**Invariants (each maps to a mandated test, §3 F2):**

1. **Read-through:** `read_file`/`stat` consult the buffer **first**; miss falls to `base_fs`. A staged file returns staged bytes even if a different file exists on disk.
2. **Anti-phantom:** `read_dir` **ignores the buffer entirely** and calls only `base_fs`. Virtual files never appear in listings. This is deliberate: directory listings stay a faithful mirror of disk, so an agent can never mistake a staged file for a persisted one. (refactree's `projectfs.Overlay` follows the same philosophy of keeping disk truth and staged content clearly separated, A.4.)
3. **Tombstone delete:** `delete()` writes a `Tombstone`. Subsequent `read_file` is `Err(VfsError::NotFound)` **even if the base file exists on disk** (the delete must win over the fallback), while `read_dir` output is unchanged (the on-disk file still lists). A bare `HashMap::remove` would violate the mandated test semantics — hence `BufferEntry::Tombstone` instead of the BRIEF's `HashMap<PathBuf, Vec<u8>>` sketch (ADR-0009).
4. **Fallback:** with no buffer entry, behavior is byte-identical to `Os`.

**Beyond the BRIEF's sketch (hidden sub-scope called out by eval-2 §3.1):**

- **Path canonicalization:** a private `VfsPath` newtype normalizes at the boundary (`./`, repeated separators, `..` resolved lexically; symlinks *not* resolved — documented), so `/a/b`, `/a//b` hit the same buffer slot. Property-tested.
- **`stat` on virtual files:** synthesized from the entry — `len = bytes.len()`, flagged `is_virtual: true` so downstream never mistakes it for a disk file.
- **Virtual-file visibility for `scan` (eval-2 §6.6):** the anti-phantom invariant quietly hides staged files from any `read_dir`-based walk, which would make `scan` silently wrong after an agent stages a new file. We resolve the conflict **without weakening the invariant**: `Overlay::virtual_paths() -> Vec<VfsPath>` is an explicit, separate surface, and `scan` walks `base read_dir ∪ virtual_paths()` when `--include-virtual` is set (default OFF — scan reports disk reality; the flag opts into overlay visibility, ADR-0016; virtual hits are marked as virtual in the report). The invariant stays pure; the product stays correct.
- **Transaction boundaries:** `stage(ops) / commit() / rollback()` around multi-file `apply`: all-or-nothing into the buffer; `commit()` flushes to `Os` with per-file tmp+rename, ordered by path, and on partial OS failure returns a structured `E_FLUSH_PARTIAL` report listing persisted vs. rolled-back paths (true cross-file atomicity is impossible without a journal; a `.janitor/txn.log` journal is post-v0.1, §7). This mirrors refactree's `StageEdits → ValidateStagedProject → CommitOverlay`, independently validating the staged-overlay-commit shape (A.4).

### 1.4 Module design — `src/ast`

#### 1.4.1 DSL surface: s-expression (ADR-0003)

The BRIEF's internal algebra maps 1:1 onto s-expressions, which tree-sitter users already recognize:

```scheme
;; structural pivot: everything below is scoped to `function_item` nodes only
(under function_item
  (seq (capture @fn (kind identifier))     ; first child, captured
       (kind block))                        ; strictly adjacent next sibling
  (constraints (unify @fn @other)))         ; top-level isomorphism constraints
```

Grammar (complete, tiny — lexer+recursive-descent parser ≈ 300 lines, no dependency):

```
rule       := "(" "under" kind body ")" | "(" "seq" atom+ ")" | atom
atom       := kind | wildcard | "(" "capture" symbol atom ")" | rule
body       := atom | constraints
constraints:= "(" "constraints" "(" "unify" symbol symbol ")"* ")"
kind       := identifier            ; tree-sitter node kind, e.g. function_item
wildcard   := "_"
```

Elaborated internal AST (the BRIEF's four primitives, plus the two atoms they imply):

```rust
enum Pattern {
    Under(NodeType, Box<Pattern>),  // structural pivot — scopes the state machine
    Seq(Vec<Pattern>),              // strict in-order adjacency among siblings
    Capture(Symbol, Box<Pattern>),  // bind matched subtree to a symbol
    Unify(Symbol, Symbol),          // constraint: two captures isomorphic
    Kind(NodeType),                 // atom: match this node kind
    Any,                            // atom: wildcard
}
```

`Seq(Vec<NodeId>)` in the BRIEF is interpreted as *pattern-element* ids in sequence position (a `Vec<Pattern>` after elaboration), not tree-node ids — tree nodes don't exist before matching (ADR-0004 records this interpretation and the alternative).

**Error quality is product surface:** the DSL's users are LLM agents that *will* hallucinate syntax. The parser produces typed errors with position, expected-token set, and a suggested correction when close (`did you mean (capture @fn ...)?`), all JSON-serializable.

#### 1.4.2 `NodeType` across languages (ADR-0002)

Node kinds are per-grammar strings (`function_item` in rust, `function_declaration` in python). Design:

```rust
struct NodeType { lang: LangId, kind: InternedStr }   // global interner, u32-backed
```

- The DSL surface uses bare kind names; resolution uses the verified API `Language::id_for_node_kind(kind, named) -> u16` (0 = unknown) under **one behavior** (eval-1 F4): at rule-compile time, a kind unknown in **every** enabled language ⇒ loud typed `UnknownKind` error (the rule is wrong everywhere); otherwise resolution is **per target file language** — when scanning file F of language L, each `NodeType` resolves against L, and `0` ⇒ the rule provably cannot match in L ⇒ the file is skipped and counted (`"skipped_for_language": {...}` in the scan report — no silent no-ops). Kind tables are seeded at first use from the **bundled per-grammar fixture corpora** (each grammar crate's own test fixtures) — not from whichever file a scan happens to hit first (removes corpus bias, eval-1 F4).
- Hot-loop matching compares `u16 kind_id`s resolved at pattern-compile time — zero string compares, zero allocations in the matcher (BRIEF hard rule). `kind()` borrows `&'tree str`, so kind *names* appear only at result-serialization time via the per-language id→name table.
- **Global-layer generic rules (eval-2 §6.8):** cross-language "generic AST heuristics" are constrained in v0.1 to a tiny normalized vocabulary (`FUNCTION`, `IDENT`, `LITERAL`, `BLOCK`) mapped per-language in a table (`lang/{rust,python}.norm.toml`). Anything richer is post-v0.1 and explicitly not promised. (refactree solves the same variance with per-language driver capability interfaces — A.4 — adopted in spirit via the registry + norm tables.)

#### 1.4.3 The evaluator: compilation to a scoped state machine (ADR-0005)

The founding value is **explicit scope semantics with a proven linearity bound** (eval-3 C6 reframing): tree-sitter matching is already compiled and traversal-linear, so the DSL's point is not backtracking rescue — it is that `Under` restricts every instruction to one pivot's child list and strict adjacency is defined over that scope, yielding the `O(children × states)` bound that KP-7 makes falsifiable. (A naive recursive matcher would still explode; the memoized-NFA mechanism below is what makes the bound real.) Design (independent precedent: refactree matches with a "token stream + pattern NFA", A.4):

1. **Compile, don't interpret.** `Pattern → Program`: a flat instruction list (Thompson-style) per `Seq` group: `Kind(k) | Any | Capture(sym, pc) | Split(a,b) | Match`. Compilation is per-language (kind resolution, §1.4.2).
2. **`Under` = hard scope.** The executor enters at each `Under` pivot node and runs **only over that node's children list** — the search space is the sibling sequence, never the whole subtree graph. Nested `Under` descends one scope level. No instruction can escape the scope, so the state space is bounded by `O(children × states)` per pivot.
3. **Linear via memoized failure.** Within a scope, matching `Seq` is NFA-simulation over the child vector: `(child_index, pc) → outcome` memo table (bitset). A state that failed at child *i* is never retried at child *i* — the mechanical guarantee of "no catastrophic backtracking". The F4 benchmark makes it falsifiable: an adversarial wide/deep tree plus alternation-heavy pattern must run ~linearly, next to a deliberately naive backtracking baseline that documents the explosion we avoid (KP-7).
4. **Trivia semantics for `Seq` (eval-2 §6.2 — resolved by experiment KP-6, fixed in ADR-0010):** adjacency is defined over **named siblings, skipping `comment`-class kinds** by default (`--strict-trivia` restores byte-strict). Real code has comments between statements; byte-strict would make most real patterns unmatchable. The F1 experiment pins this with five real-world patterns before the evaluator is built.
5. **`Capture`** fills a symbol table `(Symbol → MatchReport)`. `Unify(s1, s2)` is a post-match filter over captures.
6. **`Unify` via Merkle fingerprints (F8):** per-parse bottom-up structural hash over the **named-node skeleton with leaf token texts PLUS semantically-significant anonymous tokens (binary operators, separators, punctuation), excluding ONLY comment/whitespace-class trivia kinds** (per-grammar trivia table, KP-5) — excluding *all* anonymous nodes would conflate `a + b` with `a - b`, since operators and separators are anonymous tokens (eval-1 F2, folded into ADR-0010). The equivalence definition ADR-0010 fixes (eval-2 §6.3: trivia-sensitive isomorphism detects almost nothing real). 128-bit hashes (SipHash-1-3 of `kind_id || child_hashes || leaf_text_hash`); `Unify` = hash equality — O(1) per check, O(n) preprocessing per file, reused by duplicate-detection scans (the "base para abstração de duplicidade").

#### 1.4.4 Parse cache (ADR-0007)

In-process only for v0.1: `ParseCache` backed by `dashmap`, keyed by path, entries validated by generation + content-hash (Rule T4). Cold cache each CLI invocation — a full-repo `scan` re-parses everything; accepted for v0.1 (rayon makes it tolerable; warm cache is the daemon/LSP story, §7). refactree makes the same trade: on-demand walks, in-process per-file cache, **no persistent index to warm** (A.4). Disk cache directory layout is reserved (`.janitor/cache/parse/<sha256(lang + content)>`) but unwritten in v0.1.

**v0.1 memory discipline (eval-1 F1):** `scan` processes files streaming — each `ParseEntry` is dropped after that file's report is collected; the process never retains O(repo) bytes+trees (a single-rule cold scan has a zero cache hit rate, so retention is pure cost and an OOM vector). The dashmap pays off only for multi-rule scans / warm processes (F10). **Apply-path ownership (eval-2 amendment 1):** `apply` removes the `ParseEntry` and takes ownership before `Tree::edit` (exclusivity guard, §1.2.1 step 5); a fresh entry is inserted after re-parse. Match-set semantics — local maxima per pivot, flagged overlap, canonical `(start_byte, end_byte, rule_id)` order, determinism across rayon thread counts — are fixed in ADR-0016 and documented in `docs/SCHEMA.md`.

### 1.5 Module design — `src/memory`

Semantics adopted from OptMem where the BRIEF is silent, re-engineered (not copied — OptMem ships **no license**, see §10 and ADR-0013) and adapted from "global per-user memory" to our Global+Local layered, pointer-carrying model. Storage layout:

```
.janitor/                 (local layer; created by `janitor init`)
  LOG.txt                 append-only raw facts — JSONL, one per line (ADR-0006)
  TREE/2, TREE/4, ...     one fixed-width record file per block size (summaries;
                          pure cache, rebuildable from LOG.txt alone)
  STATE.json              linter/scan status surfaced by `wake`
  cache/parse/            reserved, unwritten in v0.1
```

#### 1.5.1 Log format (ADR-0006)

`.janitor/LOG.txt` — append-only JSONL, one fact per line, fsync on append, **position-derived ids** (note *i* is line *i*; next id = line count — OptMem's "position is identity", kept):

```json
{"v":1,"id":42,"ts":"2026-09-13T12:00:00Z","ptr":{"path":"src/vfs/overlay.rs","span":[120,480],"kind_name":"impl_item","content_sha":"…"},"text":"Overlay read_dir must never consult the buffer","flags":[]}
```

- Torn-write recovery: a trailing partial line (crash mid-append) is detected at load (JSON parse failure on the last line) and quarantined to `.janitor/QUARANTINE.txt`, with a JSON warning in `wake` — append-only never blocked by corruption. (OptMem repairs its torn 320-byte records the same way in spirit — A.2.)
- `flags` includes `MemoryFlag::ArchitecturalLock` — locked facts are **exempt from compression**: the wake tiler must not merge them into summary blocks; they always surface raw at their position (§1.5.2). Never diluted, by construction.
- Why JSONL and not OptMem's fixed 320-byte records: our facts carry structured pointers that don't fit fixed width, and the local layer is repo-scale (thousands of notes), where a full load is cheap. OptMem's fixed-width exists for 1M-memory global stores with O(1) seeks. Summary *tree* records adopt fixed width (256 B) precisely because they are plain one-liners — O(1) seek where it pays.

#### 1.5.2 Layers & `wake` — OptMem's block tiling, token-budgeted

`ContextManager` holds: **Global** layer — shipped defaults embedded via `include_str!` (`share/GLOBAL.md`, generic heuristics over the normalized vocabulary §1.4.2) plus optional user override at `$XDG_CONFIG_HOME/janitor/GLOBAL.md`; **Local** layer — `$PWD/.janitor/`.

`wake(budget_tokens)`:

- Token estimate = `ceil(bytes/4)` (`"estimator":"chars4"` — ADR-0011; no tokenizer dependency in v0.1; OptMem itself budgets in *lines*, ~96 ≈ 8k tokens, we budget in tokens directly).
- **Recall algorithm (adopted from OptMem, reimplemented):** one chronological **oldest-first** pass interleaving raw facts and summaries; the id range is tiled with aligned power-of-two blocks; a block stays whole only if its size is small relative to its age (present-bias — detail spends on the recent, coarseness on the ancient); leftover budget splits the largest compressed block. Everything fits ⇒ nothing is compressed at all.
- `ArchitecturalLock` facts are force-split out of any multi-fact block — always raw.
- **Wake gates on completeness** (OptMem's rule): if the tiling needs a summary slot that is not yet written, `wake` returns `E_NAP_PENDING` with the pending compression request instead of silently serving holes. Final success payload includes `"awake": true`.
- Output: JSON payload with loaded ids/summaries, estimate, truncation flag, linter status from `STATE.json`.

#### 1.5.3 `note` and SyntaxPointer anti-hallucination (ADR-0012)

Eval-2 §6.4 flags the strongest memory risk: raw tree node ids dangle after any re-parse. Pointers are therefore **not** node ids:

```rust
enum SyntaxPointer {
    Node { path: VfsPath, span: (u64, u64), kind_name: String, content_sha: Digest }, // re-anchorable
    Commit { sha: String },   // validated via `git rev-parse --verify <sha>^{commit}` (subprocess; ADR-0013)
}
```

`note(pointer, content)`: validate before append — (a) resolve `Node` pointer against the *current* tree: exact span+hash hit, or **re-anchor** (search the current tree for a node with matching `content_sha` near the old span; record `"reanchored": true` with drift distance), else typed error `E_POINTER_DANGLING` — the anti-hallucination gate; (b) `Commit` pointers shell out to `git` (no library dependency in v0.1; `gix` revisit post-v0.1). Facts that can't be anchored are **refused**, never silently accepted. (OptMem has *no* anchors at all — anti-hallucination there is prompt wording only, A.2; the BRIEF's `SyntaxPointer` requirement is our addition.)

One-line limit (OptMem discipline): `text` ≤ 280 bytes, single line — `wake`'s budget math depends on it.

#### 1.5.4 `nap` — compression as a synchronous agent protocol (ADR-0013)

The BRIEF calls `nap()` a background routine; OptMem's hard-won lesson (verified, A.2) is that summarization **must** be done by the LLM agent itself and sequenced **synchronously, one compression at a time, in the output of `note`/`wake` — "Nothing ever runs in the background"** — because a static tool has no LLM. We adopt exactly that model and record the deviation from the BRIEF's wording:

1. `janitor nap` (no args) → JSON with the **next pending compression request**: block `#lo-hi`, the source material (raw facts if block ≤ 16, else the two child summaries), and the instruction slot. Nothing pending → `"Nothing left to compress."`
2. The consuming agent writes the summary: `janitor nap --slot "0-1" --text "…"` (≤ 256 bytes, one line). Validates + appends one fixed-width record to `TREE/<size>`.
3. Blocks are **built in order** (each `TREE/<size>` is a dense prefix; its length *is* the build frontier; out-of-order submission → typed error; re-submission of a settled block → idempotent no-op).
4. `ArchitecturalLock` facts never enter compression ranges (§1.5.2). `TREE/*` is a pure cache rebuildable from `LOG.txt`; a `forget`-style truncate-and-rebuild command is post-v0.1.
5. `wake` enforces completeness (§1.5.2) — the loop closes without any background routine and without an LLM inside the binary.

In v0.1 this protocol ships with the format fixed (ADR-0013) but is exercised only by the agent harness/integration tests (eval-2 §7 cut list).

### 1.6 Module design — `src/cli` + `src/orchestrator`

**Commands** (clap derive; superflat, agent-friendly):

| command | purpose | key flags |
|---|---|---|
| `janitor init` | create `.janitor/{LOG.txt,TREE/,STATE.json}`; refuse if exists | `--force` |
| `janitor wake` | memory payload + linter status (§1.5.2) | `--budget N` |
| `janitor note` | append validated fact | `--ptr <json>`, `--text`, `--lock` |
| `janitor nap` | next pending compression / store a summary | `--slot "0-1"`, `--text` |
| `janitor scan` | parallel structural search (disk-visible files by default; virtual union only under the flag) | `--rule`, `--lang`, `--strict-trivia`, `--include-virtual/--no-include-virtual` (default OFF, ADR-0016), `--max-file-size` |
| `janitor apply` | stage edits → validate → flush | `--diff <json\|@file>`, `--dry-run` |

**Versioned JSON envelope from the first release** (eval-2 §6.9 — agent prompts bake formats; retrofit = breaking change):

```json
{ "schema": "janitor.v1", "ok": true, "data": { … } }
{ "schema": "janitor.v1", "ok": false, "error": { "code": "E_POINTER_DANGLING", "message": "…", "details": { … } } }
```

Everything on stdout (agents parse stdout); exit 0 success / 2 usage / 1 operational error. clap usage errors are intercepted at `main` and re-emitted as JSON envelope errors (`clap::Error` render hook) so even `--help` mistakes stay machine-parseable. `--pretty` for humans. (Precedent: refactree's `rft grep --format jsonl` is its machine surface — same separation, A.4.)

**Edits payload (`apply --diff`)** — ADR-0014:

```json
{ "operations": [ { "path": "src/x.rs",
    "expect_sha": "…",                       // optional staleness guard
    "edits": [ { "start_byte": 10, "old_end_byte": 24, "new_text": "const" } ] } ] }
```

Offsets are **half-open UTF-8 byte ranges** `[start_byte, old_end_byte)` into the **current overlay-visible content** — the same span model refactree proved out (A.4); `expect_sha` (content hash) makes stale-view agents fail fast with `E_STALE_VIEW` instead of corrupting. Validation semantics: no ERROR/MISSING tree-sitter nodes **within edited byte ranges** after re-parse (§1.2.1 step 7) — "valid" is defined as *localized* syntactic soundness, and KP-4 measures the honest false-negative rate of grammar error-recovery rather than assuming it away.

**`orchestrator::Session`** is the composition root (sketch):

```rust
pub struct Session {
    overlay: Arc<Overlay>,
    cache: Arc<ParseCache>,
    langs: LangRegistry,          // feature-gated grammars (§2 features)
    mem: ContextManager,
}
```

One session per CLI invocation; `scan` = walk (`base read_dir ∪ virtual_paths()` when `--include-virtual` is set — default OFF, ADR-0016) → filter by language → rayon (snapshot → parse-or-cache → compile-rule-once → execute → collect reports, then the ADR-0016 canonical sort — `(start_byte, end_byte, rule_id)` — so output is invariant across rayon thread counts); `apply` = §1.2.1 loop wrapped in overlay `stage/commit/rollback`.

### 1.7 Error taxonomy (thiserror, exhaustive)

`VfsError` (NotFound, Permission, Io, InvalidPath), `ParseError` (NoGrammarForLang, LanguageMismatch, SourceDecoding), `DslError` (Lex{pos,expected}, Parse{pos,expected,suggestion}, UnknownKind{lang,kind}, Compile), `MatchError`, `EditError` (Mismatch, Overlap, StaleView, OutOfRange), `ValidateError` (ParseInvalid{nodes}, FlushPartial{report}), `MemoryError` (PointerDangling, NapPending{slot}, LogCorrupt{quarantined}, BudgetInvalid), `OutputError`. All map onto the JSON envelope via one `From<JanitorError>` impl. No `unwrap`/`expect`/`panic` outside `#[cfg(test)]` — clippy-denied (§1.1).

---

## 2. Cargo.toml

Single crate, binary `janitor`, module dirs exactly as the BRIEF names them (`src/vfs`, `src/ast`, `src/memory`, `src/cli`, `src/orchestrator`) — ADR-0001. All versions verified against the crates.io API on 2026-09-13 (Appendix A.1), and the grammar×core compatibility question is **resolved by ABI evidence** (A.3): every grammar crate targets LANGUAGE_VERSION 14–15; core 0.27.0 accepts ABI 13..=15; grammar crates' *only* runtime dependency is `tree-sitter-language` (core is a dev-dependency in all of them), so no core-version conflict is even expressible at build time.

```toml
[package]
name = "janitor-prime"
version = "0.1.0"
edition = "2024"
rust-version = "1.90"            # tree-sitter 0.27.0 MSRV (crates.io rust_version, verified 2026-09-13); clap 4.6.6 (1.85) and edition 2024 (1.85) sit below the binding floor
license = "MIT OR Apache-2.0"
repository = "https://github.com/<org>/janitor-prime"
description = "Deterministic refactoring engine, transactional VFS and persistent memory for autonomous agents"
build = "build.rs"               # embeds grammar list for --version JSON

[[bin]]
name = "janitor"
path = "src/main.rs"

[dependencies]
# --- CLI & serialization ---
clap = { version = "=4.6.6", features = ["derive"] }   # derive is NOT a default feature (verified)
serde = { version = "=1.0.229", features = ["derive"] }
serde_json = "=1.0.151"
thiserror = "=2.0.20"                                   # 2.x is current; last 1.x = 1.0.69

# --- engine ---
tree-sitter = "=0.27.0"            # 2026-08-30; parse_with_options era (parse_with removed in 0.26)
tree-sitter-language = "=0.1.8"    # grammar crates' only runtime dep; same-day release as core 0.27.0
rayon = "=1.12.0"
smol-str = "=0.3.6"                # interned kind names backing store
dashmap = "=6.2.1"                 # ParseCache concurrent inserts from rayon workers

# --- diagnostics (stderr only; stdout stays 100% JSON) ---
tracing = "=0.1.44"
tracing-subscriber = { version = "=0.3.23", features = ["env-filter"] }

# --- grammars: one feature per language; rust ships in the default set (ADR-0015) ---
tree-sitter-rust = { version = "=0.24.2", optional = true }        # ABI 15
tree-sitter-python = { version = "=0.25.0", optional = true }      # ABI 15
tree-sitter-go = { version = "=0.25.0", optional = true }          # ABI 15
tree-sitter-javascript = { version = "=0.25.0", optional = true }  # ABI 15
tree-sitter-typescript = { version = "=0.23.2", optional = true }  # ABI 14
tree-sitter-java = { version = "=0.23.5", optional = true }        # ABI 14
tree-sitter-c = { version = "=0.24.2", optional = true }           # ABI 15

[features]
default = ["lang-rust"]
lang-rust = ["dep:tree-sitter-rust"]
lang-python = ["dep:tree-sitter-python"]
lang-go = ["dep:tree-sitter-go"]
lang-javascript = ["dep:tree-sitter-javascript"]
lang-typescript = ["dep:tree-sitter-typescript"]
lang-java = ["dep:tree-sitter-java"]
lang-c = ["dep:tree-sitter-c"]
lang-all = ["lang-rust", "lang-python", "lang-go", "lang-javascript",
            "lang-typescript", "lang-java", "lang-c"]

[dev-dependencies]
tempfile = "=3.27.0"
proptest = "=1.8.2"
criterion = "=0.11.0"              # benches: parse throughput, matcher linearity (KP-7)

[build-dependencies]
# nothing; build.rs is std-only

[[bench]]
name = "engine"
harness = false

[profile.release]
lto = "thin"
codegen-units = 1
strip = "symbols"
panic = "abort"        # policy: no panics in production; abort shrinks the static binary
```

The alternative — pin `tree-sitter = "=0.26.13"` (MSRV 1.77) — was considered and rejected: A.3's verified facts are all against 0.27.0, and the local toolchain is rustc 1.98, which builds against either pin.

**Deliberately absent (each an ADR):** `tokio` (ADR-0008 — not justified in v0.1), `git2` (ADR-0013 — shell out to `git`; no libgit2 in a static musl binary), `parking_lot` (std `RwLock` suffices under the no-panic policy; one less dep), `anyhow` (typed errors only, per BRIEF), `ast-grep-core` (ADR-0002 — build vs. leverage decision, §4).

**Residual `[VERIFY: F0 execution]` items:** (a) musl cross toolchain choice (cargo-zigbuild vs. musl-gcc) — an execution-environment fact, not researchable; (b) `lang-all` still gets a CI build job on every PR as a regression fence against future grammar/core releases (the ABI evidence is from *current* tags; new releases can shift it). All API touchpoints stay isolated in `src/ast/ffi_shim.rs` (§3 F3) so a pin-swap stays one-file.

---

## 3. Implementation phases (TDD)

Ordering follows eval-2's principles: kill-points before anything they could invalidate; a vertical slice in one language before breadth; `apply` before `nap`; `note` after the AST engine; nothing invisible polished before something visible works.

Gates apply to every phase unless noted: `cargo fmt --check` · `cargo clippy --all-targets -- -D warnings` (with `clippy::unwrap_used`/`expect_used`/`panic` denied in production modules) · `cargo test`. A phase ends at its commit boundary; history stays bisectable.

### F0 — Scaffold, CI, pin-check *(v0.1)*

**Objective:** repo skeleton that CI can reject; every later phase lands on green gates.

1. `Cargo.toml` (§2), `rust-toolchain.toml` (pin stable), `clippy.toml`, `rustfmt.toml`, `.gitignore` (incl. `.janitor/`).
2. `src/main.rs`, `src/cli/mod.rs` — clap derive skeleton: `init|wake|note|nap|scan|apply` stubs, `--version` emitting **JSON** (`{"schema":"janitor.v1","version":…,"grammars":[…]}`).
3. `src/output.rs` — versioned envelope types (`Envelope<T>`, error envelope, exit-code mapping), clap error render hook → JSON. Tests: envelope round-trip, error mapping.
4. `LICENSE` (MIT + Apache-2.0), `NOTICE` (credits, §10), `README.md` stub.
5. `.github/workflows/ci.yml` — jobs: fmt, clippy, test (ubuntu), **musl static build** (`x86_64-unknown-linux-musl` `[VERIFY: cargo-zigbuild vs musl-gcc — decide in F0]`), pin-check job building `--features lang-all`. Actions pinned by commit SHA.
6. **Pin-check record:** build `lang-all` against tree-sitter 0.27.0 (expected green per ABI evidence, A.3); record the outcome + musl toolchain choice in Appendix A.5.

**Gate:** CI green including musl artifact. **Commit:** `F0: scaffold, versioned JSON CLI envelope, CI gates`.

### F1 — Kill-point spikes *(v0.1; precedes all engine code)*

**Objective:** the cheap experiments that can invalidate the architecture, run *before* the architecture is built (§5 details each). Desk research already settled KP-2/3/5 (Appendix A.3); their spikes survive as confirmation fences. Spike code lives in `src/spikes/` behind `#[cfg(feature = "spikes")]`, deleted at F3 entry; output is decision memos merged into ADRs.

1. `src/spikes/incremental_reparse.rs` — **KP-1**: 3 MB generated Rust file; edits at 6 sizes; `Arc::make_mut` + `Tree::edit` + `parse_with_options` vs. full re-parse; correctness diff of resulting trees; timings to `benches/spike-kp1.json`.
2. `src/ast/send_assert.rs` — **KP-2 (fence)**: compile-time `Send`/`Sync` canaries; rayon mini-scan passing owned `Arc<Tree>` across threads, collecting plain data. Fallback path documented (§1.2) but not expected to trigger on 0.27.
3. `src/spikes/point_semantics.rs` — **KP-3 (fence)**: multibyte (emoji/CJK) corpus; confirm `Point.column`-in-bytes empirically against docs (A.3); locks the `InputEdit` math (§1.2.1 step 4).
4. `src/spikes/trivia_adjacency.rs` — **KP-6**: five real-world patterns (eval-2 §6.2) against byte-strict vs. comment-skipping adjacency; pick the semantics for ADR-0010 before the evaluator exists.
5. `src/spikes/grammar_compat.rs` — **KP-5 (fence)**: per-grammar kind-name dump (`function*`, `comment`, `identifier`) across `lang-all` via `id_for_node_kind`; feeds the normalized vocabulary table (§1.4.2).
6. `src/spikes/error_nodes.rs` — **KP-4**: broken-source corpus (10 mangled files); measure ERROR/MISSING **localization** — does "no errors in edited ranges" (§1.6) hold? Records the honest false-negative rate.
7. `src/spikes/scan_latency.rs` — **KP-10**: scan a large real repo (e.g. the tree-sitter or rust-analyzer checkout), cold parse; measure wall time and RAM ceiling. Converts ADR-0007's accepted wart into a measured decision (eval-2 amendment 3).

**Gate:** `cargo test --features spikes` green; decision memos merged into ADR-0005/0010/0012; KP-1 numbers in Appendix A. **Commit:** `F1: kill-point spikes (incremental parse, send/sync, points, trivia, grammar matrix)`.

### F2 — VFS with invariant tests *(v0.1)*

**Objective:** the transactional overlay, fully tested, before any tree-sitter dependency exists.

1. `src/vfs/error.rs` — `VfsError` (thiserror), JSON-enveloped.
2. `src/vfs/path.rs` — `VfsPath` canonicalization newtype; `proptest` suite: normalize round-trip, `..`/`./` collapse, rejection of non-normalizable inputs.
3. `src/vfs/fs.rs` — `trait ProjectFs` (read_dir / read_file → `Arc<[u8]>` / stat), `struct Os`.
4. `src/vfs/overlay.rs` — `Overlay` with `BufferEntry::{File(Arc<[u8]>), Tombstone}`, `set`, `set_string`, `delete`, `get`, `get_string`, `virtual_paths()`, generation counters.
5. **The four mandated invariant tests** (`tests/vfs_invariants.rs`), each named for the property:
   - `anti_phantom: read_dir never lists buffer files` — `set` a virtual file; `read_dir` output **equals** `Os::read_dir` exactly; the test FAILS if a phantom appears.
   - `read_through: set then read_file returns the new bytes` — including when a *different* file exists at the same path on disk.
   - `fallback: without set, read_file reads disk` — byte-identical to `Os`.
   - `delete: read_file after delete is NotFound, read_dir unchanged` — tombstone shadows disk; listing untouched.
6. `src/vfs/txn.rs` — `stage(ops)/commit()/rollback()`; commit = ordered tmp+rename flush; partial-failure → `E_FLUSH_PARTIAL` report. Tests: two-file commit; flush-failure injection (read-only dir); rollback restores generations.
7. `src/vfs/stat.rs` — synthetic stat for virtual entries (`is_virtual: true`). Tests: stat buffer-first, disk fallback.
8. Concurrency test: rayon hammering `read_file`/`read_dir`/`set` on a shared `Arc<Overlay>` under the guard-discipline rule (§1.1); no deadlock, no torn reads.

**Gate:** all gates; the four invariant tests green. **Commit:** `F2: transactional overlay with anti-phantom invariants`.

### F3 — Parse layer over Overlay *(v0.1)*

**Objective:** trees parsed from overlay bytes only, cached, generation-invalidated — the §1.2 rules implemented.

1. `src/ast/ffi_shim.rs` — the *only* file importing `tree_sitter` types: `parse_with_options` wrapper (callback serves chunks from `Arc<[u8]>`), `InputEditFor` helper (byte-column point math per A.3), grammar registration via `tree_sitter_rust::LANGUAGE.into()` (`LanguageFn` — the current constructor; the old `language()` fn no longer exists, A.3). Pin-swapping core = touching this file only.
2. `src/ast/lang.rs` — `LangRegistry`: feature-gated grammars, extension→language map, `id_for_node_kind`-based kind resolution (u16; 0 ⇒ `UnknownKind`).
3. `src/ast/parse.rs` — `parse_snapshot(&Arc<[u8]>)` (no locks held; Rules T2/T6); `InputEdit` choreography via `Arc::make_mut` (§1.2.1 steps 4–6) behind the same API. Note: `child_count() -> u32` in 0.27 (breaking vs 0.26).
4. `src/ast/cache.rs` — `ParseEntry {gen, bytes, tree, error_count}` + dashmap cache; validation by gen + Arc-ptr/content-hash (Rule T4). Tests: gen bump invalidates; stale entry never served; concurrent-insert race.
5. `src/ast/errors.rs` — ERROR/MISSING counting + range-restricted query (§1.6 validation semantics; `is_error`/`is_missing`/`has_error` verified, A.3); corpus test from KP-4.
6. `src/ast/walk.rs` — pooled-cursor traversal helpers (Rule T5; `children(&mut cursor)` + `reset()` — the API-blessed reuse pattern) + `tests/alloc.rs`: counting allocator asserting the traversal budget — **zero heap allocations per visited node** beyond result vectors (KP-8).

**Gate:** gates + send canary green + alloc test. **Commit:** `F3: generation-keyed parse cache over overlay snapshots`.

### F4 — DSL + evaluator + `scan` *(v0.1)*

**Objective:** the vertical slice — rule in, match coordinates out, rayon-parallel, one language (rust).

1. `src/ast/dsl/lexer.rs`, `src/ast/dsl/parser.rs` — s-expression front-end (§1.4.1); typed errors with position + suggestion. Tests per error class; golden parse tests.
2. `src/ast/dsl/ir.rs` — `Pattern` elaboration; `Seq(Vec<NodeId>)` interpretation locked (ADR-0004).
3. `src/ast/intern.rs` — global interner (`NodeType{lang, kind}`); per-language resolution via the registry; one resolution behavior (§1.4.2): `UnknownKind` typed error at rule-compile if the kind is unknown in every enabled language, else per-file skip counted in `skipped_for_language` (no silent no-ops).
4. `src/ast/dsl/compile.rs` — `Pattern → Program` (per-language): flat instructions, `Kind` pre-resolved to kind id.
5. `src/ast/exec.rs` — scoped state machine: `Under` scope entry, NFA over the sibling vector, `(child_index, pc)` memo bitset, capture table (§1.4.3). Trivia policy per ADR-0010 (KP-6 outcome). KP-8's counting-allocator fence covers `exec.rs` (re-anchored from F3 walk helpers — eval-2 amendment 7).
6. `src/orchestrator/session.rs`, `src/orchestrator/scan.rs` — file walk (`base read_dir ∪ virtual_paths()` under `--include-virtual`, default OFF — ADR-0016), language filter, rayon map (snapshot → cache → compile-once → exec), report collection + ADR-0016 canonical sort and the `RAYON_NUM_THREADS=1`-vs-`=8` byte-identical determinism test.
7. `src/cli/scan.rs` — CLI wiring, JSON report (`matches[]` with byte+point coords, captures, `skipped_for_language`) — refactree's `GrepHit` shape (file, line, col, captures) is the precedent, JSON-first instead of text-first.
8. `benches/engine.rs` (criterion): (a) parse throughput on a real corpus; (b) **matcher linearity (KP-7)** — adversarial pattern/tree scales ~linearly; a naive backtracking baseline documents the avoided explosion.
9. `tests/dsl_fixtures/` — fixture-driven rule tests in refactree's `op.json` spirit (A.4): pattern + language + expected match count, shared between CLI and test harness — the anti-regression corpus for the DSL.
10. Walk policy (eval-1 F6): NUL-byte sniff on the first 8 KB ⇒ skip binary; size cap (default 4 MB, CLI-overridable) ⇒ skip oversized; both surface as `"skipped": {"reason": ...}` in the scan report. `set_string` validates UTF-8 ⇒ typed error; `utf8_text`'s `Err(Utf8Error)` path is a typed error in every text-touching DSL op (including `Unify` leaf hashing at F8).
11. `docs/DSL.md` drafted here, moved from F7 (eval-2 amendment 4): it is the artifact KP-9 tests against; drafting at F4 prevents post-freeze surface churn (R12/R16). F7 keeps release polish only.
12. **KP-9 at F4 entry** (before evaluator polish): a real LLM authors N rules from `docs/DSL.md` alone; measure syntax-error and retry rates — typed errors + suggestions are the product surface (eval-2 amendment 2). If authoring fails, iterate the surface before semantics freeze.

**Gate:** gates + linearity bench tolerance met. **Commit:** `F4: s-expression DSL, scoped state-machine matcher, janitor scan`.

### F5 — `apply`: stage → validate → flush *(v0.1)*

**Objective:** the transactional rewrite loop — the integration heart (eval-2 §3.4).

1. `src/cli/edit.rs` — `EditsPayload` serde types; `expect_sha` staleness guard (ADR-0014). Tests: malformed payloads → typed errors.
2. `src/orchestrator/apply.rs` — §1.2.1 steps 1–7 end-to-end: validate offsets vs. snapshot, splice **descending by offset**, stage (gen bump), `Tree::edit` + re-parse, range-restricted ERROR/MISSING gate, commit/rollback via `vfs::txn`.
3. Multi-file apply inside one transaction; `--dry-run` (validate + report, no flush, generation restored).
4. `tests/apply_e2e.rs` — happy path (rename an identifier across 2 files); rejection path (edit that breaks syntax → `E_PARSE_INVALID` with node coords, disk untouched); staleness path (`E_STALE_VIEW`).
5. Incremental-vs-full re-parse selection from KP-1 numbers (fast path only if it won).
6. Counting-allocator test asserting **no tree copy (`ts_tree_copy`) happens on the apply path** (eval-1 F5 / eval-2 amendment 1): the exclusivity guard owns; a shared-clone attempt fails to the typed error with zero mutation.

**Gate:** gates + e2e + rollback proof (disk bytes compared pre/post failed apply). **Commit:** `F5: transactional apply with re-parse validation gate`.

### F6 — Memory: `init` / `wake` / `note` / `nap` protocol *(v0.1)*

**Objective:** accumulation and anti-hallucinated persistence (after AST, per the dependency note §1.0).

1. `src/memory/log.rs` — append-only JSONL, fsync, position-derived ids, torn-line quarantine; tests: crash simulation (truncated last line) → quarantine + warning.
2. `src/memory/pointer.rs` — `SyntaxPointer::{Node, Commit}`; validation against the current tree; **re-anchoring** by content-hash near the old span (ADR-0012); `E_POINTER_DANGLING`. Tests: pointer survives an edit that moves it; pointer into a deleted file dangles; commit-hash via `git rev-parse` subprocess (mocked in tests).
3. `src/memory/context.rs` — `ContextManager` global/local layers; `wake(budget)`: chars/4 estimator (ADR-0011), block-tiling recall with present-bias and lock-force-split (§1.5.2), `E_NAP_PENDING` gating, global embedded `share/GLOBAL.md` + XDG override.
4. `src/memory/nap.rs` — the synchronous protocol (§1.5.4): next-pending planner, `--slot/--text` storage into fixed-width `TREE/<size>` records, dense-prefix order enforcement, idempotent resubmission, `ArchitecturalLock` exclusion. **No summarizer in the binary** (ADR-0013).
5. `src/orchestrator/{init,wake,note,nap}.rs`, `src/cli/*` wiring; `.janitor/STATE.json` (linter status for the `wake` payload) — writes atomic via tmp+rename (concurrent agent invocations; ADR-0016).
6. `tests/memory_e2e.rs` — init → notes (one locked, one Node-pointer, one Commit-pointer) → edit the pointed file → note re-anchors → `wake` under a tiny budget truncates, coarsens, and reports; pending compression surfaces.

**Gate:** gates + e2e. **Commit:** `F6: persistent memory with syntax-pointer anti-hallucination and nap protocol`.

### F7 — Hardening & v0.1 release *(v0.1)*

1. `cargo-fuzz` target for the DSL lexer/parser (corpus from golden tests + fixtures) — no crashes on a bounded CI run `[VERIFY: cargo-fuzz setup effort; stretch, droppable]`.
2. Docs: `README.md` (agent-integrator quickstart), `docs/SCHEMA.md` (envelope + match-set reference, ADR-0016). (`docs/DSL.md` moved to F4 — eval-2 amendment 4.)
3. `THIRD_PARTY.toml` generation in CI (cargo-about) — grammar license manifest (§10).
4. musl release artifact + smoke test in CI; CHANGELOG; tag `v0.1.0`.
5. End-to-end agent-loop acceptance test (eval-2 amendment 5): one scripted session through the real binary — `wake → scan → apply → note → wake` — asserting the JSON contract end to end. This, not the release checklist, is v0.1's definition of done.

**Gate:** release checklist. **Commit:** `F7: v0.1.0 release hardening`.

### F8 — `Unify` + duplication scan *(v0.2: planned, not v0.1-funded)*

Merkle fingerprint pass (§1.4.3.6) over the named-node skeleton **plus semantically-significant anonymous tokens**, excluding only comment/whitespace-class trivia kinds (ADR-0010); `Unify` as a post-match filter; `scan --duplicates` grouping by fingerprint; equivalence tests from a curated real-duplicates corpus (ADR-0010 definition made falsifiable). Acceptance tests (eval-1 F2): `x + y` vs `x - y` NOT duplicates; `foo(a, b)` vs `foo(a; b)` NOT duplicates.

*Execution order (v0.2, eval-2 amendment 6):* F9 (multi-language, python) runs **before** F8 (`Unify`) — validate ADR-0002's architecture on a second language before feature depth on one. Phase letter ids F8/F9 stay unchanged everywhere; only execution order swaps.

### F9 — Multi-language breadth *(v0.2)*

Enable + CI-test `lang-python` first (the real test of ADR-0002), then go/typescript; normalized-vocabulary tables (`lang/*.norm.toml`) for global rules; per-grammar kind-name conformance tests fed by KP-5 outputs.

### F10 — Beyond *(unfunded, explicit non-goals until v0.1 proves out)*

LSP server (tower-lsp + tokio enter here, revisiting ADR-0008; refactree's server shape — overlays via `DidOpen`/`DidChange`, last-good snapshot — is the template, A.4) · long-lived daemon / warm parse cache (ADR-0007 disk format) · watch mode · web viewer · gix-based Commit pointers (ADR-0013) · `forget` command · tree-sitter-query import as DSL sugar · refactree-style semantic anchoring (`@ref` hyperlink holes) as a future DSL atom.

---

## 4. Architecture Decision Records

Each ADR: Context · Decision · Alternatives rejected · Consequences. Statuses: `accepted` unless noted (F1 may flip 0010 — that is its job).

**ADR-0001 — Single crate with BRIEF-named module dirs (not a workspace).** *Context:* BRIEF names `src/vfs` … `src/orchestrator`; repo is greenfield. *Decision:* one bin crate; split into a workspace only when incremental build > ~2 min (trigger documented, not vibes). *Rejected:* workspace-per-module (premature structure; the modules share error/envelope types tightly). *Consequences:* simpler CI; grammar compile cost bounded by feature flags instead of crate splits.

**ADR-0002 — Build the DSL evaluator; `NodeType = (LangId, InternedStr)` resolved per language.** *Context:* eval-3 shows `ast-grep-core` (MIT, embeddable via `Doc`/`Content` traits) already delivers ~70% of Module 2, and refactree (MIT, A.4) proves a from-scratch structural engine is shippable. *Decision:* still build our own evaluator: (a) the BRIEF *mandates* the `Under/Seq/Capture/Unify` algebra — `Under` as an explicit state-machine scope restriction is not expressible in ast-grep's matcher combinators; (b) `Unify` across matches (duplicate detection) doesn't exist there; (c) the transactional overlay + `apply` loop has no analogue in ast-grep (refactree has one — but in Go, and without `Under`/`Unify`/memory). Kind matching uses `id_for_node_kind` + interning → integer compares in the hot loop. *Rejected:* embed ast-grep-core (loses the two core novelties; deep coupling to its `Doc` model); adopt tree-sitter queries (no `Under`/`Unify` semantics). *Revisit trigger:* if F4 slips >50% over estimate. *Consequences:* we own matcher correctness (KP-7 linearity bench is the price of admission); eval-3's verdict lands here. *Eval-3 C2 disposition (2026-09-13):* its proposed two-way spike (hand evaluator vs `ast-grep-core` bridge, ~2–3 d) is deliberately **not** scheduled before F4 — at v0.1's one-language scope the net saving is 1.5–2.5 lane-weeks against permanent 0.x coupling and fixed matcher semantics; the revisit trigger above (F4 slips >50%) remains the flip point, and `ffi_shim.rs` is the seam that bounds the swap. Decision: trigger-based, not spike-based.

**ADR-0003 — DSL surface syntax: s-expressions.** *Context:* BRIEF asks s-expr vs. other; refactree chose a token-stream dialect, ast-grep chose "write the code you want to match" + YAML rules. *Decision:* s-expressions — 1:1 with the internal algebra, ~300-line recursive-descent parser (no dependency), familiar to tree-sitter users (their query language is s-expr shaped), trivially pretty-printable in JSON errors, and indentation-free (LLM generation robustness). *Rejected:* tree-sitter query syntax (semantics we need don't fit; a converter is a plausible post-v0.1 on-ramp); YAML rules (verbose, indentation-fragile for LLM generation); token-stream dialect (refactree shows it works, but it matches tokens, not CST structure — `Under` scoping is inherently structural). *Consequences:* agents need zero YAML/indentation correctness; DSL docs are one page.

**ADR-0004 — `Seq(Vec<NodeId>)` means pattern-element ids.** *Context:* BRIEF's sketch names `NodeId` inside `Seq`, which reads like tree node ids — impossible before matching exists. *Decision:* interpret as *pattern* node ids (positions in the sequence); elaborated to `Seq(Vec<Pattern>)`. *Rejected:* two-phase symbol-table-of-subpatterns reading (equivalent power, more ceremony). *Consequences:* recorded explicitly so eval-1/eval-2 can challenge the interpretation, not discover it.

**ADR-0005 — Evaluator = compiled scoped NFA with memoized failure.** *Context:* the BRIEF's anti-catastrophic-backtracking claim needs a mechanism, not hope. *Decision:* compile per language to flat instructions; `Under` enters a scope; `Seq` runs as NFA simulation over the sibling vector with an `(index, pc)` failure memo — no instruction can revisit a failed `(index, pc)`, so per-scope cost is `O(children × states)`; global cost `O(Σ scopes)`. Independent precedent: refactree's matcher is a "token stream + pattern NFA" (A.4). *Rejected:* backtracking interpreter with memo cache (same bound, worse constants, harder to reason); pure tree-sitter queries (again). *Consequences:* KP-7 benchmark is the falsifiability contract; alternation-heavy adversarial patterns are a CI bench case. *v0.1 scope note (ADR-0016):* `Unify` is not executable in v0.1 (it is F8/v0.2) — a rule containing `unify` fails rule-compile with a versioned `E_UNSUPPORTED_VERSION`, never a silent no-match (eval-2 §5.5.3). Capture priority and canonical match-set order are fixed in ADR-0016.

**ADR-0006 — LOG.txt is JSONL; TREE/* is fixed-width records.** *Context:* OptMem uses fixed 320 B log records + 288 B summary records for O(1) seeks at 1M-memory scale (verified, A.2). *Decision:* our raw log is JSONL (facts carry structured `SyntaxPointer`s that don't fit fixed width; local layer is repo-scale where full loads are cheap; ids stay position-derived; torn-line quarantine replaces torn-record repair). Summary **tree** files stay fixed-width (256 B one-liners) keeping OptMem's O(1) seek and dense-prefix frontier semantics. *Rejected:* fixed-width everything (pointer fields truncate); JSONL tree files (variable-width kills the seek-by-slot property). *Consequences:* two record formats, each doing what it is good at; both documented in `docs/SCHEMA.md`.

**ADR-0007 — Parse cache is in-process, generation-keyed, streaming in v0.1.** *Context:* BRIEF deliverable asks where the parse-cache index lives. *Decision:* in-process dashmap keyed by `VfsPath`, entries `{gen, bytes Arc, tree, error_count}`, validated by generation + content hash (Rule T4). **Retention discipline (eval-1 F1):** v0.1 `scan` is streaming — each `ParseEntry` is dropped after that file's report is collected; the process never retains O(repo) bytes+trees (single-rule cold scans have a zero cache hit rate; retention is pure cost and an OOM vector). The dashmap pays off only for multi-rule scans / warm processes (F10). **Apply-path ownership (eval-2 amendment 1):** `apply` REMOVES the `ParseEntry` from the cache and takes ownership BEFORE `Tree::edit`, asserting `Arc::strong_count(&tree) == 1` at the edit point (typed error otherwise — never edit a shared tree, per A.3.2's aliasing rule); a fresh entry is inserted after re-parse. Disk cache path reserved (`.janitor/cache/parse/<sha256(lang||content)>`) but unwritten in v0.1 — cold parse per invocation is the accepted cost, **measured** by KP-10 in F1 (rayon mitigates; the daemon/LSP in F10 is the real fix). *Precedent:* refactree — on-demand walks, in-process per-file cache, explicitly no persistent index, cache cleared when overlay content changes (A.4); our generation keys make stale-serving impossible by construction. *Consequences:* `scan` latency on huge repos is a known v0.1 wart, listed in §8 (R15) and converted to a measured decision by KP-10.

**ADR-0008 — No async runtime in v0.1.** *Context:* BRIEF says "tokio se justificado" (if justified). *Decision:* not justified: CLI is short-lived; nap is synchronous by design (ADR-0013); the only async-shaped consumer is LSP (F10). *Consequences:* smaller static binary, simpler CI; revisited at F10 with `tower-lsp` (which will pull tokio).

**ADR-0009 — Overlay buffer entries are `{File(Arc<[u8]>) | Tombstone}`.** *Context:* BRIEF sketches `HashMap<PathBuf, Vec<u8>>`, but its own delete-invariant ("read_file after delete = miss/erro, mas read_dir não muda") is unsatisfiable with entry-removal semantics (removal falls through to disk). *Decision:* tombstones shadow the base; `Arc<[u8]>` makes snapshots O(1) (feeds Rules T2/T4 and the guard discipline of §1.1). *Consequences:* delete-visible-to-read_file/invisible-to-read_dir falls out; rollback keeps the prior Arc alive for free.

**ADR-0010 — Trivia semantics (pinned by KP-6, decided before the evaluator exists).** *Context:* eval-2 §6.2/§6.3: byte-strict adjacency/isomorphism misses most real code (comments between siblings); trivia-insensitive matching is a *different* semantics that must be explicit. *Decision:* `Seq` adjacency = next **named** sibling skipping comment-class kinds (per-grammar trivia set from KP-5); `Unify` equivalence = named-node skeleton + leaf token texts **plus semantically-significant anonymous tokens (binary operators, separators, punctuation)**, excluding **only** comment/whitespace-class trivia kinds per grammar (KP-5's kind dump classifies them) — excluding *all* anonymous nodes conflates `a + b` with `a - b`, because operators/separators are anonymous tokens (eval-1 F2, folded 2026-09-13); `--strict-trivia` restores byte-strict for both. *Consequences:* ADR text updates with KP-6 corpus outcomes; the equivalence definition is what F8's duplicate-detection tests encode — acceptance includes `x + y` vs `x - y` NOT duplicates and `foo(a, b)` vs `foo(a; b)` NOT duplicates.

**ADR-0011 — Token estimation = ceil(bytes/4), declared in output.** *Context:* eval-2 §3.3: "tokens of which tokenizer?" is an ADR, not a discovery. *Decision:* chars/4 heuristic, `"estimator":"chars4"` field in `wake` output; no tokenizer dependency (offline, static, model-agnostic — the consuming model varies; OptMem doesn't tokenize at all, it budgets lines). *Consequences:* budgets are approximate by construction and say so in-band.

**ADR-0012 — SyntaxPointer = span + kind + content-hash with re-anchoring (not node ids).** *Context:* eval-2 §6.4, the strongest memory risk: tree node ids dangle after any re-parse by design. *Decision:* pointers carry `{path, span, kind_name, content_sha}`; `note` resolves against the current tree (exact hit → ok; content-hash match near old span → re-anchor + record drift; neither → `E_POINTER_DANGLING`). *Consequences:* facts stay anchored across edits that move them; deletion dangles loudly (correct); adds a re-anchor search cost bounded by "near the old span".

**ADR-0013 — `nap` summarization is owned by the consuming agent, synchronously; Commit pointers via git subprocess.** *Context:* eval-2 §3.3 "who writes the summaries?" — unbounded scope in every built-in answer. OptMem (verified, A.2) settles it: the LLM writes each summary as a CLI argument, one compression at a time, sequenced by the tool; "Nothing ever runs in the background." *Decision:* adopt exactly that (§1.5.4): `nap` plans and validates, the agent summarizes, `wake` gates on completeness. This deliberately deviates from the BRIEF's "rotina background" wording — a background routine without an LLM can only produce heuristic summaries (unbounded, eval-2 option b) or call an API (breaks the offline static posture, option c). Also here: `Commit { sha }` validation shells out to `git rev-parse` (no git2/gix in v0.1; gix at F10). *Note:* OptMem has **no license** (verified, A.2) — we adopt *ideas and formats* (not copyrightable as such) and copy **zero code**; credits in NOTICE (§10). *Consequences:* nap ships as protocol + format in v0.1 (eval-2 cut); needs stakeholder sign-off on the "background" deviation (§9 Q4).

**ADR-0014 — Edits serialize as half-open byte-offset JSON with a staleness hash, applied descending.** *Context:* BRIEF deliverable: "como serializar Edits no apply". *Decision:* `{start_byte, old_end_byte, new_text}` per edit (half-open `[start, old_end)` UTF-8 byte ranges — refactree's span model, A.4); per-file optional `expect_sha` of current content; sorted + non-overlapping enforced; multi-edit splice applies **descending by offset** so earlier offsets stay valid (refactree's ordering); `E_STALE_VIEW`/`E_EDIT_MISMATCH`/`E_EDIT_OVERLAP` typed errors. *Rejected:* tree-diff/patch formats (ask agents to produce diffs against exact trees — hallucination bait; byte edits against verified content are the most agent-proof). *Consequences:* the payload is dumb on purpose; all cleverness lives in re-parse validation (§1.2.1).

**ADR-0015 — Grammars: one cargo feature per language, `lang-rust` default, `lang-all` in CI only.** *Context:* grammars are C; each adds compile time and binary size; the grammar/core matrix is ABI-governed (verified: all our grammars ABI 14–15, core 0.27 accepts 13..=15, and grammar crates don't depend on core at runtime — A.3). *Decision:* features per language; default = rust; CI builds `lang-all` as a fence against future releases shifting the matrix; all tree-sitter API touchpoints isolated in `ffi_shim.rs` so a pin-swap is one file. *Consequences:* users build the languages they serve; the matrix risk is caught at CI time, not user time. *Note (eval-1 F7):* `tree-sitter-language` declares `links = "tree-sitter-language"` — a future semver-incompatible second version pulled into the graph (e.g. by a grammar fork or vendoring) is a **hard cargo error**, not a warning; future grammar additions must keep the `^0.1` line unified.

**ADR-0016 — Match-set semantics: local maxima per pivot, flagged overlap, canonical order, deterministic across thread counts.** *Context:* eval-1 F3 + eval-2 §5.5.1/§5.5.3/§5.5.4/§5.5.5/§5.5.6: the plan fixed the `MatchReport` shape and edit ordering but never the *match set* — nesting, overlap, dedup, output order, and determinism under rayon; for an LLM consumer an under-specified match set is indistinguishable from a bug. *Decision:* (1) matches are all **local maxima per pivot**; overlapping matches from different pivots are both reported, flagged `"overlap": true`; (2) canonical output order `(start_byte, end_byte, rule_id)` — collection is `par_iter().collect()` then this sort; (3) determinism is enforced by a CI test asserting **byte-identical JSON under `RAYON_NUM_THREADS=1` and `=8`**; (4) capture priority: when multiple accepting paths bind the same symbol, **earliest-position wins** (Pike-VM-style thread priority; eval-2 §5.5.1); (5) `--include-virtual` defaults to **OFF** — anti-phantom philosophy: `scan` reports disk reality, the flag opts into overlay visibility (§1.3's union applies only when set); (6) `Unify` is stubbed in v0.1: a `unify` rule fails rule-compile with versioned `E_UNSUPPORTED_VERSION`, never a silent no-match (see ADR-0005); (7) dashmap×rayon insertion rule: phase-split snapshot-then-parse per §1.1 — workers never hold shard guards across a `par_iter` join; cache inserts happen only at collection; (8) `.janitor/STATE.json` writes are atomic via tmp+rename (concurrent agent invocations cannot interleave). *Rejected:* per-pivot greedy first-match only (drops real overlaps agents need to see). *Consequences:* `docs/SCHEMA.md` documents the match set, overlap flag, canonical order, and capture priority from day one (R12).

---

## 5. Kill-points (cheap, early, falsifiable)

Principle (eval-2 §4.1): every KP runs **before** the phase whose architecture it can invalidate. Three were resolved by desk research (verified sources in Appendix A) and remain only as regression fences. KP-11 is the deliberate exception — the fusion can only be dogfooded once v0.1 exists, so it gates post-v0.1 funding rather than a phase (eval-3 C4).

| # | Hypothesis under attack | Experiment | Cost | Status / if it dies | Phase |
|---|---|---|---|---|
| KP-1 | Incremental `Tree::edit`+re-parse over an `Arc<[u8]>` overlay snapshot beats full re-parse and is byte-correct — and the edit path is sound only under exclusive ownership | 3 MB generated file, 6 edit sizes, correctness diff + timings; **shared-clone leg**: attempt the edit path while a clone exists — assert typed error, no mutation (eval-2 amendment 3) | ~0.5 d | **open** — if slower/wrong: drop `Tree::edit`, always full-parse; architecture unchanged | F1.1 |
| KP-2 | `Tree`/`Parser` are `Send`/`Sync`; owned `Arc<Tree>` + plain-data results is a workable rayon pattern | Compile-time canaries + mini rayon scan | ~0.25 d | **resolved by docs** (Send+Sync verified, A.3); canary stays as regression fence; fallback path documented | F1.2 |
| KP-3 | `Point` column semantics are knowable and testable for multibyte sources | Emoji/CJK corpus, `InputEdit` round-trip | ~0.25 d | **resolved by docs**: column = bytes (A.3); test confirms; byte-column math in `ffi_shim` | F1.3 |
| KP-4 | "No ERROR/MISSING nodes in edited ranges" is an honest validity gate (localization holds) | 10-file mangled corpus; measure false negatives | ~0.5 d | **open** — if localization is poor: widen the gate (whole-file) or add balance pre-check; report the measured rate either way | F1.6 |
| KP-5 | The grammar crate set (ABI 14–15) loads against core 0.27 (ABI 13..=15) | CI pin-check build of `lang-all` + kind dump | ~0.25 d | **resolved by evidence** (parser.c reads + dependency graph, A.3); CI fence remains | F0.6 / F1.5 |
| KP-6 | Comment-skipping adjacency matches real-world intent (5 curated patterns) | Corpus run: byte-strict vs. trivia-skip | ~0.5 d | **open** — if corpus disagrees: ADR-0010 flips to per-`Seq` explicit trivia spec (more DSL surface, documented) | F1.4 |
| KP-7 | The scoped NFA is ~linear on adversarial patterns/trees | Criterion bench vs. naive backtracking baseline | ~1 d (in F4) | **open** — memo key gains pattern-position hash; worst case documented as rule-author constraint; bench stays as fence | F4.8 |
| KP-8 | Traversal and the matcher are allocation-free per visited node | Counting-allocator test in CI, covering F3 walk helpers **and** the F4 matcher (`exec.rs`) — fence re-armed onto the matcher (eval-2 amendment 7) | ~0.5 d (F3.6 + F4) | **open** — budget allowance documented + alloc sites minimized (smallvec pool) | F3.6/F4.5 |
| KP-9 | LLM agents can author correct DSL rules from `docs/DSL.md` alone — the product's actual user, otherwise untested until after release (eval-2 §5.3.1) | At F4 entry: a real LLM authors N rules from the docs alone; measure syntax-error and retry rates (typed errors + suggestions are the product surface) | ~0.5 d | **open** — if authoring fails: iterate the surface syntax *before* evaluator semantics freeze (docs drafted at F4, item F4.11) | F4 entry |
| KP-10 | Cold-parse `scan` latency on a real repo is acceptable (ADR-0007's accepted wart, asserted but never measured — eval-2 §5.3.2) | Scan a large real repo (e.g. tree-sitter itself or a rust-analyzer checkout), cold parse; measure wall time and RAM ceiling | ~0.5 d | **open** — if unacceptable: daemon/warm cache moves up in priority (a measured decision, not opinion) | F1.7 |
| KP-11 | The fusion (staged+validated edits + anchored memory) measurably beats git-worktree + ast-grep for agents (eval-3 C4) | Post-v0.1 dogfood (~1 wk): agent runs `wake → scan → apply → note` on a real repo vs git-worktree + ast-grep baseline; measure broken edits / context loss | ~1 wk | **open** — no measurable reduction ⇒ the project stops at v0.1 (funding gate) | post-v0.1 (funding gate) |

---

## 6. Effort estimate (honest, cross-checked)

Unit: focused working days of one senior Rust engineer (knows tree-sitter exists; has not shipped a tree-sitter product — eval-2's calibration). Ranges are p50→p90. Calendar ≈ ×1.3–1.5.

| Phase | Scope | p50 | p90 |
|---|---|---|---|
| F0 | scaffold, CI, pin-check | 2 | 4 |
| F1 | kill-point spikes + KP-10 latency leg (3 of 11 pre-resolved by research) | 3 | 6 |
| F2 | VFS + invariants + txn | 5 | 7 |
| F3 | parse layer + cache | 6 | 9 |
| F4 | DSL + evaluator + scan | 10 | 16 |
| F5 | apply loop | 5 | 8 |
| F6 | memory (LOG/wake/note/nap-protocol) | 6 | 10 |
| F7 | hardening + release | 3 | 6 |
| **v0.1 total** | | **40** | **66** |
| F8 | Unify + duplicates (v0.2) | 4 | 7 |
| F9 | multi-language (v0.2) | 4 | 8 |
| **Full BRIEF** | | **48** | **81** |

**Cross-check vs. eval-2's independent model** (their §3.6: 42–70 days full scope; VFS 5–8, AST 20–35, memory 6–10, CLI 9–14, scaffold 2–3): their original model matched the draft's 42–69 almost exactly; after reconciliation the plan **adopts eval-2's §5.1 re-estimate** (eval-2 amendment 8) — v0.1 **40–66**, full BRIEF **48–81**, delta concentrated in F4 (10–16: capture-priority spec, naive-baseline authoring, error-message polish) and F6 (6–10: tiling recall, re-anchor search, TREE planner, `GLOBAL.md` content). The phase split still differs (spikes front-loaded, CLI wiring folded into feature phases); the totals are eval-2's. v0.1 remains below full scope *because* v0.1 is the eval-2-shaped cut: one language, no `Unify`, protocol-only nap.

**Calendar:** v0.1 ≈ 2.5–4 months solo; full BRIEF ≈ 3–5 months. Adding engineers does not compress F3–F5 materially (the matcher/apply design is serial, per eval-2 §3.6); F2 and F6 can overlap with F3 if — and only if — the `ProjectFs`/`ParseEntry` interfaces are frozen at F2 exit.

**Known optimism risks in my own numbers:** DSL error-message quality (product surface, endless polish — capped by "good enough for agent retry" in F4), and the no-panic rule's ongoing tax (~5–10% on lock/FFI code, priced per eval-2 §2). Research already removed three spikes' worth of uncertainty from F1; if KP-1 or KP-6 go badly, add ~1–2 d each to F3/F4 respectively (contingency not hidden in the base numbers).

---

## 7. MVP cut list (v0.1)

**Principle** (eval-2 §7): v0.1 must demonstrate the unique claim — deterministic structural refactor over a transactional overlay, consumable by an agent via JSON. Everything serving humans, convenience, or longevity waits.

**Out of v0.1:**
- **LSP** — entirely. The declared consumer is an LLM agent reading JSON; LSP adds JSON-RPC lifecycle + client-compat matrix for a user that doesn't exist yet (F10; refactree's server is the template when we get there).
- **Watch mode** — file-notify + debounce + incremental state; orthogonal to the value prop, multiplies test surface (F10).
- **Web viewer** (refactree has one — SPA + GraphQL) — humans-later (F10).
- **Built-in `nap` summarizer** — protocol + format only (ADR-0013); the agent summarizes.
- **`Unify` / duplicate detection** — F8; naive isomorphism would be 2 d but trivia-correct equivalence is the whole point (eval-2 §7).
- **Multi-language** — grammars ship as features (ABI-verified compatible, §2), but only `lang-rust` is CI-tested and supported in v0.1; python at F9 first.
- **Warm/disk parse cache, daemon** — cold parse per invocation accepted (ADR-0007).
- **Cross-file flush journal** — `E_FLUSH_PARTIAL` honest reporting instead (§1.3).
- **gix / libgit2** — shell out to `git` (ADR-0013).
- **tokio/async** — ADR-0008.
- **Windows** — linux (musl + gnu) + macOS CI; std::path keeps the door open, unsupported in v0.1.
- **`forget`, TS-query import sugar, semantic `@ref` anchors (refactree-style), normalized-vocab beyond the minimal table** — F10.

**In v0.1:** full VFS with the four invariant tests; DSL (`Under`/`Seq`/`Capture`) with defined trivia semantics; `scan` (rayon) + `apply` (re-parse gate, txn flush); `init`/`wake`/`note` + nap protocol; versioned JSON envelope; CI gates incl. clippy no-unwrap and musl.

**Cut-to-zero test** (eval-2): with only the "in" list, can an agent discover → stage → validate → flush → persist a refactor and recall it next session with anti-hallucinated anchors? Yes — every cut survives it. Whether that ability measurably beats the git-worktree + ast-grep baseline is KP-11's question — the post-v0.1 dogfood/funding gate (eval-3 C4).

---

## 8. Risks & mitigations

New risks (not the BRIEF's own list), folded with eval-2's §6 where our mitigations differ. Severity (S) × Likelihood (L), 1–5.

| # | Risk | S | L | Mitigation | Owner phase |
|---|---|---|---|---|---|
| R1 | Future grammar/core releases shift the ABI matrix (current set verified compatible, A.3) | 3 | 2 | CI `lang-all` pin-check fence; `=` version pins; `ffi_shim.rs` isolation | F0+ |
| R2 | A future tree-sitter release narrows `Send`/`Sync` impls | 3 | 1 | Compile-time canaries (verified true for 0.27); fused parse+match fallback pre-designed | F1/F3 |
| R3 | Incremental `Tree::edit` over snapshots incorrect or not faster | 3 | 3 | KP-1; drop to full re-parse; architecture unaffected | F1/F5 |
| R4 | `Seq` trivia semantics wrong → real patterns unmatchable (eval-2 §6.2) | 4 | 3 | KP-6 corpus decides ADR-0010 *before* evaluator build; `--strict-trivia` escape | F1/F4 |
| R5 | `Unify` trivia sensitivity misses real duplicates (eval-2 §6.3) | 3 | 3 | ADR-0010 equivalence definition + real-duplicates corpus at F8 | F8 |
| R6 | SyntaxPointer dangling across edits (eval-2 §6.4) | 4 | 3 | ADR-0012 span+content-hash+re-anchor; dangle = loud error | F6 |
| R7 | Validation gate dishonest (error-recovery false negatives) (eval-2 §6.5) | 4 | 3 | KP-4 measures localization honesty; measured rate documented; whole-file fallback mode | F1/F5 |
| R8 | Anti-phantom vs. scan invisibility conflict (eval-2 §6.6) | 3 | 4 | `virtual_paths()` explicit union in scan (§1.3) | F2/F4 |
| R9 | nap scope creep / background-without-LLM trap (eval-2 §3.3) | 3 | 3 | ADR-0013 synchronous protocol; stakeholder sign-off Q4 | F6 |
| R10 | Git dependency vs static binary (eval-2 §6.7) | 2 | 3 | subprocess `git rev-parse`; gix at F10 | F6 |
| R11 | Cross-language "generic rules" overpromise (eval-2 §6.8) | 2 | 4 | Minimal normalized vocab v0.1; richer = F9 with tests | F6/F9 |
| R12 | JSON schema churn breaks agent consumers (eval-2 §6.9) | 4 | 2 | `schema:"janitor.v1"` envelope + `docs/SCHEMA.md` day one | F0 |
| R13 | Grammar license spread in later versions (eval-2 §6.10) | 2 | 2 | THIRD_PARTY manifest in CI (cargo-about); all 7 initial grammars MIT-family | F0/F7 |
| R14 | Overlay lock contention under rayon (eval-2 §6.11) | 3 | 3 | `Arc<[u8]>` snapshots + guard discipline + dashmap cache; concurrency test F2.8 | F2 |
| R15 | Cold parse per invocation = slow `scan` on big repos (and, unmitigated, O(repo) RAM retention — eval-1 F1) | 3 | 4 | Streaming scan: each ParseEntry dropped after its file's report (ADR-0007 — no O(repo) retention/OOM vector); rayon; KP-10 measures real-repo latency+RAM in F1; daemon at F10 | F1/F4 |
| R16 | DSL hallucination by agent users → garbage rules | 3 | 4 | Typed errors with suggestions, golden corpus + fixtures, fuzz target, `docs/DSL.md` | F4/F7 |
| R17 | Torn writes corrupt LOG/TREE | 2 | 2 | JSONL quarantine + fixed-width dense-prefix records (torn tail = detectable) | F6 |
| R18 | musl/cc toolchain flakiness in CI | 2 | 3 | zigbuild-or-musl-gcc decision at F0; smoke-test artifact | F0 |
| R19 | Editing a tree readers still reference (eval-2 §5.5.8) | 4 | 1 | Exclusivity guard: apply removes the `ParseEntry` and asserts `Arc::strong_count == 1` before `Tree::edit`; typed error otherwise (never edit a shared tree — A.3.2 aliasing rule); KP-1 shared-clone leg; F5 counting-allocator assertion | F1/F5 |
| R20 | Merkle hash collision false-positive duplicates (F8) | 2 | 1 | 128-bit SipHash; corpus verify; second-pass structural confirm on hit | F8 |
| R21 | OptMem has no license — accidental code copying | 5 | 1 | Ideas/formats only, zero code; NOTICE credit; review gate on memory-module PRs | F6 |
| R22 | tree-sitter minor-version churn breaks API surface (`parse_with` removal, `child_count: usize→u32` both happened within 0.25→0.27) | 3 | 3 | `=` exact pins; all API touchpoints in `ffi_shim.rs`; upgrades are deliberate one-file PRs | F0+ |

---

## 9. Open questions (triaged)

**Blockers** (resolve before/inside the named phase):

- **Q1** — Is incremental re-parse over snapshots worth it? *(KP-1, F1 — decides the apply fast path; not researchable, must be measured)*
- **Q2** — Trivia semantics for `Seq`/`Unify`? *(KP-6, F1 — decides ADR-0010 and every evaluator test; corpus experiment, not a poll)*
- **Q3** — Is the localized ERROR/MISSING gate honest enough? *(KP-4, F1 — if not, the apply gate widens and F5 tests change)*
- **Q4** — Stakeholder sign-off on nap-as-synchronous-protocol (deviation from the BRIEF's "background routine" wording). *(ADR-0013; product call, not technical — ask once, in writing, before F6)*
- **Q5** — Legal posture on unlicensed inspiration (OptMem): ideas-only stance acceptable to the project owner? *(§10; ask at F0 alongside license filing)*

**Resolved during planning** (kept here for the record): grammar×core matrix — ABI evidence, all compatible (A.3, was Q1) · `Tree`/`Parser` `Send`/`Sync` — verified true (A.3, was Q2) · `Point` column units — bytes (A.3, was Q5) · `LanguageFn` constructor — `tree_sitter_rust::LANGUAGE.into()` (A.3) · refactree license — MIT (A.4).

**Noise** (decided by default, revisitable, must not block): binary name (`janitor`) · `--pretty` default off · TREE record width 256 vs 288 · chars/4 constant · `.janitor/` dir name · feature naming (`lang-*`) · whether `wake` embeds OptMem's "subagents must not run the memory tool" guidance (default: include a `hints` field, agents free to ignore) · typescript grammar covers TSX or not (flag it in `lang-typescript` docs) · colors in `--pretty`.

---

## 10. OSS hygiene from day 1

- **License:** dual `MIT OR Apache-2.0` (Rust ecosystem convention), `LICENSE-MIT`, `LICENSE-APACHE`. Filed at F0 (Q5).
- **Credits (NOTICE + README):** explicit inspiration credit to **OptMem** (Victor Taelin) for the memory model — *ideas and formats only; OptMem ships no license, zero code copied, no derivative assets*; to **refactree** (lucasew, MIT per its pkg.go.dev module snapshot — the GitHub repo is **deleted**: GitHub API 404, confirmed 2026-09-13; cite the pkg.go.dev / proxy.golang.org archive, A.4) for the ingest→pattern→match→edits pipeline shape and the overlay-staged-commit discipline; to tree-sitter and grammar authors (MIT; THIRD_PARTY manifest via cargo-about in CI — R13).
- **Secrets:** none, structurally — the binary makes **zero network calls** in v0.1 (assert-able: no network-capable deps in the graph; the `git` subprocess is local). `.janitor/` is gitignored by default with a documented opt-in for teams that *want* repo-shared memory.
- **Telemetry:** none, ever; `--pretty` and `tracing` (stderr, env-gated) are the only human affordances.
- **CI supply chain:** GitHub Actions pinned by commit SHA; `cargo-deny` (licenses + advisories) and `cargo-audit` in CI; Dependabot with grouped updates.
- **Repo hygiene:** `CHANGELOG.md`, `docs/SCHEMA.md` + `docs/DSL.md` as contract docs shipped with v0.1; contributions under the same dual license; no CLA.
- **Data hygiene:** memory logs stay local to the scanned repo (`$PWD/.janitor/`); global layer reads only `$XDG_CONFIG_HOME`; documented so agent integrators can reason about what leaves the machine (nothing).

---

## Appendix A — Verified research facts

### A.1 Crate versions (verified via crates.io API, 2026-09-13)

| Crate | Latest stable | Released | Source |
|---|---|---|---|
| clap | 4.6.6 (derive = feature, not default; MSRV 1.85) | 2026-08 | crates.io/api/v1/crates/clap |
| rayon | 1.12.0 | 2026-04 | crates.io/api/v1/crates/rayon |
| serde | 1.0.229 | 2026-07 | crates.io/api/v1/crates/serde |
| serde_json | 1.0.151 | 2026-07 | crates.io/api/v1/crates/serde_json |
| thiserror | 2.0.20 (last 1.x: 1.0.69) | 2026-08 | crates.io/api/v1/crates/thiserror |
| tokio | 1.53.1 (MSRV 1.71, rolling 6-month policy) — *not adopted, ADR-0008* | 2026-07 | crates.io/api/v1/crates/tokio |
| smol-str | 0.3.6 | 2026-03 | crates.io/api/v1/crates/smol-str |
| parking_lot | 0.12.5 — *not adopted* | 2025-10 | crates.io/api/v1/crates/parking_lot |
| dashmap | 6.2.1 | 2026-05 | crates.io/api/v1/crates/dashmap |
| tracing | 0.1.44 | 2025-12 | crates.io/api/v1/crates/tracing |
| tracing-subscriber | 0.3.23 | 2026-03 | crates.io/api/v1/crates/tracing-subscriber |
| tempfile | 3.27.0 | 2026-03 | crates.io/api/v1/crates/tempfile |
| proptest | 1.8.2 | 2026-02 | crates.io/api/v1/crates/proptest |
| criterion | 0.11.0 | 2026-03 | crates.io/api/v1/crates/criterion |
| git2 | 0.21.0 — *not adopted, ADR-0013* | 2026-05 | crates.io/api/v1/crates/git2 |
| tree-sitter | 0.27.0 (prev lines: 0.26.13, 0.25.10; 0.22.0 yanked) | 2026-08-30 | crates.io/api/v1/crates/tree-sitter |
| tree-sitter-language | 0.1.8 (first published 2024-08-26) | 2026-08-30 | crates.io/api/v1/crates/tree-sitter-language |
| tree-sitter-rust | 0.24.2 (ABI 15) | 2026-03 | crates.io + parser.c (tagged) |
| tree-sitter-python | 0.25.0 (ABI 15) | 2025-09 | same |
| tree-sitter-go | 0.25.0 (ABI 15) | 2025-08 | same |
| tree-sitter-javascript | 0.25.0 (ABI 15) | 2025-09 | same |
| tree-sitter-typescript | 0.23.2 (ABI 14) | 2024-11 | same |
| tree-sitter-java | 0.23.5 (ABI 14) | 2024-12 | same |
| tree-sitter-c | 0.24.2 (ABI 15) | 2026-04 | same |

### A.2 OptMem (verified 2026-09-13; README + `memo` + `test.py` + `install.sh` via raw.githubusercontent.com; api.github.com for metadata)

Key verified facts feeding §1.5 and ADR-0006/0013: single-file Python tool, **no license** (`license: null`); prompt block for `AGENTS.md`/`CLAUDE.md` (~426 tokens claimed); `wake` before every session; `note` = one line ≤ 280 bytes; **LOG.txt fixed 320-byte records, position IS identity** (`memory i lives at i*320`), append-only with advisory lock + fsync, torn tail auto-repaired; **wake** = oldest-first interleaving of raw entries and summaries, power-of-two block tiling, block kept whole iff size ≤ α×age, leftover budget splits the largest block ("spend what is left on the present"), budget in *lines* (96 ≈ 8k tokens), **refuses with a pending-compression instruction if a needed summary is missing**; **nap**: the agent writes each summary via `nap <lo>-<hi> "<line>"`, blocks built in order, `TREE/<size>` files are 288-byte fixed records, dense prefix per level = build frontier, idempotent resubmission, TREE rebuildable from LOG (source of truth); "Nothing ever runs in the background."; `zoom <a-b>` descends the two halves; `forget` truncates tree levels (LOG untouched); **no pointers/anchors of any kind** (anti-hallucination is prompt wording only) — janitor's `SyntaxPointer` is our addition per the BRIEF; storage is global per user (`~/.optmem/memory`, `$MEMORY_DIR` override); 1,520 stars as of research date.

### A.3 tree-sitter Rust API (verified 2026-09-13 against v0.27.0 sources, docs.rs 0.27.0, tagged parser.c files, release notes)

1. **`Tree: Send + Sync`** (unconditional `unsafe impl`s, lib.rs v0.27.0); `Parser: Send + Sync` (non-wasm); `Node<'_>`, `TreeCursor<'_>`, `Query`, `Language` also Send+Sync. — lib.rs, docs.rs struct pages.
2. **`Tree::clone` = `ts_tree_copy` = shallow, refcount-incrementing, "very fast"** (api.h docs); a single TSTree must not be mutated while clones exist — concurrent reads are the blessed shared pattern; docs explicitly say clone per thread for simultaneous *use*. — lib.rs, api.h, advanced-parsing docs.
3. **`parse_with` deprecated in 0.25, removed in 0.26.** Current: `parse_with_options<T: AsRef<[u8]>, F: FnMut(usize, Point) -> T>(&mut self, callback: &mut F, old_tree: Option<&Tree>, options: Option<ParseOptions<'_>>) -> Option<Tree>`. Callback receives `(byte_offset, position)`, returns a chunk starting at that offset; empty slice = EOF; owned `Vec` chunks allowed. `ParseOptions.progress_callback: &mut dyn FnMut(&ParseState) -> ControlFlow<()>` aborts the parse on `Break` (`ParseState::current_byte_offset()`/`has_error()`); after an aborted parse, call `Parser::reset()` to abandon. UTF-16 variants exist (`parse_utf16_le/be_with_options`, code-unit columns). — lib.rs v0.27.0, docs.rs.
4. **`InputEdit`** = `{start_byte, old_end_byte, new_end_byte, start_position, old_end_position, new_end_position}`; **`Point.column` is in bytes** (rows = newline count; standardized by the 0.25.0 UTF-16 handling fix, #3740). — lib.rs, basic-parsing docs, v0.25.0 release notes.
5. **`Node<'tree>`**: `#[derive(Clone, Copy)]`, `PhantomData<&'tree ()>`; `kind() -> &'tree str` (borrowed, no alloc); `kind_id() -> u16` (also `grammar_id()`, `id() -> usize`); `byte_range() -> Range<usize>`; `is_error()`/`has_error()`/`is_missing()`; **`child_count() -> u32` in 0.27 (breaking; `usize` in ≤0.26)**; `children(&mut TreeCursor) -> impl ExactSizeIterator` (**caller-owned reusable cursor**); `walk()`, `utf8_text(&[u8])`. — lib.rs, docs.rs, v0.27.0 release notes.
6. **`TreeCursor`**: `goto_first_child/goto_next_sibling/goto_parent` (+ reverse ops, documented slower), `node()`, `field_name() -> Option<&'tree str>`, **`reset(node)` and `reset_to(cursor)` exist**; `Clone`; cursor internals heap-allocate their stack — hence the reuse APIs. — lib.rs, docs.rs, tree_cursor.c, api.h.
7. **`tree-sitter-language`**: all 7 grammar crates use it as their **only runtime dependency** (`tree-sitter` core is a dev-dependency in every one) — no core-version constraint is expressible at build time. Grammar crates now expose `pub const LANGUAGE: LanguageFn` (`tree_sitter_rust::LANGUAGE`); the old `language()` fn is gone; usage: `parser.set_language(&tree_sitter_rust::LANGUAGE.into())`. — crates.io dependency endpoints, tree-sitter-rust v0.24.2 bindings, docs.rs LanguageFn.
8. **ABI matrix**: core 0.27.0 `LANGUAGE_VERSION = 15`, `MIN_COMPATIBLE_LANGUAGE_VERSION = 13`, `set_language` accepts 13..=15 inclusive. Grammar `parser.c` reads: rust/python/javascript/go/c = **15**, typescript/java = **14** — **all within core 0.27's range**. — api.h, abi-versions docs, per-grammar tagged parser.c.
9. **`Language::id_for_node_kind(kind: &str, named: bool) -> u16`** exists (0 = not found; `"ERROR"` named special-cased); reverse `node_kind_for_id(u16)`; `field_id_for_name`. No method literally named `symbol_for_name` in 0.25+. — docs.rs Language, language.c.
10. **`Parser::set_logger`** requires `Send + 'static` in 0.27 (breaking vs 0.26); diagnostics only. Cancellation is the progress callback (3). — lib.rs, v0.27.0 release notes.

### A.4 refactree (verified 2026-09-13 via pkg.go.dev module snapshot `v0.0.0-20260723-8593d0168fc5` + proxy.golang.org go.mod; the GitHub repo is **deleted** — GitHub API returns 404, confirmed 2026-09-13 (eval-3 C5); deletion occurred between the 2026-07-23 snapshot and research time)

Go CLI `rft` ("semantic relations on code and mechanical deterministic refactorings"), **MIT**, pre-v1, early-stage. Facts feeding this plan: **`Rule → Match → []Edit`** as the single rewrite primitive (mv's symbol-identity planner emits site rules into the *same* engine — one matcher, no second engine); **byte-span model**: half-open `[StartByte, EndByte)` UTF-8 ranges, text never stored on spans, **edits applied per file back-to-front** so earlier offsets stay valid, minimal-diff via common prefix/suffix; **staged overlay writes**: `StageEdits` (in-memory overlay, no disk) → `ValidateStagedProject` (re-ingest of staged content; clears the per-file cache "so overlay content is not masked") → `CommitOverlay` (empty content = delete) — the direct precedent for our `apply` loop; matcher = **"token stream + pattern NFA"** (precedent for ADR-0005), DSL: literals, `/regex/`, `$name`, `$name:@ref` (semantic hyperlink holes), `Multi *` suffix; `rft grep --format jsonl` (`GrepHit{File, Line, Col, Match, Captures}`) = machine surface; **no persistent index** (on-demand dir/seed/hop walks; in-process per-file cache; mtime caching only in long-lived sessions) — precedent for ADR-0007; LSP = stdio *server* (DidOpen/DidChange feed the overlay; last-good snapshot) — the F10 template; fixture-driven pattern tests (`op.json`: pattern + replacement + expected match count) and an offline invariant fuzz harness — borrowed for F4.9/F7; import hygiene computed on original-source offsets, applied after body edits — noted for future rewrite features. Caveats from its own docs: early software, best-effort writes ("keep git ready"); bus-factor-1 risk **realized** — the repo is deleted (confirmed, 2026-09-13), making the module-proxy archive the source of record (cite pkg.go.dev / proxy.golang.org, never GitHub). Sources: pkg.go.dev module + package pages (root, cmd/rft, pkg/{pattern,ingest,projectfs,reference,web,web/graphql}, internal/{lsp,fuzzy}), go.mod via proxy.golang.org.

### A.5 F0 pin-check results — executed 2026-09-13

Execution environment: rustc/cargo **1.98.0** (stable; the same version `rust-toolchain.toml` pins for local dev), Linux x86_64. CI additionally runs the MSRV leg on 1.90.0 via `RUSTUP_TOOLCHAIN`.

**lang-all × tree-sitter 0.27.0 (KP-5 fence) — GREEN.** All seven grammar crates (`=0.24.2/=0.25.0/=0.25.0/=0.25.0/=0.23.2/=0.23.5/=0.24.2`) resolve, compile, and link against core `=0.27.0` — no `links = "tree-sitter-language"` collision, as A.3.8 predicted. Evidence: `cargo build --features lang-all` green, and the binary's envelope reports the full list:

```json
{"schema":"janitor.v1","ok":true,"data":{"version":"0.1.0","grammars":["rust","python","go","javascript","typescript","java","c"]}}
```

Scope note: this is a **build/link-level** fence (dependency resolution + C compilation + linkage). Runtime language registration (`set_language`) is first exercised by F3's `ffi_shim`; the KP-5 kind dump (F1.5) upgrades this fence to a runtime check. The CI job `pin-check` re-runs it on every PR.

**Full F0 gate suite (local, 1.98.0):** `cargo fmt --check` green · `cargo clippy --all-targets -- -D warnings` green (default features) · same with `--all-features` green · `cargo test` green (13/13, also under `--all-features`) · `cargo build --no-default-features` green (`--version` then reports `"grammars": []`).

**Registry errata vs A.1 (live crates.io, re-checked at execution):** the crates.io package name is `smol_str` (underscores; PLAN §2 writes `smol-str` — cargo cannot resolve the dashed name); `proptest` 1.8.2 and `criterion` 0.11.0 were **never published** (real latest: 1.11.0 / 0.8.2). The manifest therefore pins `smol_str = "=0.3.6"` (version unchanged from §2), `proptest = "=1.11.0"`, `criterion = "=0.8.2"` — the latter two are dev-deps; every production pin matched §2 verbatim (verified crate-by-crate against the API).

**musl toolchain decision: cargo-zigbuild (preferred) with musl-gcc fallback.** Rationale: zig's bundled self-contained CC removes the system `musl-tools` dependency, is the upstream-documented install path (`pip install ziglang` + `cargo install cargo-zigbuild --locked`), and extends to the macOS cross builds the release pipeline eventually wants. Wired as: CI job `musl-smoke` tries cargo-zigbuild first; on its failure a conditional step installs `musl-tools` and falls back to plain `cargo build --target x86_64-unknown-linux-musl`; the job then smoke-tests the artifact (`--version` envelope via jq) and is `continue-on-error` (best-effort per F0.5/R18). No local musl run: the execution sandbox has no musl/zig toolchain; first real execution is the `musl-smoke` job on the F0 PR.

**THIRD_PARTY manifest snapshot: deferred to F7.3** (cargo-about in CI, PLAN §3 F7.3/R13); NOTICE carries the manual credits meanwhile (§10).

## Appendix B — Eval reconciliation record (2026-09-13)

Post-eval reconciliation of this plan against the three reviews in `docs/evals/`, per `docs/planning/CONSOLIDATE.md` (E-numbers are its edit list). Anchors (ADR/KP/R/Q/F ids, section numbers) were preserved; additions are appends only.

| Review | Condition / finding | Disposition |
|---|---|---|
| eval-1 c1 (F8.5) | MSRV 1.85 inconsistent with tree-sitter 0.27.0 | Accepted — E1: `rust-version = "1.90"`; `=0.26.13` alternative considered and rejected |
| eval-1 c2 (F2) | `Unify` Merkle equivalence drops anonymous operators | Accepted — E2: ADR-0010/§1.4.3.6 hash semantically-significant anonymous tokens, exclude only comment/whitespace trivia; F8 acceptance `x + y` ≠ `x - y`, `foo(a, b)` ≠ `foo(a; b)` |
| eval-1 c3 (F1) | ParseCache zero hit rate + O(repo) retention | Accepted — E3(a): streaming scan, per-file drop; dashmap pays off only multi-rule/warm (F10); R15 updated |
| eval-1 c4 (F5) | apply-path ownership asserted, not designed | Accepted — E3(b): remove+own before `Tree::edit`, `Arc::strong_count == 1` assert, typed error; KP-1 shared-clone leg; F5 alloc test |
| eval-1 c5 (F3) | Match-set semantics undefined | Accepted — E4: ADR-0016 |
| eval-1 c6 (F4) | `UnknownKind` contradiction; corpus-biased kind tables | Accepted — E5: one behavior + per-grammar fixture-corpus seeding |
| eval-1 c7 (F6) | No binary/size-cap/UTF-8 policy | Accepted — E6: F4 walk policy |
| eval-1 c8 (F8.1–F8.4) | Send/Sync facts, `parse_with_options` shape, clone semantics | **SUPERSEDED** — final plan was already correct (A.3.1–A.3.3, Rules T1/T3); eval-1 reviewed an intermediate draft on those points; its F5 exclusivity concern survives via E3(b) on eval-2's basis |
| eval-1 c9 (F7) | `tree-sitter-language` `links` collision | Accepted — E7: ADR-0015 note |
| eval-2 am.1 | §1.2.1 step 5 "cheap and safe" contradicts A.3.2 aliasing rule | Accepted — E3(b) [was blocking] |
| eval-2 am.2 | KP-9 LLM DSL-authorship gate | Accepted — E9 (F4 entry) |
| eval-2 am.3 | KP-10 real-repo scan-latency spike | Accepted — E9 (F1.7) |
| eval-2 am.4 | `docs/DSL.md` F7 → F4 | Accepted — E11(a) |
| eval-2 am.5 | end-to-end agent-loop acceptance test in F7 | Accepted — E11(b) — v0.1's definition of done |
| eval-2 am.6 | F9 before F8 in v0.2 | Accepted — E11(c); phase letter ids unchanged, order swap recorded under F8 |
| eval-2 am.7 | KP-8 allocation fence onto the F4 matcher | Accepted — E11(d) |
| eval-2 am.8 | corrected estimates (v0.1 40–66; full 48–81) | Accepted — E11(e) |
| eval-2 am.9 | ADR-0005/grammar spec gaps (capture priority, canonical order, `unify` stub, dashmap×rayon rule, STATE.json atomicity) | Accepted — E4 (ADR-0016 + ADR-0005 v0.1 scope note) |
| eval-3 C1 | nap synchronous + stateless overlay | Already in plan (ADR-0013; §1.0 stateless v0.1) |
| eval-3 C2 | two-way spike before F4 | **Declined** — E10: trigger-based (existing F4 >50% slip trigger), `ffi_shim.rs` seam; rationale appended to ADR-0002 |
| eval-3 C3 | slip trigger stands | Already in plan (ADR-0002 revisit trigger) |
| eval-3 C4 | post-v0.1 dogfood gate | Accepted — E9: KP-11, "post-v0.1 (funding gate)" |
| eval-3 C5 | refactree deleted; credits fact-fix | Accepted — E8 (§10 + A.4; GitHub API 404 confirmed 2026-09-13; OptMem no-license kept as-is) |
| eval-3 C6 | reframe DSL motivation | Accepted — E13 (§0 + §1.4.3): "explicit scope semantics with a proven linearity bound (KP-7)"; KP-7 stays the falsifiability fence |

**Verdicts, one line each:** eval-1 — GO-with-conditions (9): all conditions dispositioned above (c8 superseded on verified facts). eval-2 — aprovado-com-reordenacao: all nine amendments adopted. eval-3 — HYBRID with C1–C6: confirms ADR-0002 with the trigger-based C2 disposition.
