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

## Supported CLIs (automatic: `herdmesh setup`)

`herdmesh setup` writes a project-level MCP entry for every CLI below whose executable is on `PATH`, running `herdmesh mcp --kind <kind> --room <room>`. It shows a diff and asks first (unless `--yes`), merges into existing files, never writes through links, and never touches user-level config. `herdmesh setup --list-clients` shows what was detected, the file each would get and the docs link.

| kind | executable | project file | format | docs |
|---|---|---|---|---|
| claude | `claude` | `.mcp.json` | JSON `mcpServers` | https://docs.claude.com/en/docs/claude-code/mcp |
| codex | `codex` | `.codex/config.toml` | TOML `[mcp_servers]` | https://developers.openai.com/codex/mcp |
| agy | `agy` | `.agents/mcp_config.json` | JSON `mcpServers` | (this guide, section 3) |
| gemini | `gemini` | `.gemini/settings.json` | JSON `mcpServers` | https://geminicli.com/docs/tools/mcp/ |
| opencode | `opencode` | `opencode.json` | JSON `mcp` (typed, command array) | https://opencode.ai/docs/mcp/ |
| cursor | `agent` | `.cursor/mcp.json` | JSON `mcpServers` | https://docs.cursor.com/context/model-context-protocol |
| copilot | `copilot` | `.github/mcp.json` | JSON `mcpServers` (typed local) | https://docs.github.com/en/copilot/using-github-copilot/using-copilot-in-the-cli |
| kiro | `kiro-cli` | `.kiro/settings/mcp.json` | JSON `mcpServers` | https://kiro.dev/docs/mcp/ |
| qwen | `qwen` | `.qwen/settings.json` | JSON `mcpServers` | https://qwenlm.github.io/qwen-code/ |
| amp | `amp` | `.amp/settings.json` | JSON key `amp.mcpServers` | https://ampcode.com/docs/mcp |
| droid | `droid` | `.factory/mcp.json` | JSON `mcpServers` | https://docs.factory.ai/cli/mcp |
| kimi | `kimi` | `.kimi-code/mcp.json` | JSON `mcpServers` | https://code.kimi.com/docs/mcp |
| devin | `devin` | `.devin/mcp_config.json` | JSON `mcpServers` | https://docs.devin.ai/cli/mcp |
| grok | `grok` | `.grok/config.toml` | TOML `[mcp_servers]` | https://docs.x.ai/tools/mcp |
| pi | `pi` | `.pi/mcp.json` | JSON `mcpServers` | https://pi.dev/docs/mcp/ |

Paths come from `docs/research/mcp-clients.md` (best-effort, from vendor docs): if one is wrong for your version, fix its row in `crates/herdmesh/src/clients.rs`.

**Manual only** (user-level config, no project file; setup prints a hint): `cline` (`~/.cline/data/settings/cline_mcp_settings.json`) and `goose` (`~/.config/goose/config.yaml`).

---

## Manual setup

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

## 4. Tell Your Agent to Use It

Configuring the MCP server makes herdmesh tools available, but an agent will not call them unless instructed. Without instructions, an agent works in isolation and appears idle on the live room board.

Add the collaboration instructions from [docs/snippets/herdmesh-collab.md](snippets/herdmesh-collab.md) to your agent's instruction file:

| Agent | Instruction File Location |
| :--- | :--- |
| **Claude Code** | `CLAUDE.md` (at repository root) |
| **OpenAI Codex** | `AGENTS.md` (at repository root or `$CODEX_HOME/AGENTS.md`) |
| **Google Antigravity** | `AGENTS.md` (at repository root or `.agents/rules/herdmesh.md`) |

See [docs/snippets/herdmesh-collab.md](snippets/herdmesh-collab.md) for the exact copy-paste snippet (<=120 words).

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

## Daemon Lifecycle & Automatic Startup

When an agent invokes `herdmesh mcp --agent <name>`, the CLI automatically calls the daemon management layer:
- If the per-user daemon is already running, the MCP server connects directly over local IPC.
- If no daemon is running, it automatically spawns a detached daemon (`herdmesh daemon --ensure`), waits for the IPC socket to be ready, and connects.

You can also run or check the daemon manually:
```bash
# Ensure daemon is running in background:
herdmesh daemon --ensure

# Run daemon in foreground (for development/logs):
herdmesh daemon
```

---

## Live TUI Board

To view real-time room state alongside your agents, launch the interactive board:
```bash
herdmesh board --as <agent-name>
```
The board displays room members, active leases, task board status, and unread notifications (designed for herdr popup/split panes).

To watch agents collaborate visually in your browser with real-time animated messages, run `herdmesh view` (or follow in terminal with `herdmesh room log <room> --follow`; see [ADR-003](adr/ADR-003-live-room-view.md)).

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
