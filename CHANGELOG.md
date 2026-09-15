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

## [0.1.0] - 2026-09-15

First functional cut: the agent loop — discover → stage → validate → flush →
remember — runs end to end through the real binary. One statically-linked
`janitor` binary ships three fused capabilities: a transactional overlay
VFS, a tree-sitter structural search engine driven by an s-expression DSL,
and an append-only memory with anti-hallucination syntax pointers and a
synchronous compression protocol. Only `lang-rust` is CI-tested and
supported in this version (see Known limitations).

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
