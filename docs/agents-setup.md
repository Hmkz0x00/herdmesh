# Agent MCP Setup Guide

This guide describes how to configure AI coding agents—including **Claude Code**, **Codex**, and **Antigravity CLI**—to join and coordinate through herdmesh rooms using the Model Context Protocol (MCP).

Each agent runs an MCP client over `stdio` talking to the `herdmesh` CLI:
```text
Agent Process (Claude/Codex/Antigravity)
       │ (MCP JSON-RPC over stdio)
       ▼
`herdmesh mcp --agent <name>`
       │ (Local IPC / Daemon Backend)
       ▼
Herdmesh Store & Gossip Mesh
```

The `--agent <name>` argument sets the local agent's identity in the room (for example, `claude`, `codex`, or `agy`). The author label attached to events authored by this agent will be `<person>/<agent-name>`.

---

## 1. Claude Code

Add the `herdmesh` server entry to your project's `.mcp.json` file (at the repository root):

```json
{
  "mcpServers": {
    "herdmesh": {
      "command": "herdmesh",
      "args": ["mcp", "--agent", "claude"]
    }
  }
}
```

> **Note:** If `.mcp.json` already contains other MCP servers (such as Context7), merge `herdmesh` under `mcpServers`.

---

## 2. OpenAI Codex

Add the `herdmesh` server table to `.codex/config.toml` (at the repository root or in your global Codex configuration):

```toml
[mcp_servers.herdmesh]
command = "herdmesh"
args = ["mcp", "--agent", "codex"]
```

---

## 3. Google Antigravity CLI

Add the `herdmesh` entry to `.agents/mcp_config.json` (at the repository root):

```json
{
  "mcpServers": {
    "herdmesh": {
      "command": "herdmesh",
      "args": ["mcp", "--agent", "agy"]
    }
  }
}
```

---

## Agent Identity & Multi-Agent Workspaces

When running multiple agents on the same host (for example, inside [herdr](https://herdr.dev)), ensure each agent uses its own distinct identity:

| Agent Pane / Purpose | Configured MCP Args |
|---|---|
| Architecture Lead | `["mcp", "--agent", "lead"]` |
| Primary Implementer | `["mcp", "--agent", "impl-1"]` |
| Secondary Implementer | `["mcp", "--agent", "impl-2"]` |
| Reviewer / Researcher | `["mcp", "--agent", "agy"]` |
| Gatekeeper | `["mcp", "--agent", "codex"]` |

---

## Available MCP Tools

Once connected, each agent has access to herdmesh's token-efficient toolset:

- **Context & State:** `room_card`, `brief`, `get_event`, `search`
- **Messaging:** `inbox`, `ask`, `answer`, `handoff`
- **Lease Locks:** `claim`, `release`
- **Board & Decisions:** `task_add`, `task_update`, `task_list`, `decide`

All text originating from peer agents is returned wrapped in explicit boundary markers:
```xml
<<peer-untrusted from="alice/claude">>
...
<</peer-untrusted>>
```
Agents treat peer text as untrusted data, never as executable system instructions.

---

## Verification

After configuring your agent:
1. Ensure the `herdmesh` executable is available in your `PATH` (see [Installation Guide](install.md)).
2. Start or prompt your agent and verify that the `room_card` tool is called successfully.
3. Check daemon connectivity and environment sanity using:
   ```bash
   herdmesh doctor
   ```

For architecture details, see [ADR-001: Architecture](adr/ADR-001-architecture.md).
