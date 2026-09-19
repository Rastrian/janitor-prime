# Easy install — janitor for coding agents

One script, two audiences, zero questions asked: **install the binary** (the
only hard prerequisite), then **wire the agent layer** (Hermes and/or Claude
Code). Target state after a successful run:

1. `janitor` on PATH (static binary or `cargo build`), `--version` answering.
2. Hermes (this machine): an MCP server entry `janitor-<project>` scoped to
   the workspace, `hermes mcp test` green.
3. Claude Code: an `.mcp.json` in the target repo (project scope, committed —
   every agent/teammate inherits it), `claude mcp list` green.

## Script: `scripts/easy-install.sh`

```sh
#!/usr/bin/env bash
# janitor easy-install: binary + agent wiring (Hermes MCP, Claude Code MCP).
# Usage: easy-install.sh [TARGET_REPO] [--from-source]
#   TARGET_REPO defaults to $PWD. --from-source builds instead of downloading.
set -euo pipefail

REPO="${1:-$PWD}"; shift || true
FROM_SOURCE=0; [ "${1:-}" = "--from-source" ] && FROM_SOURCE=1
BIN="$HOME/.local/bin/janitor"
mkdir -p "$HOME/.local/bin"

have() { command -v "$1" >/dev/null 2>&1; }

echo "== janitor easy-install (repo: $REPO) =="

# --- 1. binary -------------------------------------------------------------
if have janitor && janitor --version >/dev/null 2>&1; then
  echo "[skip] janitor already on PATH: $(command -v janitor)"
else
  if [ "$FROM_SOURCE" = 1 ] || ! have curl || ! have tar; then
    echo "[build] from source (cargo, rust grammar)"
    have cargo || { echo "ERROR: cargo not found (install rustup)"; exit 1; }
    SRC="$HOME/.local/share/janitor-prime"
    [ -d "$SRC" ] || git clone https://github.com/Rastrian/janitor-prime "$SRC"
    git -C "$SRC" pull --ff-only || true
    (cd "$SRC" && cargo build --release)
    ln -sf "$SRC/target/release/janitor" "$BIN"
  else
    echo "[dl] latest release binary (x86_64-linux-musl)"
    curl -fsSL https://github.com/Rastrian/janitor-prime/releases/latest/download/janitor-x86_64-unknown-linux-musl.tar.gz \
      | tar xz -C "$HOME/.local/bin" janitor
  fi
  case ":$PATH:" in *":$HOME/.local/bin:"*) ;; *) echo "WARN: add ~/.local/bin to PATH";; esac
fi
janitor --version

# --- 2. per-repo init ------------------------------------------------------
cd "$REPO"
[ -d .janitor ] || janitor init
echo "[ok] .janitor ready in $REPO"

# --- 3a. Claude Code wiring (project .mcp.json — committed, inherited) -----
if [ ! -f .mcp.json ]; then
  cat > .mcp.json <<JSON
{"mcpServers":{"janitor":{"command":"$BIN","args":["mcp","--workspace","$REPO"]}}}
JSON
  echo "[ok] .mcp.json written (Claude Code: approved on next session; 'claude mcp list' to verify)"
else
  echo "[skip] .mcp.json already exists"
fi

# --- 3b. Hermes wiring (user-scope MCP entry, one per workspace) -----------
if have hermes; then
  if hermes mcp list 2>/dev/null | grep -q "janitor-$REPO\|janitor"; then
    echo "[skip] hermes mcp entry exists (hermes mcp list)"
  else
    have pip3 || true
    if python3 -c "import mcp" 2>/dev/null; then :; else
      echo "[dep] pip install mcp (Hermes MCP SDK) — needed once"
      pip3 install --user mcp >/dev/null 2>&1 || pip3 install --break-system-packages --user mcp || echo "WARN: install 'mcp' SDK manually, then 'hermes mcp add' again"
    fi
    yes | hermes mcp add "janitor" --command "$BIN" --args mcp --workspace "$REPO" >/dev/null 2>&1 \
      && echo "[ok] hermes mcp entry added" || echo "[warn] hermes mcp add failed — run manually (see README)"
    hermes mcp test janitor 2>/dev/null || echo "[warn] hermes mcp test not green — check 'hermes mcp list'"
  fi
else
  echo "[skip] hermes not found on this machine"
fi

echo "== done. agent loop: wake -> scan -> apply --diff -> note -> nap (see share/GLOBAL.md) =="
```

Notes on the choices:

- **Claude Code gets a committed `.mcp.json` (project scope)** — teams and
  subagents inherit it by cloning; user-scope (`claude mcp add -s user`)
  would make the janitor a per-machine secret. The first interactive session
  asks to approve project servers; print that in the docs.
- **Hermes gets a user-scope entry** (the machine's Hermes serves many
  repos); one entry per workspace, name the entry after the repo when wiring
  more than one (`hermes mcp add janitor-<proj> ...`).
- **`init` is CLI-only** by design (ADR: no mutating tool over MCP), so the
  script runs it once per repo.
- The Hermes SDK dep (`pip install mcp`) is the one known papercut (`hermes
  setup` non-interactive does not install it); the script attempts it and
  degrades loudly, not silently.

## Agent onboarding (the part that matters more than the install)

The binary teaches its own contract: **every `wake` payload embeds
`share/GLOBAL.md`** — the standing guidance (work from wake, when `--task`
pays, note/nap discipline, confidence tagging). A fresh agent needs exactly
two instructions, which both integration files carry:

- Claude Code: `CLAUDE.md` one-liner —
  "Structural edits go through `janitor apply --diff` (dry-run first); start
  every session with `janitor wake --budget 3000`; record durable decisions
  with `janitor note`; when wake returns `E_NAP_PENDING`, settle it via
  `janitor nap` (the request payload names the slot). Full guidance is in
  every wake payload (GLOBAL.md)."
- Hermes agents: the MCP tools are self-describing (`janitor_wake/scan/
  apply/note/nap/status`); the skill/AGENTS.md one-liner is the same as
  above. Remote agents (different machine) skip MCP entirely: drive the CLI
  over SSH — GLOBAL.md teaches the contract without any MCP.

## Layout on this machine (reference install)

- Binary: `~/.local/bin/janitor` (also the frigga pattern: symlinked there,
  real binary under the repo's `target/release`).
- Global memory store: `~/hermes-memory` (the canonical lab memory; `cd
  ~/hermes-memory && janitor wake --budget 3000` reloads it).
- Fleet bots: `~/hermes-memory/bots/<bot>/` workspaces, wired per profile
  via profile config `mcp_servers.janitor.args`.
- Sync: `~/hermes-memory-sync.sh` on cron */15 (the TREE empty-dir `.gitkeep`
  enforce lives there — git cannot version an empty directory).
