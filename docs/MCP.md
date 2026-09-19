# janitor over MCP — native tools for agent clients

`janitor mcp` (F10b, ADR-0019) exposes the engine as six native tools over
the [Model Context Protocol](https://modelcontextprotocol.io) stdio
transport, so MCP clients — Claude Code, Cursor, anything speaking the
reference SDKs — drive the same loop the CLI serves: **wake → scan →
apply → note → nap**. Thin by design: no MCP SDK, no new dependencies
(ADR-0019); the tools are in-process calls into the exact orchestrator
code paths the CLI dispatches, so every payload is the frozen
`janitor.v1` envelope of [SCHEMA.md](SCHEMA.md) and [DSL.md](DSL.md) —
no new semantics.

## Setup

One server per workspace. The client launches the binary from its config;
nothing is installed beyond janitor itself.

**Claude Code** (project scope, `.mcp.json` in the repo root):

```json
{
  "mcpServers": {
    "janitor": {
      "command": "janitor",
      "args": ["mcp", "--workspace", "."]
    }
  }
}
```

or imperatively: `claude mcp add janitor -- janitor mcp --workspace .`

**Generic MCP client** (Cursor, Zed, custom) — the same shape:

```json
{
  "mcpServers": {
    "janitor": {
      "command": "/usr/local/bin/janitor",
      "args": ["mcp", "--workspace", "/abs/path/to/repo"]
    }
  }
}
```

`--workspace` defaults to the current directory. It must be an existing
directory (a bad one is refused before the session starts — stdout stays
frame-pure even then). The server is stateless: no socket, no daemon
dependency; a daemon lane (f10a) exists in parallel and any routing
through it is future work, deliberately out of this surface.

## The tools

| Tool | Arguments | Writes | Purpose |
|---|---|---|---|
| `janitor_wake` | `budget?: int`, `task?: string` | no | budgeted memory recall (facts, summaries, hints, state); `task` ranks it (TF-IDF, scorer `tfidf-store`) below the 350-token ceiling — at or above it the positional payload serves with `task.degraded` (C1-R); `E_NAP_PENDING` carries the pending request |
| `janitor_scan` | `rule?`, `duplicates?`, `path?`, `lang?`, `include_virtual?`, `strict_trivia?`, `max_file_size?`, `min_size?` | no | structural search (DSL.md) or duplicate grouping (SCHEMA.md) |
| `janitor_apply` | `diff: object`, `dry_run?: bool` | **yes** | transactional rewrite: whole-diff staged → re-parse-gated → flushed; any failure rolls everything back |
| `janitor_note` | `text: string`, `ptr?`, `commit?`, `lock?`, `confidence?: "low"` | yes (.janitor/) | append one validated fact, optionally anchored; `confidence: "low"` is the agent-asserted demotion tag (C4) |
| `janitor_nap` | `slot?`, `text?` | yes (.janitor/) | bare = next pending request; `slot`+`text` = settle |
| `janitor_status` | — | no | cheap peek: version, grammars, memory layer summary, STATE.json |

Rules of the surface:

- **Every result is the `janitor.v1` envelope** (SCHEMA.md) — in MCP
  `structuredContent`, and repeated as a text block for clients on older
  protocol dialects. Operational failures are `isError: true` plus the
  same `E_*` code and details inside the envelope; only protocol-shaped
  mistakes (unknown tool, wrong argument types) are JSON-RPC errors.
- **Paths are workspace-relative.** Absolute paths and lexical escapes
  are the typed `E_VFS_INVALID_PATH` refusal — the same no-traversal
  rule the daemon lane enforces. Containment is lexical (symlinks stay
  unresolved, the overlay's own posture).
- **`init` is CLI-side.** Create the memory layer with `janitor init` in
  the workspace (once); `janitor_status` says so when it is missing. The
  MCP surface never creates or resets `.janitor/`.
- **`janitor_apply` mutates the repo** and is annotated
  `readOnlyHint: false, destructiveHint: false` — it is staged and
  rollback-safe, not destructive. Client agents should still confirm with
  their user before a non-dry-run call, per their own policies; prefer a
  `dry_run` first when uncertain.

## Protocol notes

- JSON-RPC 2.0 over stdio. Messages are **newline-delimited** UTF-8 JSON
  per the MCP stdio transport spec (one message per line, no embedded
  newlines). LSP-style `Content-Length: N\r\n\r\n{json}` frames are also
  accepted: the session's first frame picks the mode and every response
  mirrors it (the F10b brief specified Content-Length; the published spec
  — and every real client — is newline-delimited; the server speaks both
  rather than half-implementing either, ADR-0019).
- Handshake: `initialize` (version negotiation — `2024-11-05`,
  `2025-03-26`, `2025-06-18` known; unknown requests get the server's
  latest), `notifications/initialized`, `ping`, `tools/list`,
  `tools/call`. Requests before `initialize` are refused with `-32002`.
- Stdout discipline: **nothing but MCP frames ever touches stdout** —
  janitor's diagnostics have been stderr-`tracing` since F0, and the MCP
  loop never uses the CLI's stdout emitter. The e2e suite pins this under
  `RUST_LOG=debug`.
- Frames are capped at 32 MiB; oversized or malformed frames answer with
  `-32700` and keep the session alive.

## Two reminders that are product decisions, not transport details

1. **You (the agent) write every nap summary** (ADR-0013). `janitor_nap`
   returns the pending request — the slot and the source material — and
   you compose the one-line summary, then settle it. Nothing runs in the
   background and no LLM lives in the binary. On `E_NAP_PENDING` from
   `janitor_wake`: act on `details.request`, settle via `janitor_nap`,
   wake again. The tree is finite; the loop converges.
2. **The semantic layer is your project's language server** (ADR-0017).
   Janitor gates syntax (whole-file re-parse on every apply); types,
   resolved imports and borrow checks belong to rust-analyzer, pyright,
   gopls, etc. Run them alongside; janitor does not wrap them.
