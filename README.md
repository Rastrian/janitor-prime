# janitor-prime

A single statically-linked Rust binary (`janitor`) that gives autonomous LLM
agents three fused capabilities over a target codebase:

1. **`src/vfs`** — a transactional virtual filesystem: an `Overlay` over `Os`
   that simulates edits in memory, with a strict anti-phantom invariant and
   commit/rollback boundaries around multi-file changes.
2. **`src/ast`** — a structural search/rewrite engine over
   [tree-sitter](https://tree-sitter.github.io/), driven by a small
   s-expression DSL with explicit scope semantics and a proven linearity
   bound.
3. **`src/memory`** — a persistent, append-only memory with anti-hallucination
   syntax pointers and a synchronous `nap` compression protocol. No LLM call
   ever happens inside the binary.

**Status:** pre-alpha. Phase F0 (scaffold + CI + grammar pin-check) and F1
(kill-point spikes: incremental re-parse, validity-gate honesty, trivia
semantics, parse throughput) are complete — measured outcomes in
[`docs/planning/PLAN.md`](docs/planning/PLAN.md) §5. The VFS module (F2) is
next. The `janitor --version` envelope below is the only implemented command
so far.

## Agent topology

`janitor` is designed as a **tool**, not a service: one process per
invocation, `stdin`/`stdout` only, zero network. The consuming agent drives
it as a state machine across a coding session — `wake → scan → apply → note`
— and compression is closed by the same agent via the synchronous `nap`
protocol.

### Session lifecycle

```mermaid
sequenceDiagram
    autonumber
    actor A as Agent (LLM)
    participant J as janitor CLI
    participant O as Overlay (src/vfs)
    participant E as Engine (src/ast)
    participant M as Memory (src/memory)

    A->>J: janitor wake --budget 8000
    J->>M: block-tiling recall (newest raw, oldest coarsened)
    J-->>A: facts + summaries within budget, linter status

    A->>J: janitor scan --rule "(under function_item (seq (capture @n (kind identifier)) (kind block)))"
    J->>O: read_dir (disk truth only) + virtual_paths() if opted in
    O->>E: per-file Arc byte snapshot (rayon, owned)
    E->>E: parse + scoped NFA match, memoized failures
    E-->>J: MatchReport list (byte spans, captures)
    J-->>A: canonical matches, deterministic under any thread count

    A->>J: janitor apply --diff (edits + expect_sha)
    J->>O: stage spliced bytes, bump generation
    J->>E: exclusive Tree edit + incremental re-parse
    alt error_count = 0 in edited ranges
        J->>O: commit, atomic flush to Os
        J-->>A: ok, flushed paths
    else syntax errors introduced
        J->>O: rollback
        J-->>A: E_PARSE_INVALID + offending node coordinates
        Note over A: agent fixes the diff and retries — disk was never touched
    end

    A->>J: janitor note --ptr span+content-sha "Overlay read_dir never consults buffer"
    J->>E: resolve pointer against current tree (re-anchor if moved)
    J->>M: append JSONL fact (position = identity)
    J-->>A: ok, id 42

    A->>J: janitor nap
    J-->>A: next pending compression (block, source material)
    A->>J: janitor nap --slot "0-1" --text "one-line summary"
    J->>M: append fixed-width TREE record
```

The two anti-hallucination gates are visible in the sequence: an agent
**cannot apply** a diff computed against a stale view (`expect_sha` mismatch
→ `E_STALE_VIEW`), and it **cannot note** a fact anchored to code that does
not exist (`SyntaxPointer` fails to resolve → `E_POINTER_DANGLING`).

### Module topology

```mermaid
flowchart LR
    A[Agent / LLM] -- "JSON envelope janitor.v1" --> CLI[src/cli + src/orchestrator]

    CLI --> VFS[src/vfs - ProjectFs trait]
    CLI --> AST[src/ast - DSL + evaluator]
    CLI --> MEM[src/memory - ContextManager]

    VFS -- "base_fs" --> OS[Os - std::fs proxy]
    VFS --> BUF[(Overlay buffer - RwLock map, tombstones)]
    AST -- "reads snapshot bytes" --> VFS
    AST --> TS[tree-sitter - grammars as cargo features]
    MEM -- "SyntaxPointer validation" --> AST

    CLI --> RAY[[rayon pool - owned snapshots, no borrows]]

    MEM --> LOG[(.janitor/LOG.txt - JSONL, append-only)]
    MEM --> TREE[(.janitor/TREE - fixed-width summary records)]
```

Module dependency direction is acyclic and enforced:
`cli → orchestrator → {vfs, ast, memory}`, with `ast → vfs` (the parser reads
Overlay bytes, never disk) and `memory → ast` (pointer validation needs a
live tree). The `orchestrator::Session` is the only place the three domains
meet.

### The edit transaction

```mermaid
stateDiagram-v2
    [*] --> Staged: apply --diff (payload validated against current bytes + expect_sha)
    Staged --> Validating: splice descending by offset, generation bumped
    Validating --> Flushed: whole-file error_count == 0
    Validating --> RolledBack: E_PARSE_INVALID + coordinates
    Flushed --> [*]: tmp+rename per file, partial failure reported honestly
    RolledBack --> [*]: prior bytes restored, disk never saw the bad state
```

### The agent loop, in commands

| Step | Command | Consumer contract |
|---|---|---|
| Recall context | `janitor wake --budget N` | facts + summaries within a token budget; `E_NAP_PENDING` if a compression is owed first |
| Find refactor sites | `janitor scan --rule "..."` | canonical, thread-count-deterministic match coordinates |
| Stage + validate + flush | `janitor apply --diff '...'` | typed errors (`E_STALE_VIEW`, `E_EDIT_OVERLAP`, `E_PARSE_INVALID`) or flushed paths |
| Persist a fact | `janitor note --ptr ... "..."` | anchored note id, or `E_POINTER_DANGLING` |
| Compress | `janitor nap` / `nap --slot ... --text ...` | the agent writes the summary; the tool sequences and stores it |

## The stdout contract

`stdout` is 100% JSON, from day one. Every user-facing message flows through
a versioned envelope:

```json
{"schema":"janitor.v1","ok":true,"data":{"version":"0.1.0","grammars":["rust"]}}
```

Failures use the same envelope with an `error` body carrying a stable `E_*`
code. Diagnostics go to `stderr` via `tracing` (env-gated, `RUST_LOG`).
Exit codes: `0` success, `1` operational error, `2` usage error.

## Building

```sh
cargo build                  # default: rust grammar only (ADR-0015)
cargo build --features lang-all
cargo build --no-default-features
```

Language grammars are cargo features: `lang-rust` (default), `lang-python`,
`lang-go`, `lang-javascript`, `lang-typescript`, `lang-java`, `lang-c`,
`lang-all`.

## License

Dual-licensed under [MIT](LICENSE-MIT) OR [Apache-2.0](LICENSE-APACHE).
Credits and provenance in [NOTICE](NOTICE) — the memory design is inspired by
Victor Taelin's OptMem (ideas and formats only; that project ships no
license, so no code is copied), and the staged-edit pipeline shape is
indebted to lucasew's refactree (MIT; repo deleted, archived at pkg.go.dev).
