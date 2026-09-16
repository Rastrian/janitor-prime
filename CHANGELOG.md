## [Unreleased] — v0.3.0 candidates

### Added
- **DSL text predicates (B1)**: `(text @cap "exact")` and `(glob @cap "p*")`
  post-match filters — the audit questions (TODO hunting, panic! counting,
  method-name globs) are now expressible in scan. Linear-time promise
  untouched (filters run post-match, O(matches × filter)).
- **wake environment notes (B2)**: `data.environment` reports PATH-probed
  language servers and git status — the guidance GLOBAL.md promised now
  exists in the payload.
- **Daemon provenance + cache telemetry (B4)**: `served_by:
  "daemon"|"process"` + `stats.cache {hits, misses}` — silent fallback is
  observable, warm hits are provable.
- **Duplicates summary/filters (B3)**: `--summary` per-kind aggregate +
  `--min-members N` group floor (deterministic under RAYON 1/8).
- **Note ergonomics (B5)**: typed refusal carries bytes/limit/preview;
  `note --stdin`; the `--ptr` notation documented in-note.
- **MCP prompts + outputSchema (B8)**: every tool declares its payload
  shape; `prompts/list` + `prompts/get` ship two guided loops
  (janitor_refactor_audit, janitor_memory_recall).

### Changed
- **Twins consolidated (B6)**: `content_sha` has one definition (apply/diff),
  the scan.rs function twin merged into `crate::ast::testing` — found by
  running `scan --duplicates` on the repo itself.
- **Docs truth pass (B7)**: rustdoc 0 warnings; doc-lint gate in CI.

### Fixed
- **Daemon large responses (issue #21)**: `write_all` under a send timeout
  aborted on the first `WouldBlock` once the AF_UNIX send buffer filled
  (8 KiB on macOS), truncating every daemon response past that boundary;
  the response write is now an honest loop that retries until a global
  drain deadline. A torn frame no longer looks like "no daemon":
  `scan --daemon` warns on stderr before falling back in-process (stdout
  stays 100% JSON). The client's response read cap rose to 64 MiB (was
  4 MiB) — real-tree envelopes run multi-megabyte.

## [Unreleased] — v0.2.0 candidates

### Added (v0.3 — product truth, roadmap #6)
- **DSL leaf-text predicates, `(text …)`/`(glob …)` (B1):** the rule
  grammar grows a `(filters …)` block on `(under …)` forms (after
  `(constraints …)`, nested unders included; clauses AND): `(text @sym
  "…")` requires the capture's leaf text to equal the string byte-exact,
  `(glob @sym "…")` matches it against `*`/`?`/`[…]` classes over bytes
  (dependency-free ~100-line matcher in `src/ast/glob.rs`; no regex
  crate, no new deps). "Leaf text" is the capture's leftmost **named
  leaf** — the node itself for leaf captures and comments, the name
  token for name-carrying subtrees (`macro_invocation` → `panic`).
  Filters are strictly post-match over the bound captures: a failing
  clause kills that match only, the NFA/DP run is untouched, and the
  pass is `O(matches × clauses)` (each tested capture pays one
  `O(depth × log fanout)` walk — the new `b1/filter-text` bench family
  pins match+filter scaling linear; the first, scanning implementation
  was caught being quadratic by exactly that leg and replaced with a
  binary-search descent). Uncaptured filter symbols and unclosed glob
  classes are the loud `E_DSL_COMPILE`; near-misses (bare needle,
  unterminated string, misplaced heads) carry did-you-mean suggestions.
  Grammar, semantics, the three worked idioms (panic-counting, TODO
  hunting under `--strict-trivia`, glob on method names), and the
  linearity restatement are documented in docs/DSL.md §2/§3/§7/§9; the
  new parser paths join the no-panic fuzz target (12 corpus seeds +
  generator pieces). The panic-counting acceptance rule is golden-pinned
  (`tests/fixtures/macro_text_filter*`: exactly the `panic!` sites), and
  the whole pre-existing golden corpus + KP-6 fixtures are unchanged —
  pre-v0.3 rules are byte-identical. No `E_UNSUPPORTED_VERSION` gate:
  this ships in v0.3.

### Added (v0.3 — product truth, roadmap #3)
- **Duplicates summary + filters (B3):** `scan --duplicates` gains
  `--summary` (prepend the per-kind aggregate — one row per
  (language, root kind) with group/member counts, sorted, plus totals;
  one envelope, no jq) and `--min-members N` (only groups with ≥ N
  members survive; values below 2 behave as 2). Defaults unchanged:
  without the flags there is no `summary` key and the member floor stays
  at 2. Both knobs also ride the daemon's `duplicates` request
  (append-only optional fields) and the MCP `janitor_scan` tool, so a
  routed response honors them instead of silently dropping them.
  Golden-pinned on a known trio/pair/solo corpus (10 groups / 25 members
  default; 5 / 15 under `--min-members 3`), and the summary is fenced
  byte-identical under RAYON 1 vs 8 through the real binary.

### Added (v0.3 — product truth, roadmap #2)
- **Daemon provenance + cache telemetry (B4):** every scan/duplicates
  response the daemon serves carries `served_by: "daemon" | "process"`
  (routed vs the CLI's in-process fallback) and `stats.cache =
  {hits, misses}` — that one request's warm-cache hit/miss delta, or
  `{0, 0}` on the fallback. The silent-fallback honesty hole is closed:
  a `--daemon` scan after the daemon died now visibly says
  `"process"`. The `stats` op already carried the cumulative
  `cache.{entries, hits, misses, evictions, bytes, bound_bytes}`
  counters; the e2e fence now pins nonzero `cache.hits` after a routed
  scan and zero added misses on the warm re-scan
  (`tests/daemon_e2e.rs::scan_daemon_responses_say_who_served_them`).

### Added (v0.3 — product truth, roadmap #1)
- **`wake` environment notes (B2):** every wake payload now carries
  `data.environment` — `{janitor, servers: [{name, available}…],
  git: {available, dirty}}`. The servers are pure PATH lookups (no
  process spawn) over the eight known language-server binaries
  (rust-analyzer, pyright, basedpyright, gopls, ruby-lsp, elixir-ls,
  ocamllsp, typescript-language-server); git presence is the same lookup
  and `dirty` is one `git status --porcelain` run (staged, unstaged, or
  untracked; false when git is missing or the directory is not a
  repository). This is the field GLOBAL.md's semantic-layer guidance
  always referenced — the promise is now true, and GLOBAL.md/SCHEMA.md
  describe the actual shape. ADR-0017 unchanged: guidance with evidence,
  never a runtime.

### Added (F10a)
- **Warm-cache daemon + watch mode** (ADR-0018,
  `docs/planning/ADR-0018-daemon.md`): `janitor daemon [--workspace
  --sock --interval --max-cache-mb --idle-secs]` — a long-lived process
  serving the same `janitor.v1` envelope over a 0600 Unix socket
  (`scan`/`duplicates`/`ping`/`stats`/`shutdown`, one request per
  connection), with a polling watcher (default 2 s; dot-dirs and
  `target/` ignored) that re-parses changed warm files **incrementally**
  (KP-1's `Tree::edit` path under the `ParseCache::take` exclusivity
  door). `janitor scan --daemon` routes through it when it answers
  `ping` for this workspace; otherwise it falls back in-process
  silently. ADR-0008 partially reversed (long-lived processes yes, tokio
  still no); ADR-0017 untouched (this is a performance layer, not
  semantic validation).
- **Measured (self-dogfood, this repo's `src/`, 52 files / 858 KB,
  debug build):** cold scan 175 ms → warm scan 102 ms (52/52 cache hits,
  0 misses; the warm pass parses nothing), ~1.7× wall on a small corpus
  where parse is not yet dominant; the e2e fence proves strict warm<cold
  on a 2 MB corpus. Warm retention accounted at 3.9 MB for 858 KB of
  source (bytes + 16 B/node tree estimate) — the `--max-cache-mb` bound
  (default 512) evicts LRU against exactly that number.
- **ParseCache grew a warm shape, additively:** exact hit/miss counters,
  optional byte bound with LRU eviction (eviction is free — later queries
  re-parse), cost accounting = snapshot bytes + node count × 16 B; the
  CLI's unbounded streaming path is unchanged and pays none of it.
- **Daemon fences** (`tests/daemon_e2e.rs`, 11 tests over real sockets,
  real subprocesses for lifecycle): warm-cache proof (byte-identical
  report cold vs warm vs in-process, modulo `stats.wall_ms`),
  incremental (watcher re-parses exactly the changed file; the next scan
  is all hits), eviction (bound respected, later queries re-parse),
  concurrency (parallel scans + pings + watcher mutations under a
  bounded wait — no torn envelopes, no deadlock), hygiene (0600;
  `..`/absolute/symlink-escape paths typed-refused), crash safety
  (`kill -9` → stale socket → next start probes, removes, serves), plus
  request validation, already-running probe, connection cap, idle death,
  and the CLI routing contract.
- New error family: `E_DAEMON_BIND`, `E_DAEMON_ALREADY_RUNNING`,
  `E_DAEMON_PATH_OUTSIDE_WORKSPACE`, `E_DAEMON_REQUEST_MALFORMED`,
  `E_DAEMON_REQUEST_TOO_LARGE`, `E_DAEMON_BUSY`, `E_DAEMON_CONNECT`,
  `E_DAEMON_PROTOCOL`.

### Added
- **MCP server (`janitor mcp`, F10b, ADR-0019):** the engine as six native
  tools over the MCP stdio transport (`janitor_wake`, `janitor_scan`,
  `janitor_apply`, `janitor_note`, `janitor_nap`, `janitor_status`) —
  thin in-process wrappers over the CLI's own session paths, zero new
  dependencies. Results carry the `janitor.v1` envelope as structured
  content (`isError` + `E_*` on refusals); per-workspace with typed
  `E_VFS_INVALID_PATH` traversal refusals; newline-delimited JSON-RPC per
  the MCP spec with LSP-style `Content-Length` frames also accepted;
  stdout stays frame-pure (pinned under `RUST_LOG=debug`). The v0.1
  agent loop is e2e-tested through the layer (`tests/mcp_e2e.rs`); setup
  and tool table in `docs/MCP.md`.
- **Python** fully supported (F9a): registry, measured trivia table
  (`comment`), norm table (`lang/python.norm.toml`), end-to-end agent-loop
  acceptance, CI leg.
- **Go** fully supported (F9b): trivia = `["comment"]`, norm table, e2e +
  determinism leg. `method_declaration` documented alongside
  `function_declaration`.
- **TypeScript + TSX** fully supported (F9b): trivia =
  `["comment", "html_comment"]`; **tsx is its own LangId** (`.ts` →
  `typescript`, `.tsx` → `tsx`); norm table covers both; e2e + determinism
  leg.
- **Experimental tier (F9c+F9d): ruby, ocaml, elixir** — registered,
  trivia-measured, norm-tabled and e2e-tested through the same ladder,
  fenced OUT of `lang-all` behind `lang-experimental` until trivia corpora
  are fully curated. One CI job covers the tier. Language decisions:
  ocaml `.ml`/`.mli` are two languages (`ocaml`, `ocaml_interface`); elixir
  `.ex`/`.exs` are one (`elixir`).
- **`Unify` + `scan --duplicates`** (F8): 128-bit SipHash-1-3 Merkle
  fingerprints over the trivia-excluded skeleton (significant anonymous
  tokens hashed — `x + y` ≠ `x - y`; renamed locals differ). `(unify @a
  @b)` compiles as a post-match filter. `--duplicates --min-size N` groups
  provably-isomorphic subtrees deterministically.
- **CI:** lang-python/go/typescript e2e legs; experimental-tier job;
  `alloc.rs` fingerprint fence.

- **Guidance:** layered-validation instructions — agents are now directed (via `share/GLOBAL.md` + `wake` hints) to run the project's language server for semantic checks; janitor's gate stays write-time syntax only (ADR-0017).
### Fixed
- `merge_oldest` re-checks the present-bias rule before creating a parent
  block (property test found `n=17, budget=23`: `[0,8)+[8,16)` → `[0,16)`
  violated `size ≤ α×age`). Found via CI proptest, minimized automatically.
- `FingerprintTable::build` pre-reserves storage from
  `descendant_count()` — the alloc fence measured 213 events (budget 128)
  from capacity doublings; now 12 flat across corpus sizes.
- Ruby trivia corpus and ocaml `.ml`/`.mli` mapping caveats documented
  (heredoc parking; `let..in` nesting).

# Changelog

All notable changes to janitor-prime are documented in this file. The format
follows [Keep a Changelog](https://keepachangelog.com/); granularity is
per-build-phase (F0..F7), because that is how the project was built and
gated.

Versioning is early 0.x (SemVer-ish): while pre-1.0, minor bumps may change
internals and CLI details. The compatibility surface that is held stable is
the **stdout contract**: every command emits the versioned `janitor.v1`
JSON envelope (see `docs/SCHEMA.md`), and changes to envelope or payload
shapes are treated as breaking changes — a new schema id, never silent
drift.

Numbers below come from the phase records in
`docs/planning/PLAN.md` (§3 outcome notes, §5 kill-point table, Appendix
A.5), the kill-point memos in `docs/planning/spikes/`, and the git history.

## [Unreleased]
- **Fixed (F8 regression):** `FingerprintTable::build` left its walk and
  result arrays unreserved, so every build re-paid its capacity doublings —
  allocation events grew with tree size (123 on a ~10.7k-node corpus, 144 on
  ~53.6k) and the KP-8 alloc fence failed at 213 > 128 (the test also
  measured its own corpus regeneration inside the window). Every array is
  now reserved up front from the root's O(1) `descendant_count`: 12 one-time
  events per build, any tree size. The fence re-asserts the structural form
  — the same constant budget on two corpus sizes (~10.7k and ~32k nodes).

## [0.1.0] - 2026-09-15

First functional cut: the agent loop — discover → stage → validate → flush →
remember — runs end to end through the real binary. One statically-linked
`janitor` binary ships three fused capabilities: a transactional overlay
VFS, a tree-sitter structural search engine driven by an s-expression DSL,
and an append-only memory with anti-hallucination syntax pointers and a
synchronous compression protocol. Only `lang-rust` is CI-tested and
supported in this version (see Known limitations).
- **Fixed (KP-11 dogfood):** `scan --pretty` now parses after the subcommand
  (docs/DSL.md §8 advertised the form; the binary rejected it with `E_USAGE`).
  Root flag made a clap global + regression test.
- **Renamed:** `AGE_ALPHA` → `PRESENT_BIAS_ALPHA` (the constant implements
  ADR-0011's present-bias rule; the old name hid it).

### F0 - Scaffold, CI, JSON envelope (2026-09-13)

- Crate scaffold: `build.rs` embeds the grammar list so `--version` emits a
  `janitor.v1` envelope (`{"schema":"janitor.v1","ok":true,"data":{"version":"0.1.0","grammars":[…]}}`).
  Exit codes 0 success / 1 operational / 2 usage; clap usage errors are
  re-rendered as envelope errors, so stdout stays machine-parseable even on
  CLI mistakes.
- CI (GitHub Actions pinned by commit SHA): fmt, clippy `-D warnings`, and
  test on both an MSRV 1.90.0 leg and a stable leg; a `pin-check` job
  building `--features lang-all` on every PR; a `musl-smoke` job producing
  the `x86_64-unknown-linux-musl` static binary via cargo-zigbuild (a
  musl-gcc fallback is wired but has not been needed — the first run passed
  via zigbuild) and smoke-testing its `--version` envelope.
- Pin-check green: all seven grammar crates resolve, compile, and link
  against tree-sitter 0.27.0 (grammars at ABI 14–15, core accepts 13..=15).
  F0 gate: 13/13 tests.
- Registry errata recorded while pinning: the crates.io name is `smol_str`
  (underscore — the dashed spelling does not resolve), and proptest /
  criterion latests are 1.11.0 / 0.8.2 (the originally planned 1.8.2 /
  0.11.0 were never published).

### F1 - Kill-point spikes (2026-09-13)

Measured before the engine was built; each result fixed a design decision.

- **KP-1, incremental re-parse**: on a 3,146,176-byte generated Rust file,
  across six edit sizes (+8 B to +131,158 B), `Tree::edit` + incremental
  re-parse beat full re-parse at every size — p50 speedup 6.0–7.4× — and the
  resulting trees were identical (whole-tree s-expression equality). No size
  cutoff is needed; the fast path stays. The shared-clone leg confirmed the
  exclusivity guard: editing while another `Arc<Tree>` handle exists fails
  with a typed error and zero mutation.
- **KP-4, validity-gate honesty**: a gate restricted to the edited byte
  ranges measured a 1/10 false-negative rate (a deleted `;` parks a MISSING
  node at the *next* statement's start, outside the half-open range). The
  `apply` gate is therefore **whole-file**: a file passes iff its re-parse
  has zero ERROR/MISSING nodes.
- **KP-6, trivia semantics**: byte-strict `seq` adjacency loses 8.1% of
  consecutive-`let` pairs in this repository's own source (114 vs 124
  matches) and 0% on the other four idioms tested; the curated
  attribute-on-fn pair is invisible under strict mode (0 vs 1). Decision
  (ADR-0010) confirmed: adjacency is over named siblings skipping
  comment-class kinds by default; `--strict-trivia` restores byte-strict.
- **KP-10, cold-parse latency**: rust-analyzer at HEAD — 1,484 files /
  16.7 MB — cold-parses in 2.21 s in a serial read+parse loop with one
  parser (672 files/s, 7.54 MB/s), peak RSS 15.3 MiB (flat, not O(repo),
  via parse-then-drop streaming). Also measured: 18.5% of those files carry
  pre-existing ERROR/MISSING nodes under tree-sitter-rust 0.24.2 —
  recorded, and `apply`'s report distinguishes pre-existing from introduced
  errors.
- **KP-2 / KP-3 fences**: compile-time `Send`/`Sync` canaries over the
  tree-sitter types plus a rayon mini-scan passing owned `Arc<Tree>` across
  threads; `Point.column` byte semantics confirmed on a multibyte
  (emoji/CJK) corpus.

### F2 - Transactional overlay VFS (2026-09-13)

- `Overlay` over `Os` with four invariant tests: anti-phantom (`read_dir`
  never lists buffer files), read-through (staged bytes win over disk),
  fallback (byte-identical to `Os` with no buffer entry), and tombstone
  delete (typed `E_VFS_BUFFER_DELETED` — a staged delete stays
  distinguishable from "never existed"; listings unchanged). Built as an
  explicit red→green pair: a stub whose `read_dir` leaked phantoms failed
  the anti-phantom test first.
- `stage`/`commit`/`rollback` transactions; commit flushes per-file
  tmp+rename ordered by path; a partial OS failure mid-flush returns the
  structured `E_FLUSH_PARTIAL` report (persisted vs rolled-back paths).
- `VfsPath` lexical canonicalization (property-tested), synthetic `stat`
  with `is_virtual`, and an explicit `virtual_paths()` surface so
  `scan --include-virtual` (default off) can see staged files without
  weakening the anti-phantom invariant.
- Concurrency: a rayon hammer over a shared overlay with a mutation-checked
  torn-read detector, plus full-suite runs under `RAYON_NUM_THREADS=1` and
  `=8`. 40 tests green across the matrix at the F2 gate.

### F3 - Parse layer over the overlay (2026-09-13)

- `ffi_shim` is the only file importing `tree_sitter`: chunked parsing from
  `Arc<[u8]>` overlay snapshots (no lock held during a parse), `LanguageFn`
  registration, byte-column `InputEdit` math, and the guarded edit path.
  A tree-sitter version swap is a one-file change.
- `ParseCache`: dashmap keyed by path, entries carry
  `{generation, bytes, tree, error_count}` and are validated by generation
  plus content hash — stale entries are never served. Streaming discipline:
  `scan` drops each entry after its file's report, so the process never
  retains O(repo) bytes+trees.
- `error_count` precomputed at parse time (the whole-file gate basis, with
  the KP-4 corpus as a test).
- Pooled-cursor traversal plus the KP-8 counting-allocator fence: zero heap
  allocations per visited node beyond result vectors.

### F4 - DSL, scoped matcher, `scan` (2026-09-14)

- S-expression DSL front-end (lexer, recursive-descent parser, elaboration
  to `under`/`seq`/`capture`/`kind`/`any-of`/wildcard) with typed errors
  carrying position, expected-token set, and a suggested fix.
- Patterns compile per language to flat programs; execution is a scoped NFA
  over each pivot's child list with an `(child_index, pc)` failure memo — a
  failed state is never retried, so per-scope cost is
  `O(children × states)`. Match-set semantics (ADR-0016): one local-maximum
  match per (pivot, start), overlaps flagged, canonical
  `(start_byte, end_byte, rule_id)` order, earliest-position capture
  priority.
- `janitor scan`: rayon walk; hidden entries never walked; binary files
  (NUL in the first 8 KiB) and oversized files (default cap 4 MiB) are
  skipped *and listed*; per-language kind resolution skips and counts whole
  languages (`skipped_for_language`) rather than silently returning zero
  matches; `unify` rules fail rule-compile with a versioned
  `E_UNSUPPORTED_VERSION` (the feature is v0.2), never a silent no-match.
- Output is byte-identical under `RAYON_NUM_THREADS=1` vs `=8`
  (CI-asserted; `wall_ms` excepted).
- `docs/DSL.md`: the complete, self-sufficient rule contract — everything
  an LLM agent needs to author rules without reading the source.
- **KP-7, matcher linearity (measured)**: doubling sibling-list width
  (n=256→2048) doubles time (×2.0 per doubling, ~4.2 µs per child p50);
  alternation depth k=8→24 stays ~linear for the scoped engine (439→649 µs
  median) while the naive backtracking baseline grows as 2^k — 1.4 ms
  already at k=16, ~360 ms extrapolated at k=24; nested-under depth d=2→16
  costs ×1.7–1.8 per doubling thanks to the `(node, entry)` scope cache and
  region-restricted evaluation (the region restriction is load-bearing:
  without it, deep rules measured quadratic). The
  `benches/matcher_linearity.rs` bench is the standing fence.
- KP-6's five curated idioms ship as regression fixtures with their exact
  measured strict/skip counts (`tests/fixtures/`).

### F5 - Transactional `apply` (2026-09-14)

- Edit payload (ADR-0014): half-open UTF-8 byte ranges into the current
  overlay-visible content, optional `expect_sha` (file-level staleness) and
  `old_text` (site-level staleness) guards, sorted non-overlapping edits
  spliced descending by offset. Stale views fail fast with
  `E_STALE_VIEW`/`E_EDIT_MISMATCH` instead of corrupting.
- Whole-file re-parse validation gate (the KP-4 outcome): success requires
  zero ERROR/MISSING nodes; `E_PARSE_INVALID` carries per-node coordinates
  in new-byte coordinates, with `in_edited_range` informational.
- All-or-nothing at the granularity of the entire payload: every file is
  validated and gated before any byte is flushed; any failure rolls every
  file back (content and generations restored, disk untouched).
  `--dry-run` validates, gates, and rolls back without flushing. A mid-flush
  OS failure reports `E_FLUSH_PARTIAL` honestly.
- Exclusivity guard on `Tree::edit`: `apply` removes the `ParseEntry` and
  takes ownership before editing, asserting `Arc::strong_count == 1` at the
  edit point; a shared-clone attempt fails with a typed error and zero
  mutation.
- Reports are byte-identical across `RAYON_NUM_THREADS=1` vs `=8`
  (`wall_ms` excepted), covered alongside golden flows, typed refusals,
  gate coordinates, and flush-failure injection in the integration suites.
- The fast path is incremental re-parse on the KP-1 numbers (6.0–7.4×);
  there is no size cutoff.

### F6 - Memory: `init` / `wake` / `note` / `nap` (2026-09-14)

- `LOG.txt`: append-only JSONL, fsync on append, position-derived ids (line
  *i* is fact *i*). A torn trailing line (crash mid-append) is quarantined
  to `QUARANTINE.txt` with a warning surfaced in the next `wake`/`note`;
  corruption before the trailing line is refused (`E_LOG_UNRECOVERABLE` —
  repair would renumber every later fact).
- `SyntaxPointer` anti-hallucination: notation pointers are stamped from
  the current tree (the span+kind must hit a real node exactly — a
  hallucinated span is refused, never repaired); stored pointers resolve
  three-way — exact hit, re-anchor (same kind and content hash within
  4096 bytes of the old span, drift recorded and the stored pointer
  updated), or `E_POINTER_DANGLING`. Commit pointers resolve through
  `git rev-parse --verify <sha>^{commit}`.
- `wake --budget`: `chars4` token estimator (declared in-band), OptMem-style
  block-tiling recall with present-bias (a block stays whole only if
  `size ≤ 8 × age`), locked facts always served raw, and a completeness
  gate — a needed-but-unwritten summary refuses with `E_NAP_PENDING`
  instead of silently serving holes. Global layer from the embedded
  `share/GLOBAL.md` plus an `$XDG_CONFIG_HOME` override; `STATE.json`
  advisory status written atomically.
- `nap`: the synchronous compression protocol — bare `nap` returns the next
  pending request in canonical build order (block, source material,
  instruction); `nap --slot … --text …` stores the agent's ≤ 256-byte
  single-line summary as a 256-byte fixed-width `TREE` record; the dense
  prefix is the build frontier (out-of-order is `E_SLOT_INVALID`;
  identical resubmission is an idempotent no-op). `TREE/` is a pure cache,
  rebuildable from `LOG.txt` alone. No summarizer exists in the binary.
- End-to-end suites: pointer re-anchor after an edit above the anchor,
  locked facts raw under a tight wake budget, nap build ordering, and
  global/local layer isolation.

### F7 - Hardening and release (2026-09-15)

- No-panic property targets over the parser surfaces — the DSL parser, the
  `apply` diff payload parser, the `LOG.txt` loader (torn/corrupt lines
  quarantine, never panic), and the `TREE` record loader. The no-panic
  property is the product's contract, so it is tested rather than claimed.
- Supply chain: cargo-deny (license allow-list MIT / Apache-2.0 / BSD /
  ISC / Unicode-3.0 / Zlib, plus an advisories gate) and cargo-audit in CI;
  a machine-generated `THIRD_PARTY.md` manifest of the full dependency
  graph (145 crates, cargo-about via `about.toml`/`about.hbs`).
- Release workflow (tag-triggered): builds
  `x86_64-unknown-linux-musl` (cargo-zigbuild),
  `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-musl`,
  `x86_64-apple-darwin`, and `aarch64-apple-darwin`; stripped binaries, a
  `SHA256SUMS` file, and artifacts attached to the GitHub release.
- `tests/v01_acceptance.rs` — the definition of done for v0.1: one scripted
  session through the real binary (init → wake empty → scan finds the
  target → apply flushes a good diff → note anchors → an edit above the
  anchor → note re-anchors → budgeted wake → the nap loop settles → final
  wake fits), asserting every payload's schema and key fields.
- Release gates: fmt, clippy `-D warnings` (default and `--all-features`),
  tests in both feature configs, `build --no-default-features`, the
  counting-allocator fence, and the acceptance test green locally and in
  CI. Full-suite count on the release branch (F7 included): **199 tests
  passing under default features, 206 under `--all-features`, 0 failed**
  (plus two `#[ignore]`d spike-measurement tests).

### Known limitations

- **One language.** Only `lang-rust` is CI-tested and supported in v0.1.
  The other six grammars (python, go, javascript, typescript, java, c)
  build and link (pin-checked against tree-sitter 0.27.0) and their trivia
  tables are seeded, but they are not conformance-tested; python is the
  first to be enabled and CI-tested (planned v0.2).
- **`nap` is a protocol, not a summarizer.** The consuming agent writes
  every summary line; the tool only sequences, validates, and stores. No
  LLM call ever happens inside the binary and nothing runs in the
  background.
- **No LSP server, watch mode, web viewer, or `Unify`/duplicate
  detection.** `unify` rules fail rule-compile with
  `E_UNSUPPORTED_VERSION` until v0.2.
- **Platforms:** linux (musl + gnu) and macOS only. No Windows support.
- **Cold parse per invocation.** No daemon and no warm or on-disk parse
  cache (`.janitor/cache/parse/` is reserved but unwritten). Measured
  acceptable in KP-10 (2.21 s serial for 1,484 files / 16.7 MB; rayon
  divides further), but a full-repo `scan` re-parses everything each run.
- **Cross-file flush is honest, not journaled.** `apply` is all-or-nothing
  while staged, but a mid-flush OS failure returns `E_FLUSH_PARTIAL`
  listing exactly what persisted and what rolled back; true cross-file
  atomicity needs a journal (post-v0.1).
- **The validity gate is whole-file.** A file with pre-existing
  ERROR/MISSING nodes (18.5% of rust-analyzer's files under
  tree-sitter-rust 0.24.2) fails the gate even for an otherwise-correct
  edit; the report's `errors_before`/`errors_after` make the reason
  visible.
- **Token estimation is `chars/4` by declaration** — the payload's
  `estimator` field is `"chars4"`; wake budgets are approximate and say so
  in-band.
- **Commit pointers shell out to the system `git`** (`git rev-parse`); no
  git library is linked, and a missing or unresolvable git fails typed.
- **Rules match structure only** — node kinds and adjacency; there is no
  text or regex matching in the v0.1 DSL. Smaller contractual edges: note
  lines are ≤ 280 bytes and summary lines ≤ 256 bytes (single line by
  discipline), scan never walks hidden (dot-prefixed) entries, and
  `Point.column` is measured in bytes, not characters.

### Security posture

- **Zero network at runtime.** No network-capable dependency is in the
  graph; the only subprocess is a local `git rev-parse` for Commit
  pointers. The release/CI workflows are build-time only.
- **No telemetry.** Diagnostics go to stderr via env-gated `tracing`;
  stdout is 100% JSON.
- **Memory stays local.** Facts live under `$PWD/.janitor/` (gitignored by
  default); the global layer reads only the embedded defaults and
  `$XDG_CONFIG_HOME/janitor/GLOBAL.md`. Nothing leaves the machine.
- **No `unsafe` in janitor code** — all C pointer management stays inside
  the upstream tree-sitter crate; clippy denies `unwrap`/`expect`/`panic`
  in production modules, and release builds use `panic = "abort"`.
- Dual-licensed **MIT OR Apache-2.0**; `NOTICE` carries provenance credits
  (OptMem — ideas and formats only, that project ships no license; refactree
  — MIT, original repository deleted, archived at pkg.go.dev; tree-sitter
  and the grammar authors).
