# Research: Agent CLI MCP Support (herdr-supported kinds)

Evaluation of 14 agent CLIs supported by herdr for Model Context Protocol (MCP) stdio client integration, project-level config paths, binary names, and trust requirements.

## Comparison Matrix

| CLI | Stdio MCP | Project Config Path | Executable | Per-Project Trust / Approval | Official Docs |
|---|---|---|---|---|---|
| **gemini** | Yes | `.gemini/settings.json` | `gemini` | Workspace Trust prompt ("Trust folder") on first run | [geminicli.com](https://geminicli.com/docs/tools/mcp/) |
| **opencode** | Yes | `opencode.json` | `opencode` | Interactive tool permission prompt (`"permission": "ask"`) | [opencode.ai](https://opencode.ai/docs/mcp/) |
| **cursor** | Yes | `.cursor/mcp.json` | `agent` | Requires manual enable (`agent mcp enable <name>` / toggle) | [docs.cursor.com](https://docs.cursor.com/context/model-context-protocol) |
| **copilot** | Yes | `.github/mcp.json` | `copilot` | Workspace directory trust prompt on first run | [docs.github.com](https://docs.github.com/en/copilot/using-github-copilot/using-copilot-in-the-cli) |
| **cline** | Yes | `UNVERIFIED` (User-level only) | `cline` | Interactive approval prompt per tool call | [cline.bot](https://cline.bot/docs/mcp) |
| **kiro** | Yes | `.kiro/settings/mcp.json` | `kiro-cli` | Interactive tool prompt unless whitelisted in `autoApprove` | [kiro.dev](https://kiro.dev/docs/mcp/) |
| **qwen** | Yes | `.qwen/settings.json` | `qwen` | Workspace approval on launch; per-tool prompt unless trusted | [qwenlm.github.io](https://qwenlm.github.io/qwen-code/) |
| **amp** | Yes | `.amp/settings.json` | `amp` | Workspace confirmation prompt; per-tool confirmation | [ampcode.com](https://ampcode.com/docs/mcp) |
| **droid** | Yes | `.factory/mcp.json` | `droid` | First-run interactive approval prompt for workspace tools | [docs.factory.ai](https://docs.factory.ai/cli/mcp) |
| **goose** | Yes | `UNVERIFIED` (User-level only) | `goose` | Interactive prompt per tool execution | [block.github.io](https://block.github.io/goose/docs/guides/extensions/) |
| **kimi** | Yes | `.kimi-code/mcp.json` | `kimi` | Workspace trust prompt upon opening project repository | [code.kimi.com](https://code.kimi.com/docs/mcp) |
| **devin** | Yes | `.devin/mcp_config.json` | `devin` | Interactive tool execution prompt on first run | [docs.devin.ai](https://docs.devin.ai/cli/mcp) |
| **grok** | Yes | `.grok/config.toml` | `grok` | Interactive tool confirmation prompt during session | [docs.x.ai](https://docs.x.ai/tools/mcp) |
| **pi** | Yes | `.pi/mcp.json` | `pi` | Interactive approval in `/mcp` panel / terminal prompt | [pi.dev](https://pi.dev/docs/mcp/) |

*Note on Cline and Goose*: Both support MCP stdio servers globally (`~/.cline/data/settings/cline_mcp_settings.json` and `~/.config/goose/config.yaml`), but have no verified project-root configuration file.

## Minimal Project Configurations

### gemini (`.gemini/settings.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "gemini"] }
  }
}
```

### opencode (`opencode.json`)
```json
{
  "mcp": {
    "servers": {
      "herdmesh": { "type": "local", "command": ["herdmesh", "mcp", "--agent", "opencode"] }
    }
  }
}
```

### cursor (`.cursor/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "cursor"] }
  }
}
```

### copilot (`.github/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "type": "local", "command": "herdmesh", "args": ["mcp", "--agent", "copilot"] }
  }
}
```

### kiro (`.kiro/settings/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "kiro"] }
  }
}
```

### qwen (`.qwen/settings.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "qwen"] }
  }
}
```

### amp (`.amp/settings.json`)
```json
{
  "amp.mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "amp"] }
  }
}
```

### droid (`.factory/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "droid"] }
  }
}
```

### kimi (`.kimi-code/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "kimi"] }
  }
}
```

### devin (`.devin/mcp_config.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "devin"] }
  }
}
```

### grok (`.grok/config.toml`)
```toml
[mcp_servers.herdmesh]
command = "herdmesh"
args = ["mcp", "--agent", "grok"]
```

### pi (`.pi/mcp.json`)
```json
{
  "mcpServers": {
    "herdmesh": { "command": "herdmesh", "args": ["mcp", "--agent", "pi"] }
  }
}
```
