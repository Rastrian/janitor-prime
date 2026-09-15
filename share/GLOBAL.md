# janitor — standing guidance (global layer)

This file ships with janitor and is included in every `wake` payload. It is
advice for the consuming agent, not enforced behavior; janitor only checks
what its own commands verify.

## How to work with janitor

- Run `janitor wake` before starting work; work from the state it returns,
  not from assumptions about the repository.
- Record durable decisions with `janitor note` — one line, at most 280 bytes.
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
