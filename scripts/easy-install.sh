#!/usr/bin/env bash
# janitor easy-install: binary + agent wiring (Hermes MCP, Claude Code MCP).
# Usage: easy-install.sh [TARGET_REPO] [--from-source]
#   TARGET_REPO defaults to $PWD. --from-source builds instead of downloading.
set -euo pipefail

REPO="${1:-$PWD}"
FROM_SOURCE=0
for arg in "$@"; do
  case "$arg" in
    --from-source) FROM_SOURCE=1 ;;
    -h|--help) echo "usage: easy-install.sh [TARGET_REPO] [--from-source]"; exit 0 ;;
  esac
done
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
    have cargo || { echo "ERROR: cargo not found (install rustup first)"; exit 1; }
    SRC="$HOME/.local/share/janitor-prime"
    if [ ! -d "$SRC" ]; then
      git clone --depth 1 https://github.com/Rastrian/janitor-prime "$SRC"
    else
      git -C "$SRC" pull --ff-only || true
    fi
    (cd "$SRC" && cargo build --release)
    ln -sf "$SRC/target/release/janitor" "$BIN"
  else
    echo "[dl] latest release binary (x86_64-linux-musl, static)"
    curl -fsSL https://github.com/Rastrian/janitor-prime/releases/latest/download/janitor-x86_64-unknown-linux-musl.tar.gz \
      | tar xz -C "$HOME/.local/bin" janitor
  fi
  case ":$PATH:" in
    *":$HOME/.local/bin:"*) ;;
    *) echo "WARN: ~/.local/bin is not in PATH (add it to your shell rc)" ;;
  esac
fi
janitor --version

# --- 2. per-repo init (CLI-only by design; MCP tools never mutate layout) --
if [ ! -d "$REPO/.janitor" ]; then
  (cd "$REPO" && janitor init)
  echo "[ok] .janitor initialized in $REPO"
else
  echo "[skip] .janitor already present in $REPO"
fi

# --- 3a. Claude Code wiring: committed project .mcp.json -------------------
if [ ! -f "$REPO/.mcp.json" ]; then
  cat > "$REPO/.mcp.json" <<JSON
{"mcpServers":{"janitor":{"command":"$BIN","args":["mcp","--workspace","$REPO"]}}}
JSON
  echo "[ok] .mcp.json written (commit it — teammates/agents inherit; approve the server on next 'claude' session; verify: claude mcp list)"
else
  echo "[skip] $REPO/.mcp.json already exists"
fi

# --- 3b. Hermes wiring: user-scope MCP entry (this machine's Hermes) -------
if have hermes; then
  if hermes mcp list 2>/dev/null | grep -qi janitor; then
    echo "[skip] hermes mcp already has a janitor entry"
  else
    if ! python3 -c "import mcp" >/dev/null 2>&1; then
      echo "[dep] installing the MCP SDK for hermes (one-time)"
      pip3 install --user mcp >/dev/null 2>&1 \
        || pip3 install --break-system-packages --user mcp >/dev/null 2>&1 \
        || echo "WARN: could not pip-install 'mcp' — do it manually, then: hermes mcp add"
    fi
    if yes | hermes mcp add janitor --command "$BIN" --args mcp --workspace "$REPO" >/dev/null 2>&1; then
      echo "[ok] hermes mcp entry added (verify: hermes mcp test janitor)"
    else
      echo "[warn] hermes mcp add failed — manual: hermes mcp add janitor --command $BIN --args mcp --workspace $REPO"
    fi
    hermes mcp test janitor >/dev/null 2>&1 && echo "[ok] hermes mcp test: connected" || echo "[warn] hermes mcp test not green"
  fi
else
  echo "[skip] hermes not found on this machine"
fi

cat <<'EOF'
== done ==
Agent loop (full guidance ships inside every wake payload — share/GLOBAL.md):
  janitor wake --budget 3000                 # session start: reload state
  janitor wake --task "..." --budget 300     # one question, tight budget
  janitor scan --rule "(under function_item identifier)" --lang rust src/
  janitor apply --diff                       # transactional, syntax-gated (dry-run first)
  janitor note "decision" [--ptr span|--commit sha] [--confidence low]
  janitor nap                                # settle E_NAP_PENDING requests it names
Structural edits go through apply (never sed); summaries are written by you.
EOF
