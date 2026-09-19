# Janitor Prime — v0.3 Improvement Plan (evidence-driven, self-dogfooded)

Produced 2026-09-15 on branch `planning/v03` by a planning session that ran on
janitor itself (wake/note/scan as the substrate; every structural claim below
comes from `scan --rule`/`--duplicates` runs, every linter claim from real
clippy/cargo-doc runs, every history claim from git). Provenance appendix at
§6. Effort units: **S** (≤1 agent-day), **M** (2–4), **L** (1–2 weeks) — see
B11 for why these units are agent-calibrated, not PLAN.md §6's senior-human-
engineer days.

---

## 1. State of the union (v0.2.0)

v0.2.0 (tagged 2026-09-15 08:09, 38 h after the first commit; 133 commits on
main, 20 PR lanes, 0 reverts) ships the full BRIEF surface plus two lanes the
brief never asked for: the **F8** `unify`/`--duplicates` engine (128-bit
trivia-excluded Merkle fingerprints, `x + y ≢ x - y`), **F9** multi-language
(python, go, typescript, tsx CI-tested; ruby, ocaml, elixir behind
`lang-experimental`), the **F10a** warm-cache daemon (ADR-0018: measured cold
175 ms → warm 102 ms on this repo's 52-file src, ~1.7×; incremental watch under
the `ParseCache::take` exclusivity door; 11 e2e tests over real sockets; 8 new
`E_DAEMON_*` codes), and the **F10b** MCP server (ADR-0019: six tools over
stdio JSON-RPC, zero new dependencies, dual framing, three negotiated protocol
versions). Quality posture: `cargo clippy -D warnings` green across the feature
matrix with `deny(unwrap_used, expect_used, panic)` gates (src/lib.rs:33);
323 `#[test]`s; KP-11 dogfood gate PASSED (8 edits, 0 corrupted, wake-only
cold handoff reconstructed everything, and it surfaced a real doc/binary
contract split). The v0.1 cut-list discipline held: LSP, tokio, background
summarization, and the web viewer all stayed out.

## 2. Improvement backlog

Format: problem → evidence → proposal → effort → risk. Sources: (a) dogfood
frictions of this session, (b) scan self-investigation, (c) clippy/compiler,
(d) product-vs-BRIEF/ADR-0017, (e) ecosystem (MCP, daemon, experimental tier).

### B1. DSL leaf-text predicates — the audit questions scan cannot ask *(code)*
- **Problem:** the rule DSL matches structure only — "no regex, no string
  matching" (docs/DSL.md L17) — so the most common audit questions are
  unaskable: TODO/FIXME hunting, counting `.unwrap()` specifically, filtering
  `macro_invocation`s by name.
- **Evidence:** three of this session's planned investigations were impossible
  in scan and fell back to grep: (1) TODO/FIXME — no text atoms; (2) unwrap
  counting — `call_expression` is matchable but not filterable by method name;
  (3) `--rule macro_invocation src` returned 1382 matches with no way to
  separate `panic!` from `format!`/`assert!`. KP-11's session likewise
  verified claims with grep + cargo, not scan.
- **Proposal:** append-only v0.2 syntax `(text @cap "…")` / `(glob @cap "…")`
  as post-match filters over captures — exact + glob via std, no regex crate
  (the no-new-deps posture stands). Versioning precedent exists: `unify`
  shipped behind `E_UNSUPPORTED_VERSION` (ADR-0016, F8).
- **Effort:** M. **Risk:** M — DSL contract growth (docs/DSL.md §7 table,
  golden corpus, fuzz leg all extend); the linear-time promise must be
  restated for the filter pass (O(matches × pattern), trivially bounded).

### B2. `wake` environment notes — the guidance surface promises a field that does not exist *(code+docs)*
- **Problem:** ADR-0017's entire delivery is "documentation + guidance", but
  the guidance references a payload field that was never implemented.
- **Evidence:** share/GLOBAL.md:45-48 instructs agents: "when wake's
  environment notes say a language server is available…" — `WakePayload`
  (src/memory/context.rs:79-87) has items/global/state/hints only; `HINTS` is
  four static strings (context.rs:44-50) with zero LSP content; `grep -ri
  "language server" src/memory/` → 0 hits. The original BRIEF promised wake
  would carry "status do linter" (BRIEF.md:71) — also never shipped.
- **Proposal:** add an `environment` block to the wake payload: a
  config-declared list of semantic tools (`.janitor/config.toml` or XDG
  override) with PATH-probe results, rendered as one note per detected tool.
  Update GLOBAL.md to describe the real field. (Alternatively, decision to fix
  wording only — but the BRIEF promise argues for the field.)
- **Effort:** S. **Risk:** low — append-only key; probe is synchronous and
  cheap; ADR-0017 unchanged (still guidance, not runtime LSP).

### B3. `scan --duplicates` aggregation and filters *(code)*
- **Problem:** the default duplicate report is unfilterable noise; getting
  signal requires external jq.
- **Evidence:** `--duplicates src` (default min-size 8) → **2775 groups /
  9167 members**, dominated by expression-level noise (call_expression: 798
  groups / 2848 members). This session needed a second run at `--min-size 30`
  (243 groups / 558 members) plus a jq post-pass to rank by size and per-kind.
- **Proposal:** `--summary` (per-kind counts + top-N groups by size, one
  envelope), `--min-members N`, `--kinds k1,k2` filter flags.
- **Effort:** S. **Risk:** low.

### B4. Daemon provenance + cache telemetry *(code)*
- **Problem:** you cannot tell from a scan report whether the daemon served
  it; `stats` does not expose the counters the changelog advertises.
- **Evidence:** `scan --daemon` falls back in-process *silently* (ADR-0018 D7)
  — during this session the daemon died and a `--daemon` scan still returned
  "14 matches" with no indication it fell back; I only noticed via the socket.
  The `stats` op echoes config only (`cmd/idle_secs/interval/listening/
  max_cache_bytes/pid/sock/version/workspace`), while the CHANGELOG states
  ParseCache grew "exact hit/miss counters" — they are not surfaced.
- **Proposal:** append `served_by: "daemon"|"process"` (+ socket path when
  routed) to the scan report; extend `stats` with `cache.{entries, hits,
  misses, evictions, bytes}`.
- **Effort:** S. **Risk:** low — append-only keys, envelope versioned.

### B5. `note` ergonomics — the 280-byte wall is the top recurring friction *(code+docs)*
- **Problem:** evidence-carrying notes exceed 280 bytes; the cap rejects them
  and the agent compresses by trial and error.
- **Evidence:** E_NOTE_INVALID(>280 B) occurred 2× in the KP-11 session
  (Appendix C) and 2× in this one (4 total across the only two recorded
  dogfood sessions — the single most frequent tool refusal in both).
- **Proposal:** keep the byte cap (wake's tiling math depends on it) but:
  (1) guarantee `E_NOTE_INVALID` details carry the offending byte count and a
  head/tail preview; (2) `--stdin` input for quoting sanity; (3) GLOBAL.md
  recipe: split long evidence into numbered note pairs.
- **Effort:** S. **Risk:** low.

### B6. Consolidate the duplicated twin paths *(code)*
- **Problem:** real duplicate logic survives in otherwise clean code.
- **Evidence (scan --duplicates --min-size 30, this repo's src):**
  - `scan.rs` rows ~305 vs ~595: the sequential and `par_iter` paths duplicate
    the `ReadParsed` match handling — largest group in the repo (subtree
    size 129);
  - `content_sha` implemented **verbatim twice**: apply/diff.rs:58 and
    memory/pointer.rs:228 (both comments admit "identical convention");
  - test helper `setup()` triplicated: ast/cache.rs, ast/compile.rs,
    ast/errors.rs;
  - ast/ffi_shim.rs rows 167/185: twin 71-node arms;
  - spikes/incremental_reparse.rs ↔ spikes/validity_gate.rs: a 108-node
    function shared across spike files.
- **Proposal:** one shared `content_sha`; extract the per-candidate closure in
  scan.rs; a `#[cfg(test)]` shared setup mod. All internal; stdout unchanged
  (pinned by the determinism e2e).
- **Effort:** S/M. **Risk:** low.

### B7. Docs truth pass: rustdoc links + doc-lint gate *(code+docs)*
- **Evidence:** `cargo doc --no-deps` → 4 warnings (unresolved `Transaction`,
  `dsl::parse_rule`, `SyntaxPointer`, `pointer` module/primitive ambiguity);
  clippy pedantic: 431 in the lib, ~658 all-targets — dominated by
  `must_use_candidate` 119, `doc_markdown` 106, `cast_possible_truncation` 93,
  `missing_errors_doc` 91. Default clippy is **0 warnings** — the baseline is
  strong, so the gate should grow selectively, not chase pedantic-zero.
- **Proposal:** fix the 4 links; add `-W clippy::doc_markdown
  -W clippy::missing_errors_doc` to CI; explicitly decline the cast/must_use
  families. **Effort:** S. **Risk:** none.

### B8. MCP: prompts + outputSchema *(code)*
- **Problem:** the MCP surface is tool-complete but discovery-poor vs what
  Claude Code/Cursor users see from peer servers.
- **Evidence:** docs/MCP.md + src/mcp/mod.rs:237 — capabilities advertise
  `{tools}` only; `resources/list` answers -32601 (pinned by test,
  mcp/mod.rs:977); no prompts; tool specs carry `inputSchema` +
  `annotations` but no `outputSchema`; config snippets exist and are correct.
- **Proposal:** 2–3 MCP prompts (e.g. "structural audit" recipe — the exact
  workflow this session performed; "wake-once session start"), `outputSchema`
  per tool (the envelope shape). ADR-0019's rejection of SDK/sampling/
  elicitation stands.
- **Effort:** M. **Risk:** M — spec surface growth against the "revisited,
  not stretched" clause; keep it to prompts + outputSchema only.

### B9. Experimental tier: measurable promotion criteria + the ocaml gate hole *(code+process)*
- **Problem:** "promotion requires full trivia-corpus curation"
  (README.md:50-53) is not a measurable gate, and one language has a known
  honesty hole in the product's core promise.
- **Evidence:** ocaml unclosed comments parse with **zero ERROR nodes** and
  are NOT caught by the whole-file apply gate (docs/SCHEMA.md:616-621) — the
  only documented apply-gate false negative; `--version` lists all 12 grammars
  flat, so consumers cannot distinguish stable from experimental.
- **Proposal:** a per-language promotion checklist (curated corpus ≥ N
  fixtures, determinism leg, apply-gate honesty probes including the ocaml
  comment case); expose tier in `--version`; fix or explicitly fence the ocaml
  hole before any promotion.
- **Effort:** M. **Risk:** M — the ocaml fix may be grammar-level.

### B10. Memory bootstrap ritual *(process+docs)*
> **EXECUTADO 2026-09-19** — recipe shipped as the "Session zero (empty
> store)" section of `share/GLOBAL.md` (incl. the C4 `--confidence low`
> seeding rule).

- **Problem:** a repo with rich history wakes with an empty memory; wake-only
  reconstructability (the KP-11 standard) is left to luck.
- **Evidence:** this session's opening `wake --budget 4000` returned **2
  facts** (ids 0,1) for a 133-commit repo; reconstruction relied on
  PLAN.md/CHANGELOG, not memory. Nothing in GLOBAL.md tells a first-session
  agent to seed.
- **Proposal:** a "session zero" recipe in GLOBAL.md: on `init` (or first wake
  in a fresh clone) the agent writes 5-10 seed notes (project goal, load-
  bearing invariants with `--ptr`, key commits with `--commit`). Explicitly
  NOT auto-summarization (ADR-0013 stands).
- **Effort:** S. **Risk:** low.

### B11. Effort-unit recalibration *(process)*
- **Evidence:** PLAN.md §6 estimated v0.1 at **40-66 senior-engineer-days**
  (cross-checked by eval-2); actual wall-clock first-commit → v0.1.0 tag was
  **~32 hours**, → v0.2.0 **~38 hours** (git-mined; 133 commits, 17 lanes).
  The §6 units were honest for humans and ~25× off for agent execution.
- **Proposal:** v0.3 items are sized in agent-days (S/M/L above); PLAN.md §6
  keeps its table as a historical record with a one-paragraph errata note.
  Challenged assumption: PLAN.md §6 "Calendar: v0.1 ≈ 2.5-4 months solo".
- **Effort:** none (this document). **Risk:** optimism inflation in the other
  direction — mitigate by keeping the KP discipline (every L item carries a
  measurable kill-point).

### Also-noted, not roadmaped
- **error.rs code-table derive** — 55 `E_*` codes with triple bookkeeping
  (variant + `code()` arm + `details()` arm) and error.rs is the #2 churn
  file (12 commits). A derive macro with `#[code]` overrides would shrink it;
  the codes are test-pinned so behavior cannot drift. S/M, low risk, low
  urgency.
- **37 pedantic `items_after_statements` / 26
  `redundant_closure_for_method_calls`** — mechanical cleanups, ride along
  with B7's gate if adopted.

## 3. Prioritized roadmap (top 8, value/effort order)

| # | Item | Type | Acceptance criterion (testable) |
|---|------|------|--------------------------------|
| 1 | **B2 wake environment notes** | code | `wake` payload contains `environment` with probe results; on a machine with rust-analyzer in PATH it lists rust-analyzer; GLOBAL.md contains no reference to an unimplemented field; unit test asserts the block's shape |
| 2 | **B4 daemon provenance + telemetry** | code | `daemon_e2e.rs`: `scan --daemon` with a live socket reports `served_by:"daemon"`, with a dead socket `"process"`; `stats` returns nonzero `cache.hits` after a routed scan; zero `misses` on re-scan |
| 3 | **B3 duplicates summary/filters** | code | `--duplicates --summary src` emits per-kind counts in one envelope (no jq); `--min-members 3` returns only groups with ≥3 members (golden test) |
| 4 | **B5 note ergonomics** | code | a 300-byte note is refused with `details.bytes == 300` and a preview; `note --stdin` accepts piped text; ≤280-byte behavior unchanged (regression test) |
| 5 | **B7 docs truth pass** | code+docs | `cargo doc --no-deps` → 0 warnings; CI clippy runs with `-W doc_markdown -W missing_errors_doc` at 0 new findings |
| 6 | **B1 DSL text predicates** | code | rule `(under _ (seq (capture @m macro_invocation)))` + `(text @m "panic")` counts exactly the `panic!` sites (fixture-pinned); pre-v0.2 rules unchanged; `E_UNSUPPORTED_VERSION` path tested |
| 7 | **B6 consolidate twins** | code | `content_sha` defined once (grep count 1); the scan.rs size-129 duplicate group no longer appears at `--min-size 30`; determinism e2e byte-identical |
| 8 | **B8 MCP prompts + outputSchema** | code | `tools/list` shows `outputSchema` on all 6 tools; `prompts/list` returns ≥2 prompts; mcp_e2e covers a prompt round-trip |

B9 and B10 are scheduled opportunistically: B10 (docs-only) rides with #1;
B9 waits until after the roadmap lands — promotion is a program, not a
feature.

## 4. Anti-backlog (deliberately NOT next)

- **In-process LSP / embedding a language server** — ADR-0017 stands; the
  semantic layer belongs to rust-analyzer et al. B2 delivers the *guidance*
  ADR-0017 promised, not a runtime.
- **tokio/async runtime** — ADR-0008; the daemon proves long-lived processes
  work on std threads.
- **Built-in nap summarizer / any background automation** — ADR-0013;
  "nothing runs in the background" is a product stance, and the agent-written
  summaries are the point. B10 stays on the agent side of that line.
- **Disk parse cache** — rejected in ADR-0018; RAM-warm + LRU is measured
  sufficient (3.9 MB for 858 KB of source).
- **apply over the daemon socket** — ADR-0018 D5 deferral stands until a
  second mutating client exists; cross-process write serialization is a
  design project, not a flag.
- **New grammars** — before ruby/ocaml/elixir are promoted (B9); the stable
  four + experimental three are the funded surface.
- **clippy-pedantic-zero** — 658 findings are mostly taste families
  (`must_use_candidate`, casts); gate the doc lints (B7), decline the rest.
- **MCP elicitation/sampling/roots/resources** — ADR-0019: revisited when the
  spec forces it, not stretched to look complete. B8's prompts are inside the
  line.
- **Regex engine in the DSL** — B1 is exact+glob over captures; a regex crate
  would break the no-new-deps posture and reopen the linear-time question.
- **Auto-seeding memory from repo docs** — anti-hallucination stance: an agent
  writes the seeds (B10); janitor never invents facts.
- **Web viewer, Windows, gix** — the v0.1 cut list (PLAN §7) continues to
  hold; no evidence has arrived to reopen any of them.

## 5. Dogfood report (honest)

**What earned its keep.** `scan --rule` answered every structural count in
this plan, fast, and its report spans fed `note --ptr` — the anti-
hallucination loop is real: my first pointer guess (`--ptr src/scan.rs`) was
refused, the span taken from the scan report (`src/scan.rs:4239-4587:
function_item`) anchored cleanly. `--duplicates` found things I would not
have found by eye (the size-129 twin inside scan.rs; `content_sha` pasted
twice with confessing comments). `note`'s append-only discipline worked, and
`wake` returned a compact, honest payload (budget math never constrained a
2-fact memory).

**What was ceremony.** `--pretty` (raw JSON + jq is strictly better for an
agent); the four static `hints` strings — read once, never consulted again,
yet they ride every wake payload; GLOBAL.md's generic AST heuristics (six
normalized-vocabulary prompts I never turned into rules because the session's
questions needed text matching — see B1).

**What broke or annoyed.** (1) The 280-byte note cap rejected two
evidence-rich notes; compressing real findings into 280 bytes lost pointer
detail (B5; 4 rejections across the two recorded dogfood sessions). (2) The
pointer notation lives in an error.rs comment and SCHEMA.md, not in `note
--help` — I learned it by failing. (3) `--duplicates` at default settings is
a firehose (2775 groups) with no summary (B3). (4) The daemon fooled me once,
live: after it died, `scan --daemon` returned normal results from the silent
in-process fallback — indistinguishable from a warm hit (B4). (5) GLOBAL.md
sent me looking for "wake's environment notes" that do not exist (B2).

**The pattern, and what it implies.** The engine never failed once — zero
matcher errors, zero envelope surprises, zero parse issues across every scan.
Every friction above is a *guidance-surface* or *telemetry* failure: the
product knows things it does not say (daemon provenance, cache counters), and
says things it does not know (environment notes). v0.3's center of gravity is
therefore product truth, not engine capability — items #1-#5 of the roadmap
are all "make what exists visible and honest", and the one capability add
(B1) is justified by three failed investigations in a single session, not by
ambition.

## 6. Appendix — provenance

**janitor notes written this session** (append-only, `.janitor/LOG.txt`):
ids **2, 3, 4, 5, 6, 7, 8, 9** (+ the final plan-pointer note, id **10**).
Id 4 is pointer-anchored (`src/scan.rs:4239-4587:function_item`); ids 0-1
predate the session. The plan is reconstructable from `wake` up to the
doc-reading work itself.

**Scan rules used** (all `./target/debug/janitor scan`, repo root):

| rule / mode | scope | result |
|---|---|---|
| `--duplicates` (default min-size 8) | `src` | 2775 groups / 9167 members |
| `--duplicates --min-size 30` | `src` | 243 groups / 558 members; top groups by size: 129 (scan.rs×2), 108 (spikes×2), 72 (cache/errors), 71 (ffi_shim×2), 67 (diff/pointer), 59 (diff/validate) |
| `--rule function_item --lang rust` | `src` | 791 matches / 52 files |
| `--rule function_item --lang rust` | `src/scan.rs` | span 4239-4587 taken for note-4 anchor |
| `--rule macro_invocation --lang rust` | `src` | 1382 matches (unfilterable by name → B1) |
| `--rule try_expression --lang rust` | `src` | 317 matches |
| `--rule function_item --lang rust --daemon` | `src/cli.rs` | 14 matches (routed; later identical under dead-socket fallback → B4) |

**Where janitor could NOT answer, and the real tool used** (the gap list):
lint/semantic quality → `cargo clippy --all-targets --all-features` (0
default; ~658 pedantic; top lints listed in B7) and `cargo doc --no-deps` (4
warnings); dead/broken doc links → rustdoc; text-filtered counting (TODO,
unwrap, macro names, `#[test]`) → `grep`/`rg` (323 `#[test]` across
src+tests+benches; 33/47 src files with inline tests, the 14 without are
e2e-covered); history/velocity/churn → `git log` (133 commits; PLAN.md the
#1 churn file at 19 touches; error.rs and lang.rs 12 each); aggregation of
duplicate groups → `jq`; module sizes → `wc -l` (scan.rs 1344, lang.rs 1243,
mcp/mod.rs 1144, fingerprint.rs 889, cache.rs 798; src total 23,584 lines
ex-fixture). `cargo bloat`/`tokei`: not installed.

**Sub-agent fan-out** (read-only research, three parallel lanes): clippy/doc
sweep; git-history mining; MCP/daemon/ADR/BRIEF documentation review. All
synthesis and every scan run: this session.
