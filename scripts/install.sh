#!/usr/bin/env bash
# herdmesh installer (build from local source)
set -euo pipefail

echo "== herdmesh installer"

# Check for rustup
if ! command -v rustup >/dev/null 2>&1; then
  echo "Error: 'rustup' was not found in PATH." >&2
  echo "Please install Rust using rustup from https://rustup.rs and retry." >&2
  exit 1
fi

# Check for cargo
if ! command -v cargo >/dev/null 2>&1; then
  echo "Error: 'cargo' was not found in PATH." >&2
  echo "Please install Rust and cargo via https://rustup.rs and retry." >&2
  exit 1
fi

# Locate repository root and target crate
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
CRATE_PATH="$REPO_ROOT/crates/herdmesh"

if [ ! -d "$CRATE_PATH" ]; then
  echo "Error: Directory '$CRATE_PATH' not found." >&2
  exit 1
fi

echo "Building and installing herdmesh CLI from source..."
cargo install --path "$CRATE_PATH" --locked

echo ""
echo "✔ herdmesh installed successfully!"
echo ""
echo "Next steps:"
echo "  1. Verify CLI environment:   herdmesh doctor"
echo "  2. Configure agent MCP:       see docs/agents-setup.md"
echo "  3. Create or join a room:     herdmesh room create <room-name>"
echo "  4. (Optional) herdr plugin:   herdr plugin install herdmesh"
