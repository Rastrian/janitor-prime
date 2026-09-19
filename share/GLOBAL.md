# janitor — standing guidance (global layer)

This file ships with janitor and is included in every `wake` payload. It is
advice for the consuming agent, not enforced behavior; janitor only checks
what its own commands verify.

## How to work with janitor

- Run `janitor wake` before starting work; work from the state it returns,
  not from assumptions about the repository.
- Run `janitor wake --task "<what you are working on>"` when the session
  has a concrete question (an error message, a subsystem, a symbol),
  and give it a **tight budget** — `--budget` below 350 tokens
  (200–300 works well): ranking only pays when budget is scarce, and at
  or above 350 the ranked pass is skipped on purpose — the positional
  payload serves with `"degraded": "budget_at_or_above_ceiling"` in the
  task block, because a big positional budget already covers everything
  via summaries (measured on the real store: coverage is not ranking).
  Below the ceiling, the best-matching facts are served raw first,
  ranked by lexical TF-IDF over the store itself (the payload's `task`
  block says what was served and dropped). It trades coverage for
  relevance and says so — `truncated: true` means relevant facts were
  dropped for budget, and the plain positional recall above is
  unchanged, so re-run `wake` without `--task` when you need the full
  chronological picture, not the answer to one question. Matching is
  word-level only: give it the project's own vocabulary ("daemon
  socket", not "why does networking stall").
- Record durable decisions with `janitor note` — one line, at most 280 bytes.
- Tag a note `--confidence low` when YOU cannot vouch for it: facts
  imported from a migration or another tool (especially ones truncated
  mid-thought), claims you read but did not verify against the code, or
  secondhand summaries whose source you never saw. The tag is yours to
  assert and janitor's to honor — it never invents one. A tagged fact is
  demoted at recall (served last among equals, dropped first under
  budget, seated coarser) but never deleted: when you later verify the
  claim, append a fresh untagged note rather than trusting the old one —
  the log is append-only and the correction is a new note.
- Anchor notes to real code with pointers taken from janitor's own `scan`
  output; never invent spans, offsets, or node ids.
- When `wake` refuses with E_NAP_PENDING, run `janitor nap`, write the
  requested one-line summary (at most 256 bytes), and submit it with
  `nap --slot <lo-hi> --text "<line>"`.
- Nothing runs in the background: no daemon, no scheduler, no automatic
  summarization. Every state change happens because you ran a command.
- Facts flagged architectural_lock are load-bearing: carry them forward
  verbatim and never compress or drop them.
- The fact log is append-only; supersede an old fact by appending a new
  note, never by rewriting LOG.txt.

## Generic AST heuristics (normalized vocabulary)

These heuristics are phrased only over the normalized vocabulary — FUNCTION,
IDENT, LITERAL, BLOCK — so they apply to any enabled language. They are
prompts for you to verify with `scan`, not automated rules.

- Check a FUNCTION with many parameters; prefer grouping them or passing one options IDENT.
- Flag deep BLOCK nesting (BLOCK within BLOCK within BLOCK); prefer early returns.
- Flag repeated identical LITERALs inside a FUNCTION; prefer one named IDENT constant.
- Check IDENT pairs that differ only by a style suffix; prefer one spelling per concept.
- Flag a long FUNCTION body with many sibling BLOCKs; prefer splitting along BLOCK lines.
- Check LITERALs used as magic codes or flags; prefer an IDENT that names the intent.

## Generic lint rules

- Validate every edit with `apply --dry-run` before flushing; read the report, then flush.
- Byte offsets come from janitor reports, not from memory or manual counting.
- A failed apply changes nothing — the whole diff is all-or-nothing; retry the full diff.
- Never edit on a stale view: if expect_sha does not match, re-run `scan` and rebuild.
- Prefer one whole-diff over many small applies; batch related edits and flush once.
- Janitor validates SYNTAX only (tree-sitter sees structure, not types or resolved
  imports). For semantic truth — types, unresolved symbols, dead code — rely on the
  project's language server (rust-analyzer, pyright, gopls, ruby-lsp, elixir-ls...).
  The `wake` payload's `environment.servers` block lists which of them its PATH
  probe found (`available: true`, one row per name). When a row says available,
  treat that server's diagnostics as the semantic layer and cross-check non-obvious
  edits against it before the final flush; when none is available, say so in your
  final report. `environment.git.dirty` is true when the working tree has
  uncommitted changes (staged, unstaged, or untracked) — check it before claiming
  the tree is clean, and `environment.janitor` is the version answering you.
