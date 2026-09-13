# KP-4 — Is the localized validity gate honest?

| | |
|---|---|
| **Kill-point** | PLAN §5 KP-4 · §1.2.1 step 7 · §1.6 · risk R7 · Q3 |
| **Status** | **MEASURED at F1 (2026-09-13) — localized gate FN 1/10; recommend whole-file gate** |
| **Code** | `src/spikes/validity_gate.rs` |
| **Data** | `benches/spike-kp4-outcome.json` |
| **Commit** | F1 work item 4 |

## Hypothesis under attack

"No ERROR/MISSING nodes within the edited byte ranges after re-parse" is an
honest `apply` validity gate — i.e. grammar error-recovery parks its error
nodes where the edit was, not arbitrarily far away.

## Setup

10-file mangled corpus, one distinct error class per file (deleted fn
brace, deleted semicolon, deleted call paren, missing initializer, extra
open brace, deleted `=`, deleted match `=>`, unterminated string, deleted
array `]`, deleted fn name). Each pristine file is asserted to parse clean
first; the edit is applied through the F5 choreography shape (exclusively
owned tree → `Tree::edit` → incremental re-parse). Counted: ERROR/MISSING
(a) anywhere vs (b) restricted to the edited range in new-byte coordinates
— the exact gate §1.2.1 step 7 specifies.

## Results

| file | errors (in-range) | missing (in-range) | FN |
|---|---|---|---|
| deleted-fn-close-brace | 1 (1) | 0 (0) | no |
| deleted-let-semicolon | 0 (0) | **1 (0)** | **YES** |
| deleted-call-close-paren | 1 (1) | 0 (0) | no |
| missing-initializer | 1 (1) | 0 (0) | no |
| extra-open-brace | 1 (1) | 0 (0) | no |
| deleted-let-equals | 1 (1) | 0 (0) | no |
| deleted-match-arrow | 1 (1) | 0 (0) | no |
| unterminated-string | 1 (1) | 0 (0) | no |
| deleted-array-close-bracket | 0 (0) | 1 (1) | no |
| deleted-fn-name | 2 (1) | 0 (0) | no |

**False negatives: 1/10 files with real errors (rate 0.10).**

9/10 land an error node inside the edited range (min distance 0 — error
recovery is local, as hoped). The one miss is instructive: deleting a `;`
makes tree-sitter report a **MISSING `;` node positioned at the start of
the *next* statement** — byte-adjacent to the edit range (distance 0) but
outside the half-open `[start, new_end)` gate. `deleted-fn-name` also shows
a second error node escaping the range, though the first still catches it.

## Verdict

**FN rate > 0 ⇒ take PLAN §5 KP-4's whole-file branch: the apply gate
counts ERROR/MISSING over the whole re-parsed tree, not just edited
ranges.** Rationale:

- 10% silent corruption-slipping-through is exactly the dishonesty R7
  exists for; a "valid" verdict must mean the file re-parses clean.
- The cost is nil by construction: `ParseEntry.error_count` is precomputed
  at parse time (§1.2 Rule T2 sketch) — the gate is `new_count == 0`, no
  extra walk. (The localized walk is *more* work than the whole-file count,
  not less.)
- The measured failure mode (MISSING node at next-statement start,
  distance 0) suggests a "range extended to the next sibling" gate would
  also work, but it is fiddlier to specify and still leaves unmeasured
  classes; whole-file is simpler and strictly safer. §1.6/§1.2.1 step 7 and
  F5's tests move to whole-file semantics; the offending-node coordinates
  in `E_PARSE_INVALID` stay as the report payload.
