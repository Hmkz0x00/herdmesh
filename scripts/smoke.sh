#!/usr/bin/env bash
# herdmesh end-to-end smoke test script
set -euo pipefail

echo "========================================="
echo " herdmesh End-to-End Smoke Test (MCP)   "
echo "========================================="

# 1. Locate herdmesh binary
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

BIN="${HERDMESH_BIN:-}"
if [ -z "$BIN" ]; then
  if [ -x "$REPO_ROOT/target/debug/herdmesh" ]; then
    BIN="$REPO_ROOT/target/debug/herdmesh"
  elif [ -x "$REPO_ROOT/target/debug/herdmesh.exe" ]; then
    BIN="$REPO_ROOT/target/debug/herdmesh.exe"
  elif [ -x "$REPO_ROOT/../hm-target/debug/herdmesh" ]; then
    BIN="$REPO_ROOT/../hm-target/debug/herdmesh"
  elif [ -x "$REPO_ROOT/../hm-target/debug/herdmesh.exe" ]; then
    BIN="$REPO_ROOT/../hm-target/debug/herdmesh.exe"
  elif command -v herdmesh >/dev/null 2>&1; then
    BIN="$(command -v herdmesh)"
  else
    echo "Error: herdmesh binary not found. Build with 'cargo build -p herdmesh --bin herdmesh' or set HERDMESH_BIN." >&2
    exit 1
  fi
fi

echo "Using binary: $BIN"

# 2. Setup temporary data dir
DATA_DIR="$(mktemp -d 2>/dev/null || mktemp -d -t 'herdmesh-smoke')"
echo "Using temporary data dir: $DATA_DIR"

DAEMON_PID=""
cleanup() {
  if [ -n "$DAEMON_PID" ] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    echo "Stopping daemon (PID $DAEMON_PID)..."
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$DATA_DIR"
}
trap cleanup EXIT INT TERM

# Unique socket name isolation
export USERNAME="smoke-$$"
export USER="smoke-$$"

# Step 1: Create room
echo ""
echo "[Step 1/6] Creating room 'alpha' with agent 'a'..."
"$BIN" --data-dir "$DATA_DIR" room create alpha --as a

# Step 2: Start daemon
echo ""
echo "[Step 2/6] Starting herdmesh daemon in background..."
"$BIN" --data-dir "$DATA_DIR" daemon &
DAEMON_PID=$!
sleep 1
if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
  echo "Error: daemon failed to start" >&2
  exit 1
fi
echo "Daemon running with PID $DAEMON_PID"

# Step 3: Run MCP session for agent 'a'
echo ""
echo "[Step 3/6] Running MCP session for agent 'a' (initialize, tools/list, room_card, ask, claim)..."
MCP_A_IN=$(cat <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-sh","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"room_card","arguments":{}}}
{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"ask","arguments":{"to":"b","question":"Smoke test question from a to b"}}}
{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"claim","arguments":{"paths":["src/x/**"],"ttl_min":10}}}
EOF
)

MCP_A_OUT=$(printf "%s\n" "$MCP_A_IN" | "$BIN" --data-dir "$DATA_DIR" mcp --agent a)

# Verify step 3 assertions
if ! echo "$MCP_A_OUT" | grep -q '"room_card"'; then
  echo "Error: tools/list did not contain 'room_card'" >&2
  exit 1
fi
echo "  ✔ tools/list verified"

if ! echo "$MCP_A_OUT" | grep -q 'alpha'; then
  echo "Error: room_card output did not contain 'alpha'" >&2
  exit 1
fi
echo "  ✔ room_card verified"

if ! echo "$MCP_A_OUT" | grep -q '"id":4'; then
  echo "Error: ask call failed" >&2
  exit 1
fi
echo "  ✔ ask sent to agent b"

if ! echo "$MCP_A_OUT" | grep -q -E 'pending|granted'; then
  echo "Error: claim for src/x/** did not return pending/granted" >&2
  exit 1
fi
echo "  ✔ claim for src/x/** registered"

# Step 4: Run MCP session for agent 'b'
echo ""
echo "[Step 4/6] Running MCP session for agent 'b' (inbox, overlapping claim)..."
MCP_B_IN=$(cat <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke-sh","version":"0.1.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"inbox","arguments":{}}}
{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"claim","arguments":{"paths":["src/x/y.rs"],"ttl_min":10}}}
EOF
)

MCP_B_OUT=$(printf "%s\n" "$MCP_B_IN" | "$BIN" --data-dir "$DATA_DIR" mcp --agent b)

# Verify step 4 assertions
if ! echo "$MCP_B_OUT" | grep -q '<<peer-untrusted'; then
  echo "Error: inbox did not wrap peer message with <<peer-untrusted>>" >&2
  exit 1
fi
if ! echo "$MCP_B_OUT" | grep -q 'Smoke test question from a to b'; then
  echo "Error: inbox did not receive ask from agent a" >&2
  exit 1
fi
echo "  ✔ inbox received peer ask wrapped as untrusted"

if ! echo "$MCP_B_OUT" | grep -q -E 'lost|pending'; then
  echo "Error: overlapping claim for src/x/y.rs was not lost or pending" >&2
  exit 1
fi
echo "  ✔ overlapping claim correctly resolved to lost/pending"

echo ""
echo "========================================="
echo " ✔ All smoke test checks PASSED!         "
echo "========================================="
