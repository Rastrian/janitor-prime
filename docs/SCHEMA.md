# janitor — JSON contract reference (`janitor.v1`)

Every command speaks one stdout contract: the versioned envelope (PLAN
§1.6, R12). This document freezes the `apply` payload and result shapes
(F5, ADR-0014) and the transaction semantics they ride on. The `scan`
report shape lives in `docs/DSL.md` §8; the memory payloads land with F6.

## The envelope

```json
{ "schema": "janitor.v1", "ok": true,  "data": { … } }
{ "schema": "janitor.v1", "ok": false,
  "error": { "code": "E_…", "message": "…", "details": { … } } }
```

`data` and `error` are mutually exclusive; each is **omitted** (not
`null`) when absent, so consumers can rely on exact key sets. Exit codes:
`0` success · `2` usage · `1` operational. Diagnostics never touch stdout
(stderr via `tracing` only).

## `janitor apply` — the edit payload (ADR-0014)

```
janitor apply --diff '<json>' | --diff <@file> | --diff-file <path|-> [--dry-run]
```

```json
{ "operations": [
    { "path": "src/x.rs",
      "expect_sha": "0f3ac1d8e5b24719",
      "edits": [
        { "start_byte": 10, "old_end_byte": 24,
          "new_text": "const",
          "old_text": "static" } ] } ] }
```

| key | presence | meaning |
|---|---|---|
| `operations[].path` | required | target file, as spelled (relative paths resolve against the process working directory; the report echoes the spelling) |
| `operations[].expect_sha` | optional | [`content_sha`](#content_sha) of the content the offsets were computed against; mismatch ⇒ `E_STALE_VIEW` |
| `operations[].edits[].start_byte` | required | first replaced byte, **half-open `[start_byte, old_end_byte)` UTF-8 byte range** into the file's current overlay-visible content |
| `operations[].edits[].old_end_byte` | required | one past the last replaced byte; `start_byte == old_end_byte` is a pure insertion |
| `operations[].edits[].new_text` | required | replacement bytes; a JSON string, therefore valid UTF-8 by the time serde hands it over (E6) |
| `operations[].edits[].old_text` | optional | the bytes the agent expects at the replaced range; mismatch ⇒ `E_EDIT_MISMATCH` (the edit-level staleness guard — `expect_sha` pins the file, `old_text` pins the site) |

Discipline the payload must already satisfy (violations are typed errors,
never silently repaired):

- edits are **sorted ascending by `start_byte`** and **non-overlapping**
  (adjacent ranges are fine — half-open ranges that touch share no byte)
  ⇒ `E_EDIT_OVERLAP`;
- offsets are in range for the current content ⇒ `E_EDIT_OUT_OF_RANGE`,
  and inverted ranges ⇒ `E_EDIT_MISMATCH`;
- ranges start and end on UTF-8 character boundaries ⇒ `E_EDIT_MISMATCH`;
- each `path` appears at most once ⇒ `E_EDIT_PAYLOAD` (janitor never
  guesses an interleaving); malformed JSON ⇒ `E_EDIT_PAYLOAD`.

Validation always runs against the file's **current** content — `expect_sha`
plus `old_text` are how an agent acting on a stale view fails fast instead
of corrupting. The payload is deliberately dumb (ADR-0014): byte edits
against verified content; all cleverness lives in re-parse validation.

### The transaction boundary is the whole diff

`apply` is **all-or-nothing at the granularity of the entire payload**
(PLAN §3 F5.5): every file in `operations` is validated and re-parse-gated
**before any byte is flushed**. One failure anywhere ⇒ nothing is flushed,
every file is rolled back to its prior content and generation, disk is
untouched. The unit of commit is the diff, not the file. Stated
differently: there is no partial-success shape other than the honest
`E_FLUSH_PARTIAL` below, which can only happen when the OS rejects a write
mid-flush (e.g. a read-only directory) — its report says exactly which
paths persisted and which were rolled back.

The validity gate is **whole-file** (KP-4): an edited file passes iff its
re-parse has zero ERROR/MISSING tree-sitter nodes — errors are not
restricted to the edited ranges, because grammar error recovery parks a
MISSING token at the *next* statement's start (measured 1/10
false-negative rate for the localized variant). On failure the typed
`E_PARSE_INVALID` carries each offending node as JSON (see below);
`in_edited_range` is informational only — the gate stays whole-file.

## `janitor apply` — the result

Success (per-file results sorted by path; deterministic under any rayon
thread count — `stats.wall_ms` is the only field allowed to vary):

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "dry_run": false,
    "files": [
      { "path": "src/a.rs", "status": "flushed", "edits": 2,
        "bytes_before": 210, "bytes_after": 232,
        "sha_before": "0f3ac1d8e5b24719", "sha_after": "9c21bf0e77a140cc",
        "errors_before": 0, "errors_after": 0 } ],
    "stats": { "files": 1, "edits": 2, "bytes_before": 210,
               "bytes_after": 232, "wall_ms": 4 } } }
```

- `status` is `"flushed"` after a real apply, `"rolled_back"` under
  `--dry-run` (validated and gated, staged, then rolled back: content and
  generations restored, disk never touched).
- `errors_before`/`errors_after` are whole-file ERROR/MISSING counts; a
  success always has `errors_after == 0` (that is the gate).
- `sha_before` of one invocation is a natural `expect_sha` for the next —
  a dry-run's `sha_before` chains into the real apply's staleness guard.

Failure is the error envelope. `E_PARSE_INVALID`'s `details.nodes` carries
the offending nodes with coordinates in the **new** byte coordinates
(points are zero-based; `column` is bytes, per tree-sitter):

```json
{ "nodes": [ { "path": "src/b.rs", "kind": "ERROR", "kind_id": 65535,
    "is_missing": false, "start_byte": 41, "end_byte": 63,
    "start_point": { "row": 3, "column": 4 },
    "end_point":   { "row": 3, "column": 26 },
    "in_edited_range": true } ] }
```

`E_FLUSH_PARTIAL`'s `details` is the flush report
(`{persisted: [path…], removed: [path…], rolled_back: [path…],
failures: [{path, error}…]}`).

## `content_sha`

16 lowercase hex characters: SipHash-1-3 (std `DefaultHasher`, zero keys)
of the file's bytes, formatted `{:016x}`. It is an identity check, not a
security property. Obtain the value from janitor's own output (any apply
report's `sha_before`/`sha_after`) rather than recomputing it — the
contract is within-tool-session consistency.

## The memory layer (F6) — `init` / `wake` / `note` / `nap`

Local memory is **always** `$PWD/.janitor/` (never `$HOME`); the global
layer reads exactly two surfaces: the embedded `share/GLOBAL.md` defaults
and their `$XDG_CONFIG_HOME/janitor/GLOBAL.md` override. Nothing ever
leaves the machine, and nothing ever runs in the background (ADR-0013):
the tool sequences, the agent writes every summary.

### On-disk layout (created by `janitor init`)

```
.janitor/
  LOG.txt          append-only JSONL facts; one per line; fsync on append
  QUARANTINE.txt   torn-tail evidence (diagnostic, never machine-read)
  TREE/2, TREE/4…  one fixed-width file per block size (summaries)
  STATE.json       advisory last-run scan/apply status (atomic writes)
```

`LOG.txt` line *i* is fact *i* — **position is identity** (ids are line
indexes; the next id is the line count). Line shape:

```json
{"v":1,"id":42,"ts":"2026-09-14T12:00:00Z","ptr":{…}|null,"text":"…","flags":[],"confidence":"low"}
```

- `text`: one line, ≤ 280 bytes (`E_NOTE_INVALID` otherwise);
- `flags`: `["architectural_lock"]` marks a fact **exempt from
  compression** — never inside a summary block, never dropped by budget
  truncation, always surfaced raw at its position;
- `confidence` (v0.3, C4): the optional field `"confidence": "low"` is
  the **agent-asserted** demotion tag — set only by `note --confidence
  low`, never detected or invented by the binary. A field, not a flag
  variant, on purpose: serde ignores unknown fields, so a v0.3.1 binary
  parses a tagged log line for line (an unknown enum variant in `flags`
  would read as corruption). Absent on untagged facts, so older readers
  and older payloads are unchanged. Tagged facts are **down-weighted,
  never filtered**: tie-broken against in the ranked serve order, dropped
  before normal facts under budget pressure, seated one split level
  coarser in the positional tiling (below) — but they still serve
  whenever the budget allows, and the log stays append-only;
- a torn *trailing* line (crash mid-append) is quarantined to
  `QUARANTINE.txt` and LOG.txt is rewritten without it, with a warning in
  the next `wake`/`note` payload; corruption before the trailing line is
  the `E_LOG_UNRECOVERABLE` refusal (repair would renumber every later
  fact).

`TREE/<size>` holds exactly-256-byte records: the summary text NUL-padded.
A level's file contains, in ascending slot order, one record per
**compressible** slot (aligned power-of-two block, fully inside the fact
range, lock-free); a slot's record index is its rank among the level's
compressible slots — a pure function of LOG.txt, so the tree is a pure
cache, rebuildable (and re-requestable via `nap`) from the log alone.
The **dense prefix is the build frontier**: out-of-order submission is
`E_SLOT_INVALID`; resubmitting a settled slot with identical text is an
idempotent no-op (different text: `E_SLOT_INVALID`). A block of size ≥ 4
summarizes its two half-size children; blocks are built bottom-up.

### `janitor note` — pointers (ADR-0012)

```
janitor note [--ptr <notation|json> | --commit <sha>] [--lock] [--confidence low] TEXT
```

The compact notation (byte offsets, split from the right so the path may
contain `:`):

```
--ptr src/lib.rs:120-480:function_item
```

A notation pointer is **stamped** from the current tree: the span+kind
must hit a real node exactly (spans come from janitor's own `scan` output
or an earlier note report — never from memory), and the content hash is
written from that node's bytes. A span that is not a node boundary is
`E_POINTER_DANGLING`: a hallucinated span is refused, never repaired.

A **stored pointer JSON** (echoed verbatim in every note report's `ptr`
and in wake items) is the re-anchorable form:

```
--ptr '{"path":"src/lib.rs","span":[0,27],"kind_name":"function_item","content_sha":"2fc7ae9ebd3514a4"}'
```

Stored-pointer resolution is three-way: exact span+hash hit ⇒ stored
unchanged; a same-kind node with the same `content_sha` within 4096 bytes
of the old span ⇒ **re-anchored** (`"reanchored": true` with the byte
`drift`, and the *stored* pointer carries the new coordinates so the next
resolution is exact); neither ⇒ `E_POINTER_DANGLING`. `content_sha` is
the same 16-hex SipHash convention as [`content_sha`](#content_sha).

`--commit <sha>` (4–64 hex chars; resolved through `git rev-parse
--verify <sha>^{commit}` as a subprocess) anchors the fact to a commit;
the stored pointer carries the resolved full sha. An unresolvable sha is
`E_COMMIT_INVALID`.

`--confidence low` (v0.3, C4) is the provenance tag — your assertion,
never the binary's: it demotes the fact at recall (see `wake` below) but
changes nothing else. Any other value is the `E_USAGE` envelope.

Success:

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "appended": true, "id": 3,
    "text": "alpha re-anchored after the insert",
    "ptr": { "path": "lib.rs", "span": [27, 54],
             "kind_name": "function_item", "content_sha": "2fc7ae9ebd3514a4" },
    "reanchored": true, "drift": 27,
    "flags": [], "confidence": "low",
    "density": { "novelty": 0.83, "window": 64 },
    "warnings": [] } }
```

`confidence` appears **only** when asserted (append-only key) — an
untagged note's report is the pre-v0.3 shape exactly.

`density` (v0.3, C2) is the redundancy signal, computed at append time:
`novelty = 1 − max Jaccard(W(this fact), W(j))` over the `window` = 64
facts that precede it, where `W` is a fact's distinct lowercase word
tokens (runs ≥ 2 chars, C1's tokenizer discipline) after a deterministic
bracket-prefix strip — leading `[…]` groups (`[optmem #N]`, dates) are
migration boilerplate and are cut before tokenizing. In `[0, 1]`; 1.0
means nothing in the window looked like this fact, near 0 means the
window already said it. Nothing is stored — the score is recomputed
from LOG alone and identical between runs; near-0 novelty is guidance
to consolidate, never a refusal (`note` always appends).

### `janitor wake --budget N [--task QUERY]` — budgeted recall

Default budget 8192 tokens; estimator `"chars4"`: each item's cost is
`ceil(bytes/4)`, and the payload's `estimate` is the sum of served item
costs. When every fact raw fits the budget, every fact is served raw and
TREE is not consulted. Otherwise the id range is tiled oldest-first with
aligned power-of-two blocks under **present-bias** (a block stays whole
only if `size ≤ 8 × age`, where age is the count of newer facts — the
newest facts are raw by construction); over budget, sibling blocks merge
into parents and then whole tiles drop from the oldest end
(`"truncated": true`); leftover budget splits the largest blocks.
Locked facts are pinned raw always. One v0.3 (C4) exception to the bias
rule: a bias-demoted block covering a `low`-confidence fact stops one
split level coarser — the cheaper seat for agent-demoted material,
firing only when the coarser slot's summary is already settled (it never
widens the `E_NAP_PENDING` gate). An untagged store tiles exactly as
before.

**`--task "<query>"` (v0.3, C1)** switches the recall to a ranked pass:
every fact is scored by store-local TF-IDF (tokens = lowercase
alphanumeric runs ≥ 2 chars; `score(q,f) = Σ_{t∈q} idf(t)·tf(t,f)` with
`idf(t) = ln(1 + N/df(t))` over the store's own facts — lexical, not
semantic, and declared in-band). The best-scoring facts are served raw
in serve order (score desc, ties to the higher confidence — a `low` tag
demotes only, C4 — then to the newer id) while their cost fits; a
score-positive fact that does not fit is dropped from raw service (a
stored summary may still cover it). Seating runs in two tiers, normal
confidence first and `low` with the leftover (C4): under budget pressure
the low facts are the first dropped, after the score ordering, which
still governs within each tier — down-weighted, never filtered. Locked
facts stay pinned raw under ranking, honestly over budget if it comes to
that. The leftover budget fills with stored summaries over the unserved
ranges — the same biased tiling, with served facts force-split out like
locks, and the same `E_NAP_PENDING` gate. Item order under `--task`: the
raw evidence first (serve order), then the compressed backdrop
oldest-first. Without `--task` the payload is byte-identical to the
positional pass above (golden fixtures pin it).

The ranked pass runs only **below the task-budget ceiling**
(`TASK_BUDGET_CEILING` = 350 tokens, chars4 — v0.4, C1-R): the KP probe
on the real 448-fact store (2026-09-19, 10 hand-labeled queries)
measured the ranked pass winning 7–8/10 at budget ≤ 300 and 0/10 at
budget ≥ 400 — at high budget the positional layout already covers the
whole store through summaries, and ordering has nothing left to buy.
At or above the ceiling `--task` serves the positional payload above
(items, estimate, `truncated` all positional) and the `task` block
reports the honest degradation: `"degraded":
"budget_at_or_above_ceiling"` with `served_scored`/`dropped_scored` 0
(nothing was scored).

Payload (flattened: the recall core plus the standing context; the
`task` block appears **only** under `--task`):

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "awake": true, "budget_tokens": 30, "estimator": "chars4",
    "estimate": 18, "truncated": false,
    "task": { "query": "daemon socket errors", "scorer": "tfidf-store",
              "served_scored": 10, "dropped_scored": 0 },
    "items": [
      { "kind": "fact", "id": 0, "text": "…",
        "ptr": { "sha": "…" }, "flags": [], "confidence": "low" },
      { "kind": "summary", "slot": "0-2",
        "covers": [0, 2], "text": "…" } ],
    "warnings": [],
    "global": { "source": "embedded", "text": "…", "estimate": 600 },
    "state": { "updated": "…", "last_scan": { … }, "last_apply": { … } },
    "hints": [ "…" ],
    "environment": {
      "janitor": "0.3.0",
      "servers": [
        { "name": "rust-analyzer", "available": true },
        { "name": "pyright", "available": false } ],
      "git": { "available": true, "dirty": false } } }
}
```

A fact item's `confidence` key appears only on tagged facts (C4,
append-only) — untagged items are the pre-v0.3 shape exactly, so the
agent can always see which served evidence is agent-demoted.

`task.query` is the query as given; `task.scorer` names the statistic
(the `"estimator": "chars4"` precedent, ADR-0011 — the scorer's limits
are auditable in-band); `served_scored`/`dropped_scored` count the
score-positive facts by raw service (`served + dropped` = the
score-positive count). Under `--task`, `truncated: true` means
**score-positive facts were dropped for budget** — the fill's shaping of
score-zero material is not a truncation the agent needs to act on.
`task.degraded` (append-only, v0.4 C1-R) appears only on a degraded
`--task` wake: the budget reached the 350-token ceiling, the payload is
the positional pass's, nothing was scored, and `truncated` carries its
positional meaning (oldest tiles dropped), not the ranked one.

`global.estimate` is informational — the budget governs the memory
payload, not the standing instructions. `state` is STATE.json's contents
(or `null`): `{ "last_scan": {"ts","files","matches","skipped"},
"last_apply": {"ts","files","edits","dry_run"} }`, written atomically by
every successful `scan`/`apply` **only when `.janitor/` exists**.

`environment` (v0.3) is the guidance surface ADR-0017 promised, as
evidence rather than a promise: `servers` carries one `{name, available}`
row per probed language server (`rust-analyzer`, `pyright`,
`basedpyright`, `gopls`, `ruby-lsp`, `elixir-ls`, `ocamllsp`,
`typescript-language-server` — a pure PATH lookup, no process spawn);
`git.available` is git on the PATH and `git.dirty` is whether one
`git status --porcelain` run over the repo listed anything (staged,
unstaged, or untracked; false when git is missing or the directory is
not a repository); `janitor` is the answering binary's version. Janitor
never starts or embeds a server — the block says what the PATH offers so
the semantic-layer advice in GLOBAL.md can key on it.

**Completeness gate:** when compression is needed and a block the layout
needs has no stored summary, `wake` refuses with `E_NAP_PENDING` — never
holes served silently. `details.slot` is the needed block;
`details.request` is the protocol's next **actionable** step (see `nap`),
which may be a descendant when the tree is not built that deep yet.

### `janitor nap` — the synchronous compression protocol

```
janitor nap                                  # the next pending request
janitor nap --slot "0-2" --text "<line>"     # store the agent's line
```

Bare `nap` returns levels-ascending, slots-ascending — the canonical
build order (so every proposed block's children are already settled):

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "pending": true, "slot": "0-2", "level": 2, "covers": [0, 2],
    "source": { "kind": "facts", "facts": [ …two FactRecords… ] },
    "instruction": "Write ONE summary line (max 256 bytes, single line) … \
Submit: janitor nap --slot 0-2 --text \"<summary>\"",
    "density": { "window": 64, "novelty": [1.0, 0.14] } } }
```

`density` (v0.3, C2) appears **only** on `"kind": "facts"` sources:
each source fact's windowed novelty (the same statistic `note` reports),
index-aligned with `source.facts`, windowed against the whole log — so
the summarizer can see which facts carry information the block's
history already had and which ones are its actual news. Children
sources are built from stored summaries, which have no log window of
their own, and carry no `density` key.

**The density floor (v0.3, C2)** — the last gate `nap --text` applies,
after every protocol check (notation, ordering, children, idempotency)
and before the write, so those refusals keep priority and an already
settled line is never re-judged. The binary never scores *meaning*; it
floors *emptiness*:

- **`E_NAP_LOW_DENSITY`** — a summary with ≤ 3 distinct words
  (tokenizer above) over a block covering ≥ 8 facts: the placeholder
  signature (`"Fatos 0-32: "` passed the byte gate on the real
  migration store; it cannot pass this one). `details` carries the
  observed counts: `slot`, `distinct_words`, `source_distinct_words`,
  `covered_facts`.
- **`density_warning`** — accepted, but the summary's distinct-word
  ratio against the block's facts is below 0.25: the settle payload
  carries `"density_warning": {"kind": "nap_low_word_ratio",
  "message", "distinct_words", "source_distinct_words", "ratio"}`
  (append-only key, absent otherwise).

**Any summary with ≥ 10 distinct words always passes** — it can be
thin (warned), never refused — so the agent/nap loop converges by
construction: rewrite the line with real words and the gate opens.
Blocks smaller than 8 facts take no refusal at all.

Summary lines are ≤ 256 bytes, single line (`E_SLOT_INVALID`
otherwise). Nothing pending:

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "pending": false, "message": "Nothing left to compress." } }
```

The loop an agent runs: `wake` → on `E_NAP_PENDING`, act on
`details.request` (or just follow bare `nap`) → `wake` again. Every
settle is permanent, the tree is finite, the loop converges; no
background routine exists, and no LLM lives in the binary.

### Memory error codes

| code | meaning |
|---|---|
| `E_NOT_INITIALIZED` | `.janitor/` missing — run `janitor init` |
| `E_ALREADY_INITIALIZED` | `init` without `--force` over an existing layer |
| `E_NOTE_INVALID` | note text discipline (empty / multi-line / > 280 bytes) |
| `E_POINTER_INVALID` | pointer notation/JSON malformed, or target file unparseable |
| `E_POINTER_DANGLING` | parsed pointer will not anchor (span gone/changed) |
| `E_COMMIT_INVALID` | commit sha not hex, git missing, or unresolvable |
| `E_SLOT_INVALID` | slot notation, ordering, idempotency, or summary text discipline |
| `E_NAP_LOW_DENSITY` | summary ≤ 3 distinct words over ≥ 8 covered facts (the placeholder signature); counts in `details` |
| `E_NAP_PENDING` | wake needs an unwritten summary; request in `details` |
| `E_LOG_CORRUPT` | (taxonomy slot) torn-tail corruption events surface as `warnings`, not errors |
| `E_LOG_UNRECOVERABLE` | corruption before the trailing line — refused, never renumbered |
| `E_TREE_CORRUPT` | TREE record structurally invalid |
| `E_BUDGET_INVALID` | `wake --budget 0` |

## Language support — python (F9a, v0.2 wave 1)

Python is the second fully-supported language (rust was the first). What
this means for the schemas above — nothing changes structurally; these
are the python-specific facts a payload consumer needs:

- **File routing**: `.py` / `.pyi` (case-insensitive) → the `python`
  grammar, which must be compiled in (`--features lang-python`; the
  release artifacts embed all seven via `lang-all`). A scan over a
  mixed repo reports per-language counts in `stats.langs` and skips
  files of languages whose grammar is absent from the build (never an
  error — the files are simply not scan candidates).
- **`skipped_for_language`** (see the scan report): a rule whose kinds
  resolve in rust but not python (e.g. `function_item`) skips python
  files and counts them there — the accounting is identical to the rust
  direction.
- **Kind names in reports** are the python grammar's own: `module` is
  the root; functions are `function_definition` (decorators wrap them
  in a `decorated_definition` — the def itself is still visible inside);
  `identifier`, `block`, `if_statement` with `elif_clause`/`else_clause`
  siblings, `comment` for trivia. The norm-table mapping for
  cross-language rules lives in `lang/python.norm.toml`
  (FUNCTION=`function_definition`, IDENT=`identifier`,
  LITERAL=`integer|float|string|…`, BLOCK=`block`), conformance-fenced
  against the grammar in CI.
- **Trivia** (ADR-0010): python's table is `comment` only. NEWLINE and
  INDENT/DEDENT are anonymous tokens (never named siblings), so
  adjacency semantics port unchanged; `line_continuation` is a named
  extra but only ever appears inside expressions, never between
  statements. Measured in the KP-6-python corpus
  (`tests/kp6_python.rs`): byte-strict loses 28.6% of method-adjacency
  pairs and 10.5% of docstring-then-return pairs on the bundled corpus.
- **The apply re-parse gate** runs under the python grammar: a diff
  that would produce `ERROR`/`MISSING` nodes anywhere in the edited
  python file is refused with `E_PARSE_INVALID` and node coordinates —
  demonstrated end to end over the bundled fixture repo in
  `tests/python_e2e.rs`.
## Language support — go (F9b, v0.2 wave 2)

Go is fully supported alongside rust and python. Same story as python:
nothing in the payloads changes structurally; these are the go-specific
facts a consumer needs:

- **File routing**: `.go` (case-insensitive) → the `go` grammar
  (`--features lang-go`; release artifacts embed it via `lang-all`).
- **Kind names in reports** are the go grammar's own: `source_file` is
  the root; plain functions are `function_declaration`, methods are
  `method_declaration` (a distinct kind — its name child is a
  `field_identifier`, not an `identifier`); a function body is
  `block > statement_list`, so **statement-adjacency rules scope to
  `statement_list`**, not `block` (a go block wraps a single
  statement_list); grouped imports are `import_declaration >
  import_spec_list > import_spec`; struct fields are
  `field_declaration` inside `field_declaration_list` (tags are
  `raw_string_literal` children). The norm table lives in
  `lang/go.norm.toml` (FUNCTION=`function_declaration`+
  `method_declaration`, IDENT=`identifier`, LITERAL includes go's named
  `true`/`false`/`nil` kinds, BLOCK=`block`), conformance-fenced in CI.
- **Trivia** (ADR-0010): go's table is `comment` only — line and block
  comments are the same kind. Distinctive measured facts: same-line
  comments are legal go between declaration children
  (`func (t T) /* note */ Name()`, `if ok /* why */ {`) and land as
  named siblings there; the else-if chain is same-line and
  comment-immune between blocks. Measured in the KP-6-go corpus
  (`tests/kp6_go.rs`): byte-strict loses 50% of file-level
  function-adjacency pairs on the bundled module (2 vs 4).
- **The apply re-parse gate** runs under the go grammar — demonstrated
  end to end over the bundled fixture module in `tests/go_e2e.rs`
  (atomic six-edit three-file rename, refused syntax-breaking diff with
  coordinates, re-anchored pointer note). One honest go shape fact for
  agents planning edits: go import lines carry package *paths*, not
  symbol names, so a symbol rename's cross-file edits are def + call
  sites — there is no import-statement edit to make (a package *move*
  is what rewrites an import line).

## Language support — typescript and tsx (F9b, v0.2 wave 2)

TypeScript is fully supported, and the crate's second grammar ships
with it. Payload shapes are unchanged; the ts-specific facts:

- **File routing and the tsx decision**: `.ts`/`.mts`/`.cts` → the
  `typescript` grammar; `.tsx` → the `tsx` grammar. The
  `lang-typescript` feature registers **two** languages (one cargo
  feature, two `LangId`s) because the plain typescript grammar does not
  parse JSX — measured, a `.tsx` file under it yields `ERROR` nodes,
  which would corrupt scans and make every valid JSX edit fail the
  apply re-parse gate as if it were broken. `--version` lists both
  (`…,"typescript","tsx",…`); `--lang tsx` scopes a scan to it; tsx
  carries its own kind table (the JSX-bearing superset, 191 vs 183
  named kinds) and shares the typescript norm table — every kind in
  `lang/typescript.norm.toml` is conformance-checked against BOTH
  grammars, so a divergence between them fails CI, not a scan.
- **Kind names in reports** are the grammar's own: `program` is the
  root; top-level declarations are usually wrapped in
  `export_statement` (the decorator, if any, is a sibling of the class
  inside the export); functions are `function_declaration` (methods:
  `method_definition` in classes, `method_signature` in interfaces,
  `abstract_method_signature` in abstract classes, `function_signature`
  for ambient/overload declarations — all six are FUNCTION in the norm
  table; `arrow_function`/`function_expression` are expression-position
  and are NOT); interfaces are `interface_declaration > interface_body`
  with `property_signature` members; bodies are `statement_block`.
  `;`-separated and newline-separated members are equally adjacent —
  separators are anonymous tokens.
- **Trivia** (ADR-0010): the table is **two** kinds — `comment` (line,
  block, JSDoc alike) and `html_comment` — for BOTH typescript and tsx.
  Measured basis: the grammar accepts Annex-B HTML-like comments
  (`<!-- … -->`) in plain `.ts` as named children between statement
  siblings, and the tsx grammar additionally parks them between
  `jsx_text` siblings inside JSX trees. `hash_bang_line` is a named
  program child but only ever at index 0 — never between siblings — so
  it is deliberately OUT of the trivia table. (This corrects the v0.1
  seeded `["comment"]` table.) Measured in the KP-6-typescript corpus
  (`tests/kp6_typescript.rs`): byte-strict loses 50% of
  exported-declaration adjacency pairs on the bundled module (2 vs 4).
- **The apply re-parse gate** runs under each file's own grammar
  (typescript for `.ts`, tsx for `.tsx`) — demonstrated end to end in
  `tests/typescript_e2e.rs`: an atomic seven-edit three-file rename
  across both grammars in one diff (including import-line edits —
  typescript's named imports carry the symbol), a refused
  syntax-breaking diff with coordinates, and a re-anchored pointer
  note.
## Language support — elixir (F9d, v0.2 wave 3, EXPERIMENTAL TIER)

Elixir is registered, e2e-tested and documented, but **experimental**
(ADR-0015 appended note): it lives in the `lang-experimental` feature,
is NOT part of `lang-all` (the stable set stays the seven-grammar ABI
fence pin-check asserts), and is CI-tested in one dedicated job instead
of the gates matrix. Promotion = full trivia-corpus curation + moving
the feature into `lang-all`. Nothing below changes any payload shape —
these are the per-language facts a consumer needs.

**File routing**: `.ex` and `.exs` → one `elixir` language (feature
`lang-elixir`, tree-sitter-elixir =0.3.5 from the elixir-lang org,
ABI 14). The two extensions are the same grammar — the distinction is
script-vs-compile at the BEAM level, not a syntax split (unlike
typescript/tsx, which needed two grammars, and ocaml's .ml/.mli
interface split).

- **The FUNCTION=call mapping decision** (the F9d measurement, cited
  from the crate's `node-types.json`): this grammar has **no dedicated
  def kind** — the brief's guessed `definition` does not exist. A named
  function is `call target: (identifier)` where the target is the
  `def`/`defp` keyword, the head `add(a, b)` is a nested `call` inside
  `arguments`, and the body is a `do_block` child. **Both clause
  shapes measure identically** — `def add(a, b), do: a + b` and
  `def add(a, b, c) do … end` are sibling `call`s. Consequence, fenced
  in the e2e acceptance's docs: FUNCTION=`call` also matches every
  ordinary invocation and `defmodule` itself; a def discriminator is
  post-v0.2 pattern work. The norm table `lang/elixir.norm.toml`
  (EXPERIMENTAL header) records IDENT=`identifier` (`alias` is the
  module namespace — the ocaml namespace-typing precedent),
  LITERAL incl. `string` (heredocs ARE strings here; `sigil` excluded,
  the ruby-regexp precedent), BLOCK=`block|body|do_block`.
- **do_block structure**: elixir's `do_block` is BOTH the do…end
  expression AND the statement suite — statements are its **direct**
  named children (no wrapper; a deliberate divergence from ruby's
  table, where `body_statement` wraps). The module suite is the
  `defmodule` call's `do_block`; `case`/`cond`/`receive`/`try` are
  also `call`+`do_block`, with `stab_clause` children (`arguments` +
  `body`); `fn … end` is `anonymous_function` (an expression — the
  python-lambda precedent keeps it out of FUNCTION). Module attributes
  (`@doc`/`@spec`/`@moduledoc`/`@unit`) are `unary_operator` nodes
  wrapping a `call`, named siblings of def clauses — content-bearing,
  deliberately NOT trivia.
- **Clause adjacency** (the F9d trivia study, measured in
  `tests/kp6_elixir.rs`): trivia = `["comment"]` (the only
  comment-class kind among the grammar's 45). Successive `def`
  clauses of the SAME name are ordinary sibling calls — there is no
  same-name special case in the grammar; a comment between clause
  heads is what byte-strict loses (corpus-wide: strict loses 42.9% of
  def-clause adjacency pairs, 4 vs 7). Two measured body facts the
  corpus pins: assignment `a = b` parses as `binary_operator`, and a
  pipe chain `x |> f() |> g()` collapses to ONE `binary_operator`
  sibling — so def bodies surface no (call, call) adjacency in the
  fixture shapes.
- **Apply gate**: demonstrated end to end in `tests/elixir_e2e.rs` —
  an atomic six-edit two-file rename (including the `@spec` mention
  and both Phoenix-shaped controller call sites), a refused
  syntax-breaking diff with coordinates (an unclosed `@doc` string:
  elixir strings run across newlines, so the ERROR node swallows the
  module tail — measured), and a re-anchored pointer note (anchored on
  a name unique in its file, because the renamed funnel name has four
  content-identical occurrences and the content-hash re-anchor is
  first-nearest).
## `janitor scan --duplicates` (F8)

Duplicate-detection mode (mutually exclusive with `--rule`). Groups every
capturable subtree at or above `--min-size` participating nodes (default 8;
comment- and formatting-invariant) by its 128-bit Merkle fingerprint, and
reports groups with ≥ 2 members (raised by `--min-members N`, v0.3 B3 —
values below 2 behave as 2).

```json
{
  "schema": "janitor.v1", "ok": true,
  "data": {
    "kind": "duplicates",
    "min_size": 8,
    "summary": {
      "kinds": [
        { "lang": "rust", "kind": "block", "groups": 2, "members": 5 },
        { "lang": "rust", "kind": "function_item", "groups": 2, "members": 5 }
      ],
      "groups": 10, "members": 25
    },
    "groups": [
      {
        "id": 0,
        "fingerprint": "6bcb9006d728…",
        "lang": "rust",
        "size": 67,
        "members": [
          { "path": "src/a.rs", "start_byte": 512, "end_byte": 900,
            "start_point": {"row": 50, "column": 0},
            "end_point": {"row": 62, "column": 0},
            "kind": "function_item", "kind_id": 188,
            "virtual_file": false, "overlap": false, "captures": [] }
        ]
      }
    ],
    "stats": { "files": 6, "bytes": 174000, "nodes": 412, "groups": 10,
               "members": 25, "langs": {"rust": 6}, "wall_ms": 3 }
  }
}
```

- **Equivalence** (ADR-0010): named-node skeleton + leaf token texts plus
  semantically-significant anonymous tokens (operators, separators);
  comments and whitespace never affect the fingerprint. `x + y` and `x - y`
  are NOT duplicates; renamed locals ARE different (leaf text is hashed).
- **Order**: groups sorted by canonical first-member span (path, start_byte),
  members within a group sorted the same way — byte-identical under any
  rayon thread count (RAYON determinism fence covers this mode).
- **Tuning**: `--min-size` raises the floor so import blocks and tiny
  repeated idioms drop out; raise it until the report is actionable.
- **`--summary` (v0.3 B3)** prepends the aggregate block shown above
  (omitted without the flag): one row per (language, root kind) with its
  group and member counts, sorted by (lang, kind), plus the group/member
  totals — the per-kind ranking that used to need jq, one envelope.
  `--min-members N` keeps only groups with ≥ N members; both flags also
  ride the daemon socket (`duplicates` request's `min_members`/`summary`)
  and the MCP `janitor_scan` tool, so a routed response honors them.
  Defaults are unchanged: without the flags the report carries no
  `summary` key and the member floor stays at 2.

## Language support — ruby & ocaml (F9c, v0.2 wave 2, EXPERIMENTAL TIER)

Ruby and ocaml are registered, e2e-tested and documented, but **experimental**
(ADR-0015 appended note): they live in the `lang-experimental` feature, are
NOT part of `lang-all` (the stable set stays the seven-grammar ABI fence
pin-check asserts), and are CI-tested in one dedicated job instead of the
gates matrix. Promotion = full trivia-corpus curation + moving the feature
into `lang-all`. Nothing below changes any payload shape — these are the
per-language facts a consumer needs.

### Ruby (`.rb` → `ruby`, feature `lang-ruby`, tree-sitter-ruby =0.23.1, ABI 14)

- **Kind names** are the grammar's own: `program` is the root; defs are
  `method` at every scope (ruby has no top-level functions — a top-level
  `def` is also a `method`; `def self.x` is `singleton_method`); bodies
  are `body_statement` (class/method/do-block bodies) and `block_body`
  (brace bodies); `attr_reader`/`attr_accessor` are plain `call`s;
  `identifier` names, `comment` trivia. Norm table `lang/ruby.norm.toml`
  (EXPERIMENTAL header): FUNCTION=`method|singleton_method`,
  IDENT=`identifier`, BLOCK=`body_statement|block_body`.
- **Trivia** (ADR-0010): `["comment"]`, measured in `tests/kp6_ruby.rs`
  — byte-strict loses 28.6% of method-adjacency pairs on the bundled
  corpus.
- **HEREDOC caveat** (the F9c measurement): the grammar splits a heredoc
  in two — `heredoc_beginning` (`<<~SQL`) is the assignment's right-hand
  side, while the body text floats as a named `heredoc_body` *extra*
  parking **between statements**. It is deliberately NOT trivia
  (content-bearing; trivia-classing it would blind `--duplicates`
  fingerprints to heredoc text), so a `seq` spanning a heredoc-using
  statement and the next statement matches in **neither** trivia mode.
  Whether the vocabulary grows a heredoc-tolerant adjacency is the open
  curation question the EXPERIMENTAL marker tracks.
- **Blocks are children, never siblings**: `do..end` attaches a
  `do_block` child (wrapping `body_statement`) to its `call`; `{ }`
  attaches a `block` child (wrapping `block_body`). Also measured: a
  bare `private` parses as an `identifier` statement — not a call, not
  trivia — and breaks method adjacency in both modes.
- **Apply gate**: demonstrated end to end in `tests/ruby_e2e.rs` (atomic
  rename, refused syntax-breaking diff with coordinates, re-anchor).

### OCaml (`.ml` → `ocaml`, `.mli` → `ocaml_interface`; feature `lang-ocaml`, tree-sitter-ocaml =0.26.0, ABI 15 ×3)

- **The ml/mli mapping decision**: the crate bundles THREE grammars
  (`ocaml`, `ocaml_interface`, `ocaml_type`). Its own tree-sitter.json
  maps `ocaml`→`ml`, `ocaml_interface`→`mli`, and gives `ocaml_type` NO
  file types — so exactly **two LangIds** register: `ocaml` and
  `ocaml_interface`. They report as separate languages in
  `stats.langs` (one scan over a mixed tree shows both), each file
  re-parses under its own grammar in `apply`, and each carries its own
  norm table (`lang/ocaml.norm.toml`, `lang/ocaml_interface.norm.toml`,
  both EXPERIMENTAL-headed). `ocaml_type` registers nothing.
- **Kind names**: `compilation_unit` is the root (both grammars); a
  top-level `let f x = e` is `value_definition > let_binding`
  (`value_name`, `parameter`s, body) — the planned `value_binding` kind
  does NOT exist in 0.26.0; module bodies are `structure`, sig bodies
  `signature`; in `.mli`, `val f : t` is a `value_specification`. Norm
  tables: FUNCTION=`value_definition` (.ml) / `value_specification`
  (.mli), IDENT=`value_name` (ocaml has no generic `identifier` — names
  are namespace-typed), BLOCK=`signature|structure`.
- **let..in caveat** (the measured surprise): `let a = … in let b = … in
  body` parses as right-nested `let_expression`s — each `in` wraps the
  rest of the chain as its body, so successive let-bindings are
  parent-and-child, never siblings, in either trivia mode. Only
  file-level (`compilation_unit`) and module-body (`structure`) items
  are true siblings. `attribute` (`[@…]`) is likewise a named extra
  that can float between items but is content — not trivia.
- **Trivia** (ADR-0010): `["comment"]` for both LangIds, measured in
  `tests/kp6_ocaml.rs` — byte-strict loses 20.0% of top-level binding
  pairs and 33.3% of val-specification pairs on the bundled corpus.
- **Apply gate caveat** (measured in `tests/ocaml_e2e.rs`): the
  whole-file re-parse gate is ERROR/MISSING-node based, and this
  grammar's scanner recovers an **unclosed comment** silently
  (`(* … ` to EOF parses with zero ERROR nodes) — that breakage class
  is NOT caught for ocaml. Deleting mandatory tokens (`in`, `let`, `=`)
  IS caught (MISSING nodes) and is what the e2e refusal demonstrates.
  ADR-0010's equivalence semantics otherwise generalize unchanged;
  this is the one per-language caveat the experimental tier records.

## The daemon (F10a, v0.2) — `janitor daemon` and the socket protocol

ADR-0018 (`docs/planning/ADR-0018-daemon.md`) is the design record. The
daemon is a long-lived process owning a warm parse cache (kept fresh by a
polling watcher with incremental re-parses) and serving the SAME
`janitor.v1` envelope over a Unix domain socket. **A Unix domain socket is
not a network**: it is a kernel-local IPC object addressable only through
the filesystem — the daemon performs zero network calls of any kind.

### Lifecycle

```
janitor daemon [--workspace DIR] [--sock PATH] [--interval SECS]
               [--max-cache-mb N] [--idle-secs N]
```

- Socket path: `$JANITOR_SOCK`, else `/tmp/janitor-$UID.sock`; file
  permissions **0600** (owner-only).
- Startup/exit: exactly two envelope lines on stdout (`listening: true`
  with the config, then `reason: "shutdown" | "idle"`); everything else
  is the socket or stderr.
- Idle death: `--idle-secs` (default 1800) without connections exits the
  process, so stale daemons die naturally. `kill -9` leaves the socket
  file; the next start probes it (a live daemon answers → typed
  `E_DAEMON_ALREADY_RUNNING`; a dead one's leftover is removed and the
  bind retried).
- The CLI never spawns the daemon in v0.2. `janitor scan --daemon`
  (opt-in) routes through the socket iff it answers `ping` AND its
  workspace covers the scan root; anything else falls back to the
  in-process path (a stderr note, never a stdout side effect). The
  fallback is no longer silent (v0.3 B4): every `--daemon` response
  says who served it — see **Provenance** below.

### Request/response

One JSON request line in, one envelope line out, one request per
connection. Unknown fields are refused (`deny_unknown_fields` — an
agent-facing protocol fails loudly on typos):

```json
{"schema":"janitor.v1","cmd":"ping"}
{"schema":"janitor.v1","cmd":"scan","rule":"(kind function_item)","path":"src",
 "lang":null,"include_virtual":false,"strict_trivia":false,"max_file_size":4194304}
{"schema":"janitor.v1","cmd":"duplicates","path":"src","min_size":8}
{"schema":"janitor.v1","cmd":"stats"}
{"schema":"janitor.v1","cmd":"shutdown"}
```

- `scan`/`duplicates` take the same options as their CLI counterparts;
  `path` is workspace-relative or absolute-inside-the-workspace (default:
  the workspace). Responses carry the exact CLI report shapes as `data`.
- `ping`/`stats` answer liveness + identity + the numbers below.
- `shutdown` answers its ack envelope, then the daemon exits.

**Provenance (v0.3 B4):** every scan/duplicates response the daemon
serves — over the socket or through `scan --daemon` — carries two keys a
plain in-process report omits:

```json
{ "served_by": "daemon", "stats": { "cache": { "hits": 52, "misses": 0 }, … } }
```

`served_by` is `"daemon"` when the warm-cache daemon produced the report
and `"process"` when the CLI fell back in-process (dead socket, no
daemon, or a daemon serving another workspace) — agents can SEE when the
daemon is not serving instead of guessing. `stats.cache` is that one
request's hit/miss delta against the daemon's warm cache (a cold routed
scan shows `misses == files, hits == 0`; a warm re-scan `hits >= files,
misses == 0`); the in-process fallback always reports
`{"hits": 0, "misses": 0}` because nothing was warm. The cumulative
counters (plus entries/bytes/evictions/bound) ride `ping`/`stats` — see
the payload below.

`ping`/`stats` `data` (abridged; every field always present):

```json
{"cmd":"ping","version":"0.2.0","pid":1234,"uptime_secs":12,
 "workspace":"/abs/workspace","sock":"/tmp/janitor-1000.sock",
 "cache":{"entries":52,"bytes":4087026,"hits":52,"misses":0,
          "evictions":0,"bound_bytes":536870912},
 "watch":{"interval_secs":2,"passes":7,"reparses":1,"warm":52}}
```

- `cache.bytes` is the accounted cost — snapshot bytes **plus** an
  estimated tree share (16 B/node, computed from the parse) — and
  `bound_bytes` is the `--max-cache-mb` budget LRU-evicted against.
  Eviction is free for correctness: a later scan simply re-parses.
- `watch.reparses` counts incremental re-parses (KP-1's `Tree::edit` path)
  performed on changed warm files between polls.

### Watch mode (ADR-0018 D2/D3)

Polling every `--interval` seconds (default 2) over the warm set —
exactly the cached paths; ignore set is dot-prefixed entries (`.git/`,
`.janitor/`) and `target/`. Detection is polling, not inotify: zero new
dependencies, portable, deterministic to test; correctness NEVER depends
on it — every served entry is re-validated against live bytes (Rule T4:
generation ∧ content-hash), so a change the watcher has not seen yet is a
miss and a fresh parse, never a stale serve.

### Error codes

`E_DAEMON_BIND`, `E_DAEMON_ALREADY_RUNNING`,
`E_DAEMON_PATH_OUTSIDE_WORKSPACE` (requests — including symlink escapes,
resolved via canonicalization — must stay inside the pinned workspace),
`E_DAEMON_REQUEST_MALFORMED` (bad JSON, wrong schema, unknown cmd/field,
mode violations), `E_DAEMON_REQUEST_TOO_LARGE` (1 MiB request cap),
`E_DAEMON_BUSY` (64-connection cap), `E_DAEMON_CONNECT`,
`E_DAEMON_PROTOCOL` (client-side).

`apply` is deliberately NOT socket-exposed in v0.2 (ADR-0018 D5): the one
destructive operation keeps the CLI's one-writer-per-invocation
discipline; the only mutating paths inside the daemon are the watcher's
re-parses and LRU eviction, both under the `ParseCache::take` exclusivity
door (Rule T1).
