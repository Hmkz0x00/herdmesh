# Real per-CLI MCP Smoke Tests

Date: 2026-11-13  
Scope: Real coding agent CLI integrations with the `herdmesh` MCP server (`herdmesh mcp --agent <name>`).

## Results Summary

| CLI | Command | Tools Called | Pass / Fail | Notes |
| :--- | :--- | :--- | :--- | :--- |
| **Claude Code** (`claude` 2.1.322) | `claude -p "<prompt>" --mcp-config <file> --strict-mcp-config --model haiku --allowedTools 'mcp__herdmesh__*'` | `room_card`, `ask` | **PASS** | Automated run via `scripts/cli-smoke.ps1`. Claude initialized stdio MCP, called `room_card` then `ask(to="b", question="ping?")`, and returned responses. |
| **Antigravity** (`agy`) | `agy --settings <file> -p "<prompt>"` | *None* | **UNVERIFIED** | `agy --settings` rejects `mcpServers` (`unknown setting "mcpServers"`). `agy` has no CLI flag for ephemeral MCP server configuration without mutating persistent user configuration (`~/.gemini/config/mcp_config.json`). |
| **Codex** (`codex-cli` 0.167.2) | `codex exec "<prompt>" --ephemeral -c 'mcp_servers.herdmesh.command=...' -c 'mcp_servers.herdmesh.args=...' -c 'mcp_servers.herdmesh.env=...'` | *None* | **PENDING** | Upstream provider rate-limited until 02:33. CLI parameter syntax verified with `codex exec -c` TOML overrides without config file edits. |

---

## 1. Claude Code Verification

### Configuration & Invocation
A temporary MCP configuration file was written to an isolated scratch path:
```json
{
  "mcpServers": {
    "herdmesh": {
      "command": "<repo-target>/release/herdmesh.exe",
      "args": ["--data-dir", "<temp-data-dir>", "mcp", "--agent", "claude-agent"],
      "env": {
        "USER": "<isolated-user>",
        "USERNAME": "<isolated-user>"
      }
    }
  }
}
```

The CLI was invoked non-interactively using `--mcp-config`:
```bash
claude -p "Call the herdmesh room_card tool, then ask agent b 'ping?' with the ask tool. Reply with the tool results only." \
  --mcp-config <scratch-path>/claude-mcp.json \
  --strict-mcp-config \
  --model haiku \
  --allowedTools 'mcp__herdmesh__*'
```

### Output Received
```text
Room card result:
room smoke
members <node-id>/claude-agent ?

Ask result to agent b:
asked id=<ask-id>
```

Both MCP tools were discovered, authorized, and executed against the background `herdmesh daemon` instance.

---

## 2. Antigravity Verification

### Investigation
Antigravity CLI (`agy`) options were inspected:
- `agy --print` / `-p`: Supports non-interactive prompting.
- `agy --settings <file>`: Accepts a JSON object of `settings.json` keys.
- Passing `"mcpServers"` via `--settings` fails with:
  ```text
  Error: --settings: unknown setting "mcpServers"
  ```
- `agy` discovers MCP servers exclusively from `~/.gemini/config/mcp_config.json` or enabled plugins (`plugins/*/mcp_config.json`).
- Because modifying global/user configuration files is forbidden in testing environments, and no CLI parameter exists for ephemeral MCP server definition, `agy` is marked **UNVERIFIED**.

---

## 3. Codex CLI Verification

### Status & Syntax
Codex CLI is temporarily rate-limited until 02:33. The exact non-interactive invocation using `--config` / `-c` TOML overrides without editing `~/.codex/config.toml` is:

```bash
codex exec "Call the herdmesh room_card tool, then ask agent b 'ping?' with the ask tool. Reply with the tool results only." \
  --ephemeral \
  -c 'mcp_servers.herdmesh.command="<repo-target>/release/herdmesh.exe"' \
  -c 'mcp_servers.herdmesh.args=["--data-dir", "<temp-data-dir>", "mcp", "--agent", "codex-agent"]' \
  -c 'mcp_servers.herdmesh.env={ USER = "<isolated-user>", USERNAME = "<isolated-user>" }'
```

CLI argument parsing and nested TOML array deserialization were validated locally with `codex exec`. The test remains **PENDING** until the rate-limit window expires.

---

## 4. Reproducing

The end-to-end flow is automated in [scripts/cli-smoke.ps1](../../scripts/cli-smoke.ps1).
Run:
```powershell
powershell -ExecutionPolicy Bypass -File scripts\cli-smoke.ps1
```
This script creates a temporary isolated data directory, launches a background daemon, executes the Claude Code MCP smoke test, and guarantees process cleanup on exit.
