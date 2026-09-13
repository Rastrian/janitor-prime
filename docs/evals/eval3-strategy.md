# Eval 3 — Build vs. Leverage Strategy

**Question:** Does Janitor Prime duplicate prior art? Is the right move BUILD (from scratch), FORK (an existing project), or ORCHESTRATE (compose libraries behind a thin CLI)?

**Status:** COMPLETE (2026-09-13).

**Inputs:** `docs/planning/BRIEF.md` (authoritative spec). `docs/planning/PLAN.md` did not exist when this eval started; it appeared mid-eval (parallel planning lane) and was **read in full before the verdict** — §3 and §5 engage its ADR-0002, which explicitly defers to this eval. Research: web search + primary-source fetches (docs.rs, LICENSE files, GitHub API, pkg.go.dev, proxy.golang.org). Every license below was verified against a primary source, not a summary page.

**Scope of Janitor Prime per BRIEF (3 fused domains in one static binary):**
1. Transactional VFS (overlay filesystem; edits simulated off-disk)
2. AST refactoring engine — DSL over tree-sitter (`Under`/`Seq`/`Capture`/`Unify`)
3. Hierarchical persistent memory (OptMem-like: append-only log, `wake`/`note`/`nap`, compression tree)
Plus: 100% JSON CLI output for LLM agent consumers.

---

## 1. Prior Art Survey

### 1.1 ast-grep — the closest prior art (and the strongest leverage candidate)

**What it is.** `ast-grep` (`sg`) is a Rust CLI for structural search, lint, and rewrite over tree-sitter CSTs, using a "write the code you want to match" pattern DSL plus a YAML rule system (`kind`, `inside`, `has`, `regex`, constraints, composite `all/any/not` rules). It supports 20+ languages, ships `scan` (lint with `--update-all` autofix) and `run` (search/rewrite with `--rewrite`), and emits JSON output (`--json=pretty`). It is the reference implementation of "pattern DSL over tree-sitter" — i.e., most of Janitor Prime's Module 2.

**Key facts verified:**
- **License:** MIT, plain, no riders ([repo](https://github.com/ast-grep/ast-grep); docs.rs confirms MIT for the crates). Verified via GitHub and docs.rs, not hearsay.
- **Maintenance:** exceptionally active. Current crate `ast-grep-core` 0.45.x on docs.rs; the team recently rewrote tree-sitter's C runtime in Rust for ~30% faster parsing ([blog](https://ast-grep.github.io/blog/tree-sitter-rust-rewrite)). Multi-thousand-star project, used in production (CodeRabbit, editor integrations).
- **Embeddable as a library, not just a CLI.** [`ast-grep-core`](https://crates.io/crates/ast-grep-core) exposes `AstGrep<D: Doc>`, `Pattern`, `Matcher`, `NodeMatch`, and — critically — the [`Doc` and `Content` traits](https://docs.rs/ast-grep-core/latest/ast_grep_core/) that "abstract away source code encoding issues," i.e., source text does NOT have to be a disk file. A caller can supply in-memory content. So the BRIEF's hard requirement ("the tree-sitter parser MUST read bytes primarily from vfs::Overlay") is *expressible* on top of ast-grep-core via a custom `Doc`/`Content` impl.
- **Metavariable unification already exists.** ast-grep's [pattern syntax](https://ast-grep.github.io/guide/pattern-syntax) states you can "reuse same name meta variables to find previously occurred AST nodes" — `$A == $A` requires identical code at both occurrences. This covers Janitor Prime's `Unify(Symbol, Symbol)` **within one match**. What it does not cover: unification across *separate matches* (e.g., "find duplicated function bodies across the repo"), which needs a different pass (collect captures, group by structural hash) — and which Janitor Prime's spec implies ("base para abstracao de duplicidade").

**What's missing vs. Janitor Prime:**
- **No transactional VFS.** ast-grep rewrites go straight to files (`--rewrite` / `--update-all`). No overlay buffer, no anti-phantom `read_dir` semantics, no "apply diff → re-parse → validate → only then flush" transaction. (Editor integrations edit buffers, but that's the editor's buffer, not ast-grep's.)
- **No `Under(NodeType)` scoping operator** as a first-class construct — closest analogues are `MatchStrictness` and rule combinators (`inside`, `has`). Note nuance: ast-grep's `Matcher` is an implementable trait, so custom semantics are *expressible* — but you'd be writing your own matcher on top of their object model, which is building the evaluator with extra coupling.
- **No memory system at all** — nothing like `wake`/`note`/`nap`, no log, no SyntaxPointer anti-hallucination anchoring.
- **Agent-first JSON:** present for match output, but no state handshake or diff-apply protocol; a stateless filter, not an agent-side tool.
- **Fixed matching semantics.** Pattern trivia/adjacency semantics are ast-grep's, not yours — relevant when your spec wants named-sibling-skipping-comments adjacency (PLAN's ADR-0010).

**Bottom line:** ast-grep-core already delivers ~70% of Module 2 *at the capability level* (parse, match, capture, rewrite, multi-language, perf-tuned, JSON). It does not deliver Janitor Prime's specific *semantics* (scoped state machine, cross-match unification, overlay-fed transactions). That distinction drives the verdict in §5.

### 1.2 tree-sitter native query system

**What it is.** tree-sitter ships its own query language: [S-expression patterns](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/1-syntax.html) with `@name` captures, quantifiers (`*`, `+`, `?`), anchors, alternations, [predicates/directives](https://tree-sitter.github.io/tree-sitter/using-parsers/queries/2-predicates.html) (`#eq?`, `#match?` regex, `#is?` conditional). The Rust crate exposes `Query` + `QueryCursor` (stateful execution over a `Tree`, iterating `QueryMatch`/`QueryCapture`; [docs.rs](https://docs.rs/tree-sitter/latest/tree_sitter/struct.QueryCursor.html)). This is what Neovim/Helix/GitHub semantic highlighting use at scale.

**Key facts:**
- **License/maintenance:** MIT, canonical, extremely active (restructured into the `tree-sitter` org; core crate at 0.27.0 as of 2026-08-30, per PLAN's Appendix A.1, which verified versions against the crates.io API). Zero abandonment risk; the real risk is the grammar-crate × core version matrix (PLAN R1/KP-5), which any consumer inherits.
- **It is a *matching* engine only — no replacement.** Queries find and capture nodes; no rewrite/template mechanism, no edit application. Every "codemod on tree-sitter" tool (ast-grep, GritQL, semgrep's engine layer) builds its own rewrite layer on top.
- **`#eq? @a @b` predicate** gives textual equality between two captures *within a pattern* — within-match unification as a predicate.
- **Caller supplies everything:** tree-sitter never touches the filesystem. Parse from bytes; the caller decides where bytes come from. Zero-impedance substrate for a VFS-fed pipeline — hand it your overlay snapshot.

**What's missing vs. Janitor Prime:** everything except matching — no rewrite templates, no rich rule composition, no file walking, no VFS, no memory. It is the *lowest* layer, not a competitor — and therefore the natural leverage point.

**Sibling note — GritQL:** a Rust DSL for search/lint/rewrite over tree-sitter, ~SQL-like. **MIT** ([repo](https://github.com/biomejs/gritql)), but governance just churned: owner Grit.io wound down OSS maintenance (archived pattern repos, May 2025) and the project was [transferred to the Biome organization](https://biomejs.dev/blog/gritql-under-biome-umbrella/), now powering Biome 2.0+'s plugin system; crates.io artifacts (`grit-pattern-matcher` 0.5.x) are stale (>1 year, personal account). Interesting DSL ideas; embedding it today carries governance risk ast-grep-core does not.

### 1.3 comby

**What it is.** [comby](https://comby.dev/) ([repo](https://github.com/comby-tools/comby)) — structural search & replace for "~every language," using hole-based templates (`:[hole]` matches balanced sub-expressions) rather than full ASTs. Written in **OCaml**. Interactive review mode, JSON output, powers Sourcegraph's structural search.

**Key facts verified:**
- **License:** Apache-2.0, plain, no riders — verified directly against the [LICENSE file](https://raw.githubusercontent.com/comby-tools/comby/master/LICENSE) ("Copyright 2019 Rijnard van Tonder", standard text, no modifications, no dual licensing).
- **Maintenance:** slow but not dead. `comby-ui` archived 2023; main repo has activity into 2026 with a sizeable issue backlog. Single-maintainer bus factor.
- **Matching model:** language-agnostic hole matching is both its strength (works where no grammar exists) and its weakness (less AST-precise than tree-sitter tools).

**Why it's not our substrate:** OCaml cannot be linked into a static Rust binary — subprocess only, breaking the BRIEF's "single statically-compiled executable." No VFS transaction, no memory, no agent protocol. Value to this eval: proof that template-hole UX is beloved; nothing to fork.

### 1.4 grep-ast

**What it is.** [grep-ast](https://github.com/Aider-AI/grep-ast) (Paul Gauthier, aider ecosystem) — regex grep that renders matches with AST context: the `TreeContext` class shows "lines of interest" plus enclosing function/class headers. Built on tree-sitter (Python; migrating from unmaintained `tree-sitter-languages` to `tree-sitter-language-pack` — [issue #7](https://github.com/Aider-AI/grep-ast/issues/7)).

**Key facts verified:**
- **License:** Apache-2.0 (repo license file). **Maintenance:** active as an aider-adjacent utility; its packaging churn (py-tree-sitter-languages unmaintained) is a cautionary tale about multi-grammar dependency management — the Rust workspace avoids that class of problem by pinning per-grammar crates.
- **Relevance:** a *context display* tool, not a rewrite engine; duplicates nothing in Janitor Prime. The adjacent idea — aider's **repo-map** (tree-sitter-derived, ranking-weighted repo map fed to the LLM) — is real prior art for "what context to give an agent," but it is per-invocation and stateless: no persistence, no budgeted wake, no compression tree. It competes with Module 3 only in spirit.

### 1.5 refactree — the BRIEF's named inspiration is now a ghost

**What it was.** `refactree` (`rft`), by Lucas Eduardo Wendt (`lucasew`): "Tool to get semantic relations on code and do simple mechanical deterministic refactorings." A Go CLI building a semantic model (modules → files → "atoms"/named definitions → uses) from tree-sitter parses, on demand with no persistent index ("like rg/fd"). Commands: `rft ls/doc/edit/astdump/browse/serve/desktop/mv/rewrite/lsp/grep`. Structural grep with a token-stream pattern dialect (e.g. `$F:@go:fmt::Errorf($MSG, $ERR)`), `rft rewrite` with `-n` dry-run / `-i` confirm / `-b` backups, pattern IR + string-dialect parser (`pkg/pattern`), reference scheme `provider:where::what`, per-language ingest (Go/Python/Java/JS full; Nix/Svelte partial), web viewer (GraphQL + HTML annotate), Charm TUI, `rft lsp` (stdio LSP). Its `projectfs` package: "a small path-oriented file reader for project load and **LSP overlays**."

**Critical finding — the repo no longer exists.** As of 2026-09-13:
- `github.com/lucasew/refactree` → HTTP 404 (deleted or private), likewise `lucasew-graveyard/refactree`.
- GitHub-wide search for "refactree" → **0 repositories**.
- What survives is the **Go module proxy cache**: [pkg.go.dev](https://pkg.go.dev/github.com/lucasew/refactree) shows last pseudo-version `v0.0.0-20260723033240-8593d0168fc5` (2026-07-23), and the [go.mod](https://proxy.golang.org/github.com/lucasew/refactree/@v/v0.0.0-20260704191207-d346faab853f.mod) remains fetchable: deps were `spf13/cobra`, the Charm TUI stack, `docker` + `testcontainers-go` (sandboxed execution), and tree-sitter via **`lucasew/ccgo-tree-sitter`** — a personal CCgo-transpiled binding, i.e., a bespoke non-CGo tree-sitter fork. Source zip is still downloadable from `proxy.golang.org`.
- **License:** MIT (detected by pkg.go.dev on the cached module; no riders detected).
- **Maintenance:** single author; README self-describes as early software "heavily developed with AI agents." Deletion between 2026-07-23 and 2026-09-13 — bus-factor-1 risk *realized*.

**What it had vs. Janitor Prime:** pattern IR ≈ Module 2 (in spirit); `projectfs` overlays ≈ a *proto*-VFS (path-oriented reader for LSP overlays; no transactional apply/re-parse/flush cycle, no anti-phantom contract); `rft lsp` ≈ the LSP surface the BRIEF defers. **What it never had:** persistent memory, `wake`/`note`/`nap`, agent-JSON protocol, tree-invalidation semantics over a live overlay.

**Strategic meaning:** (a) FORK is structurally awkward — Go code, dead repo; "forking" means vendoring a proxy-cached zip into a Rust codebase: a rewrite with extra steps. (b) The PLAN's §10 credits and Appendix A.4 should cite refactree as historical inspiration, **MIT-licensed**, with the archival URL (proxy.golang.org / pkg.go.dev), noting the repo is gone. (c) Its disappearance is a data point: in this niche, one of the BRIEF's two named inspirations died quietly; the survivor is ast-grep.

### 1.6 Agent memory systems — OptMem (the other named inspiration), mem0, Letta, Zep

**OptMem ([VictorTaelin/OptMem](https://github.com/VictorTaelin/OptMem)).** "Permanent memory for AI agents. A 426-token prompt, a script, plug and play." Not a library: a **single-file, zero-dependency Python CLI (`memo`)** plus a ~426-token prompt block pasted into the agent's `AGENTS.md`/`CLAUDE.md`. Model: append-only `LOG.txt` (fixed-width records; position = identity; single-seek lookups; `wake` fast even at ~1M memories), rebuildable `TREE/` summary cache forming a binary tree of pairwise LLM-written summaries, commands `wake/note/nap/recall/zoom/forget`. Notably: **nothing runs in the background** — merge requests accumulate, resolved by `nap`; *the consuming LLM writes the summaries*; subagents are told not to run memo. (PLAN Appendix A.2 independently verified the same facts, plus record widths: 320 B log / 288 B tree.)
- **License: NONE.** GitHub API reports `license: null` — no LICENSE file, no grant. The inverse trap of "MIT with riders": no license at all = all rights reserved; **the code cannot legally be forked or vendored.** The *concept* (append-only log + binary summary tree + budget tiling) is an idea, not copyrightable expression — a clean-room Rust reimplementation is legal, and given the code is one Python file, small effort.
- **Maintenance:** modest but alive (pushed 2026-07-31, 1.5k stars, 42 commits). A weekend-scale artifact with viral reach, not infrastructure.

**mem0 ([mem0ai/mem0](https://github.com/mem0ai/mem0)).** Apache-2.0 (verified on repo), 65k+ stars, YC-backed, very active. Architecture: *extract-and-retrieve* — **requires an LLM** (default gpt-5-mini-class) + embedding model + vector store; single-pass ADD-only extraction, entity linking, hybrid semantic+BM25+entity retrieval, temporal ranking. Ships Python/TS SDKs, self-hosted Docker server, agent "skills" for Claude Code/Codex/Cursor.
- A **different architectural species** from Module 3: LLM-dependent infrastructure with vector retrieval, versus a deterministic local log navigated by the agent itself. Not embeddable in a static Rust binary except as an external service; philosophically opposite (memory extraction by a tool-LLM vs. memory *discipline by the agent*).

**Letta (ex-MemGPT) / Zep (Graphiti).** Apache-2.0 / MIT-Graphiti respectively; stateful agents with self-managed tiered memory / temporal knowledge graphs. Same species as mem0: services or heavy SDKs, LLM-in-the-loop, server-side state. None is a local single-binary deterministic log.

**Gap that survives:** a *tiny, deterministic, local, license-clean* memory substrate with (a) token-budgeted `wake`, (b) notes **anchored to code reality** (SyntaxPointer: AST span/content-hash or commit hash — *no surveyed system does this*), (c) lock flags protecting facts from compression dilution (OptMem has `forget` but no locks), (d) Global/Local layering. A genuinely unoccupied cell of the design space — but a *small* cell: the intersection of "OptMem concept" with "clean license + Rust + code-anchored pointers," not a moat measured in engineering years.

---

## 2. Gap Analysis vs. Janitor Prime's Three Domains

| Capability (per BRIEF) | ast-grep | tree-sitter queries | comby | refactree | OptMem | mem0/Letta/Zep |
|---|---|---|---|---|---|---|
| Parse from custom bytes (VFS-fed) | ✅ `Doc`/`Content` traits | ✅ native | ✅ (own I/O) | ✅ (dead repo) | n/a | n/a |
| Pattern DSL search | ✅ | ✅ (queries) | ✅ holes | ✅ (dead) | ❌ | ❌ |
| Rewrite + edit application | ✅ | ❌ match-only | ✅ | ✅ (dead) | ❌ | ❌ |
| In-match unification (`Unify`) | ✅ metavar reuse | ✅ `#eq?` | partial (same hole) | unknown (dead) | ❌ | ❌ |
| Cross-match unification (dup detection) | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Transactional overlay (apply→validate→flush) | ❌ | ❌ | ❌ | ❌ proto-only | ❌ | ❌ |
| Anti-phantom read_dir contract | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Persistent agent memory (log + budget wake) | ❌ | ❌ | ❌ | ❌ | ✅ (unlicensed) | ✅ (LLM-infra species) |
| Notes anchored to AST span / commit hash | ❌ | ❌ | ❌ | ❌ | ❌ | ❌ |
| Compression tree with locks | ❌ | ❌ | ❌ | ❌ | partial (no locks) | ❌ |
| Agent JSON protocol (wake/scan/apply) | partial (`--json`) | ❌ | partial | ❌ | ❌ | SDK-shaped, not CLI |

**Domain verdicts:**
- **Module 2 (AST engine): duplicated at the capability level, not the semantics level.** ast-grep-core (MIT, maintained, embeddable) already does parse/match/capture/rewrite/multi-language/JSON. Janitor Prime's residuals are semantic: `Under`/`Seq` with named-sibling-skipping-trivia adjacency (PLAN ADR-0010), scoped state-machine execution, and cross-match unification (which the PLAN itself defers to v0.2/F8). Building a *thin* evaluator over raw tree-sitter buys spec fidelity and zero coupling; orchestrating ast-grep-core buys maturity but imports its fixed semantics and a 0.x-crate dependency. Neither choice changes the product's differentiation (see §4) — this is an engineering-economics call, quantified in §3 and ruled on in §5.
- **Module 1 (VFS): genuinely unoccupied.** No surveyed tool offers a staged, syntax-validated, atomically-flushed overlay as a CLI primitive. Closest analogues: git worktrees (text-level, heavyweight, no syntax gate) and editor LSP buffers (not CLI-accessible). The PLAN resolves the BRIEF's ambiguity correctly: stateless across invocations in v0.1, transaction = one `apply` invocation (stage → validate → flush/rollback), no persistent phantom overlay directory.
- **Module 3 (memory): concept exists (OptMem) but is unlicensed Python with no code-anchoring and no locks; industrial systems are a different (LLM-infra) species.** A clean-room Rust reimplementation with SyntaxPointer + locks + layers is legal, cheap, and unoccupied. The PLAN's ADR-0013 correctly resolves the BRIEF's "background compression" contradiction by adopting OptMem's model: tool plans/sequences, *consuming agent* writes summaries, synchronously — no LLM inside the binary. This eval's research independently confirms that is the right (and only coherent) resolution.
- **The fusion itself (three domains, one static binary, one JSON protocol) is the actual differentiator** — no surveyed project combines staged edits + structural queries *over the staged state* + persistent anchored memory. Each domain has neighbors; the intersection is empty.

## 3. Option Costs — lane-weeks

(1 lane-week = 5 focused working days of one senior Rust engineer. Calibrated against PLAN §6, whose independent estimate is v0.1 = 35–55 d and full BRIEF = 43–70 d; my earlier top-down BUILD estimate of 12–18 lw for v0.1 was pre-cut-scope and is superseded by the harmonized numbers below. v0.1 cut: one language, no `Unify`, no LSP/watch/daemon.)

| Option | v0.1 cost | Full-BRIEF cost | Ongoing tax / quarter | Notes |
|---|---|---|---|---|
| **BUILD** (PLAN as written: raw tree-sitter, own thin evaluator) | **7–11 lw** (35–55 d) | 9–14 lw (43–70 d) | 1.5–3 lw | Engine share (F3+F4) = 13–20 d. Owns tree-lifecycle risk (mitigated by PLAN's F1 spikes + `ffi_shim.rs` isolation). Grammar matrix risk inherited from tree-sitter regardless of option. |
| **FORK** | ≈ BUILD + archaeology | — | as BUILD + merge debt | **No viable target.** refactree: Go *and* deleted (source only via Go proxy zip). ast-grep: needs no fork — it's already a library with extension traits. comby: OCaml. OptMem: **no license** — legally unforkable code, trivially re-implementable concept. |
| **ORCHESTRATE** (ast-grep-core as the engine dep) | **5–8.5 lw** (26–43 d) | 6–10 lw | ~1 lw + 0.x-upgrade churn | F3+F4 (13–20 d) replaced by bridge: custom `Doc`/`Content` over `Arc<[u8]>` snapshots + DSL→rules compiler + custom `Matcher` for `Under`/trivia semantics ≈ 4–8 d. **Net saving only 1.5–2.5 lw**, in exchange for: fixed matcher semantics vs. the DSL's specified semantics, coupling to a 0.x crate's object model, and a second tree-sitter surface (memory's SyntaxPointer wants raw `Tree` anyway). Savings grow with language breadth (F9) — grammar plumbing is what ast-grep does best. |
| **HYBRID** (build differentiators on leveraged substrates + evidence gates; §5) | **7.5–12 lw** incl. the C2 spike + C4 dogfood | 9.5–15 lw | 1.5–3 lw | ≈ BUILD + ~2–4 d of decision experiments, buying an evidence-based escape hatch to ORCHESTRATE if F4 fights, and a value-hypothesis test before post-v0.1 funding. |

## 4. Differentiation Stress-Test

Claimed differentiation: **transactional VFS + OptMem-like memory + agent-first JSON CLI**. Does each survive contact with the survey?

1. **Transactional VFS — SURVIVES, with a required articulation.** Empty cell in the matrix. The honest counter-question is "why not `git worktree` + `janitor apply --check`?" The defensible answer: the transaction is the *product* — staged bytes, syntax validation as the commit gate, no `.git` state pollution, and anti-phantom `read_dir` semantics that stop agents from "discovering" files that don't exist on disk. A nameable workflow primitive ("staged refactor with a syntax gate"), not a data structure. Condition: keep the PLAN's v0.1 shape — overlay is process-internal, `apply` is atomic, `virtual_paths()` is the explicit visibility surface. A persistent overlay directory would re-invent a bad git.
2. **OptMem-like memory — SURVIVES as extended reimplementation, not as novelty.** OptMem owns the concept and the mindshare; Janitor Prime's defensible adds are SyntaxPointer (anti-hallucination anchoring to span+content-hash/commit — no surveyed system does this, and it synergizes with the AST engine: notes point into the tree the engine already parsed), ArchitecturalLock, Global/Local layering. "Inspired-by, credited, extended" — and with OptMem unlicensed, the day-1 credits file is the legality story, not polish.
3. **Agent-first JSON CLI — DOES NOT SURVIVE ALONE.** Every modern tool has `--json`; refactree had GraphQL+TUI+LSP; mem0 ships agent skills. Table stakes. What survives is the **protocol shape**: `wake` (state handshake) → `scan --rule` (query over staged state) → `apply --diff` (validate-gated mutation) → `note` (anchored persistence) as one coherent agent-side state machine. Individually weak; as the connective tissue of #1 and #2, it is the product.
4. **Claims that do NOT survive:**
   - *"DSL designed to defeat catastrophic backtracking"* — tree-sitter matching is compiled and traversal-linear; the monster lives in regex engines. `Under` is fine as DSL sugar and as a stated complexity bound; as the engine's founding motivation it justifies nothing. (The PLAN already reframes it correctly: memoized-failure NFA with a falsifiability bench, KP-7.)
   - *"Deterministic engine" + "background compression"* — contradictory; the OptMem merge-request protocol is the only coherent resolution, already adopted as ADR-0013.
   - *"Single static binary"* as a differentiator — a deployment nicety (and one that conveniently rules out comby/mem0 as embedded deps), not a moat.
5. **The intersection risk (the one that matters):** empty cells are only opportunities if someone wants to stand in them. The value hypothesis — "agents refactor more safely when edits are staged+validated, and remember more usefully when knowledge is anchored to code" — is plausible and unfalsified, but *unproven*. Adjacent demand is proven (OptMem's 1.5k stars = agents want persistent memory; ast-grep's adoption = agents want structural ops); the *combination* is not. Kill-point: a ~1-week dogfood — run an agent through `wake→scan→apply→note` on a real repo vs. a git-worktree+ast-grep baseline; if it doesn't demonstrably reduce broken edits or context loss, the fusion is a science project. Note the asymmetry: the VFS and protocol are only *reachable* through the fusion product, so this test cannot be run before v0.1 exists — it gates post-v0.1 funding, not F0.

## 5. VERDICT

**HYBRID — build the differentiators on leveraged substrates, with two evidence gates. (This confirms PLAN ADR-0002's decision, adds conditions, and names its designated fallback.)**

**The decomposition:**

- **BUILD (because unoccupied or semantics-mandated):** the transactional VFS (F2/F5), the thin DSL evaluator over raw tree-sitter (F3/F4) — *not* because ast-grep can't match code, but because the BRIEF/PLAN specify matching *semantics* (scoped `Under` state machine, trivia-skipping `Seq`, `Unify`) that would have to be reimplemented as a custom `Matcher` on top of ast-grep-core's 0.x object model anyway; and the memory module (F6, clean-room per the no-license finding), plus the JSON protocol/CLI.
- **LEVERAGE (because duplicating is re-litigating solved problems):** tree-sitter core + per-grammar crates (the entire parse substrate); OptMem's *design* (log + binary summary tree + block-tiling wake + synchronous nap — ideas only, zero code, credited); `git` subprocess for commit pointers; clap/serde/rayon/dashmap and the standard Rust stack. Explicitly **not** adopted: ast-grep-core as an engine dependency for v0.1 (1.5–2.5 lw saving < semantic-impedance + coupling cost at v0.1's one-language scope; revisit at F9 multi-language, where its grammar plumbing is most valuable), GritQL (governance churn), comby (OCaml), mem0/Letta/Zep (wrong species).

**Why not the alternatives:**
- **FORK** — structurally unavailable: refactree is Go and its repo is *deleted* (only the Go module proxy zip survives); OptMem has no license (legally unforkable); ast-grep needs no fork — it is already an MIT library with extension traits.
- **Pure ORCHESTRATE** — optimizes the wrong fraction. The differentiating domains (VFS, memory, protocol) exist in no library and get built under every option; orchestration only substitutes the engine layer, saving 1.5–2.5 lw at v0.1 scope while importing matcher-semantics fights and a 0.x dependency. The saving is real but small; the coupling is permanent.
- **Pure BUILD** — mislabeled even for the PLAN as written: tree-sitter, OptMem's design, and git are all leverage. Calling it "build" hides where the risk actually lives.

**Conditions (each falsifiable, each cheap):**
- **C1 — Adopt the PLAN's two spec corrections** (this eval's research independently confirms both): nap as a synchronous agent-authored protocol (ADR-0013 — OptMem's "nothing ever runs in the background" is the only coherent deterministic design), and stateless-per-invocation VFS with atomic `apply` (no persistent overlay dir).
- **C2 — Evidence gate before F4 (~2–3 d):** implement the five KP-6 real-world patterns *both* ways — hand-evaluator sketch vs. ast-grep-core rules + `Doc`/`Content` bridge over `Arc<[u8]>` snapshots. If ast-grep expresses them cleanly *and* the bridge is trivial, flip ADR-0002 to orchestrate with data. If it fights (expected: trivia-skipping adjacency, `Under`-as-scope), ADR-0002 is confirmed with evidence instead of conviction. Record the outcome in the ADR either way.
- **C3 — Slip trigger stands (PLAN's own):** if F4 exceeds estimate by >50%, the C2 spike becomes mandatory before any further evaluator investment; ast-grep-core is the designated fallback, and `ffi_shim.rs` (PLAN F3) is the seam that keeps the swap bounded to one module.
- **C4 — Dogfood gate before post-v0.1 funding (~1 wk):** agent through `wake→scan→apply→note` on a real repo vs. git-worktree+ast-grep baseline; measurable reduction in broken edits or context loss, or the project stops at v0.1. The fusion is the product; this is its kill-point.
- **C5 — Day-1 OSS hygiene, now with facts:** credit OptMem (Victor Taelin) — *no license, ideas/formats only, zero code*; credit refactree (lucasew) — **MIT**, repo deleted 2026-07/09 window, cite the proxy.golang.org/pkg.go.dev archive (this fills PLAN Appendix A.4 and §10's ⏳ marker); dual MIT OR Apache-2.0 is compatible with every adopted dependency (tree-sitter MIT; grammars tracked by cargo-deny per PLAN R13).
- **C6 — Reframe the DSL's motivation** in PLAN/docs from "defeats catastrophic backtracking" to "explicit scope semantics with a proven linearity bound" (KP-7 stays as the fence); the backtracking claim did not survive this survey.

**Cost:** v0.1 ≈ 7.5–12 lane-weeks including both gates; full BRIEF scope ≈ 9.5–15. The gates cost ~1.5 lw and are the cheapest insurance in the whole plan.
