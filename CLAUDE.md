# janitor-prime

The repo IS the product: docs are load-bearing. Read `docs/planning/PLAN.md`
(ADRs decided most questions) and `docs/planning/PLAN-v0.3.md` /
`docs/planning/PLAN-C2C.md` before proposing changes.

## janitor (agent memory + structural edits)

- Start sessions with `janitor wake --budget 3000`; for one question,
  `janitor wake --task "<query>" --budget 300` (ceiling: 350).
- Structural edits go through `janitor apply --diff` (dry-run first). Never sed
  structural changes.
- Record durable decisions: `janitor note "<fact>"` (+ `--ptr` span from scan,
  `--commit`; `--confidence low` when unverified). Cap: 280 bytes.
- On `E_NAP_PENDING`, settle via `janitor nap` (the request names the slot; you
  write the summary).
Full guidance ships inside every wake payload (share/GLOBAL.md). MCP tools:
janitor_wake/scan/apply/note/nap/status.

## Hard rules (from PLAN.md — do not regress)

- deny(clippy::unwrap_used, expect_used, panic); zero unsafe in prod code.
- No new dependencies; no regex crate; no tokio; zero network.
- Tests must not dirty the worktree; benches/ are frozen F1 evidence.
- `export PATH=$HOME/.cargo/bin:$PATH` before cargo (apt cargo 1.75 precedes
  rustup and breaks edition-2024).
