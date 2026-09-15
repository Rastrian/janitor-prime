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
{"v":1,"id":42,"ts":"2026-09-14T12:00:00Z","ptr":{…}|null,"text":"…","flags":[]}
```

- `text`: one line, ≤ 280 bytes (`E_NOTE_INVALID` otherwise);
- `flags`: `["architectural_lock"]` marks a fact **exempt from
  compression** — never inside a summary block, never dropped by budget
  truncation, always surfaced raw at its position;
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
janitor note [--ptr <notation|json> | --commit <sha>] [--lock] TEXT
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

Success:

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "appended": true, "id": 3,
    "text": "alpha re-anchored after the insert",
    "ptr": { "path": "lib.rs", "span": [27, 54],
             "kind_name": "function_item", "content_sha": "2fc7ae9ebd3514a4" },
    "reanchored": true, "drift": 27,
    "flags": [], "warnings": [] } }
```

### `janitor wake --budget N` — budgeted recall

Default budget 8192 tokens; estimator `"chars4"`: each item's cost is
`ceil(bytes/4)`, and the payload's `estimate` is the sum of served item
costs. When every fact raw fits the budget, every fact is served raw and
TREE is not consulted. Otherwise the id range is tiled oldest-first with
aligned power-of-two blocks under **present-bias** (a block stays whole
only if `size ≤ 8 × age`, where age is the count of newer facts — the
newest facts are raw by construction); over budget, sibling blocks merge
into parents and then whole tiles drop from the oldest end
(`"truncated": true`); leftover budget splits the largest blocks.
Locked facts are pinned raw always.

Payload (flattened: the recall core plus the standing context):

```json
{ "schema": "janitor.v1", "ok": true, "data": {
    "awake": true, "budget_tokens": 30, "estimator": "chars4",
    "estimate": 18, "truncated": false,
    "items": [
      { "kind": "fact", "id": 0, "text": "…",
        "ptr": { "sha": "…" }, "flags": [] },
      { "kind": "summary", "slot": "0-2",
        "covers": [0, 2], "text": "…" } ],
    "warnings": [],
    "global": { "source": "embedded", "text": "…", "estimate": 600 },
    "state": { "updated": "…", "last_scan": { … }, "last_apply": { … } },
    "hints": [ "…" ] } }
```

`global.estimate` is informational — the budget governs the memory
payload, not the standing instructions. `state` is STATE.json's contents
(or `null`): `{ "last_scan": {"ts","files","matches","skipped"},
"last_apply": {"ts","files","edits","dry_run"} }`, written atomically by
every successful `scan`/`apply` **only when `.janitor/` exists**.

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
Submit: janitor nap --slot 0-2 --text \"<summary>\"" } }
```

Blocks with more than 16 facts carry `"kind": "children"` — the two child
summaries — as source. Summary lines are ≤ 256 bytes, single line
(`E_SLOT_INVALID` otherwise). Nothing pending:

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
## `janitor scan --duplicates` (F8)

Duplicate-detection mode (mutually exclusive with `--rule`). Groups every
capturable subtree at or above `--min-size` participating nodes (default 8;
comment- and formatting-invariant) by its 128-bit Merkle fingerprint, and
reports groups with ≥ 2 members.

```json
{
  "schema": "janitor.v1", "ok": true,
  "data": {
    "kind": "duplicates",
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
    "stats": { "files": 1, "bytes": 29000, "wall_ms": 3,
               "files_matched": 1, "matches": 2, "langs": {"rust": 1} }
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
