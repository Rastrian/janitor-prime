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
