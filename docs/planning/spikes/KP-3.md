# KP-3 — `Point` column semantics on multibyte sources (regression fence)

| | |
|---|---|
| **Kill-point** | PLAN §5 KP-3 · §1.2.1 step 4 · risk R22 |
| **Status** | **CONFIRMED at F1 (2026-09-13)** — desk-resolved (A.3.4), fence armed |
| **Code** | `src/spikes/point_semantics.rs` |
| **Commit** | F1 work item 2 |

## Hypothesis under attack

`Point` row/column semantics are knowable and testable for multibyte
sources, and the byte-based derivation janitor will implement for
`InputEdit` (§1.2.1 step 4) round-trips against the real parser. If column
units were UTF-8 code points or UTF-16 code units, every edit after an
emoji/CJK/combining byte would corrupt silently — this is the fence against
that class of bug (upstream *did* change this once: column units were
standardized to bytes in tree-sitter 0.25, #3740).

## What was run

Corpus covering the multibyte classes that break naive math — emoji (4-byte
UTF-8), CJK (3-byte), a combining mark (`e` + U+0301, written via `\u{}`
escape so its byte length is guaranteed rather than normalization-luck),
regional indicators (🇧🇷) — plus a statement sharing a row with 33 bytes of
CJK ahead of it. All expected positions are derived by `point_at`: pure
byte math (row = newline count; column = byte − row start). Three tests:

1. `point_at_math_is_pinned` — unit pins on the derivation itself
   (newline counting, column reset, EOF clamp).
2. `cold_parse_positions_are_byte_based` — every position of the root and
   of the behind-CJK probe equals the byte derivation; the probe's column
   equals the **byte** length of its same-row prefix while being strictly
   greater than its char count (bytes ≠ chars, demonstrated, not assumed).
3. `input_edit_round_trip_is_byte_based` — two `Tree::edit` +
   `parse_with_options(old_tree)` round trips (a whole multibyte line
   inserted before the probe — row shift; one 🦀 doubled mid-line inside a
   string literal — column shift). Asserts: re-parse has zero ERROR/MISSING
   nodes, root spans exactly the new bytes, the probe moves by exactly the
   byte-length delta, and all positions re-derive from byte math against
   the new source.

## Result

All green on tree-sitter `=0.27.0` / tree-sitter-rust `=0.24.2`:

- `Point.column` is **bytes** — empirically confirmed against A.3.4.
- The §1.2.1-step-4 math (`column = byte − last line-start byte`) is the
  contract `ffi_shim` will embed; the spike's `point_at`/`input_edit_for`
  are the reference implementation to port at F3.
- Incremental re-parse after a correctly derived `InputEdit` produces a
  clean tree on multibyte content — no surprise interactions between
  byte-offset edits and multibyte lexing.

## Surprises

None. No discrepancy between derived and actual positions in any test; no
error nodes after in-language multibyte edits.

## What the fence protects

A future tree-sitter release moving column units (e.g. to UTF-16 code
units for LSP parity — the `parse_utf16_*_with_options` family already
measures those) breaks `point_at`-vs-parser equality and fails these tests
loudly. The `utf8`-vs-`utf16` split is exactly the kind of minor-version
churn R22 exists for.
