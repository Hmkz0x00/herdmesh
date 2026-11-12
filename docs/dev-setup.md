# herdmesh starter kit

Drop these files into an empty git repo, then:

1. **Install the CLIs:** herdr, Claude Code, Codex, Antigravity CLI (`agy`), Rust (rustup), Node (for `npx`). On Windows, use **WSL2**, because herdr's native Windows support is beta.
2. Get a free Context7 key and run `export CONTEXT7_API_KEY=...`. Paste the same key into `.codex/config.toml` and `.agents/mcp_config.json`.
3. Run `bash scripts/bootstrap.sh`. Then trust the folder when Claude Code first opens it. Plugins in `.claude/settings.json` only load after you trust it.
4. Run `herdr`, open a pane, start `claude`, name the agent `lead`, and paste **MASTER_PROMPT.md**.

## What loads automatically, per agent
| Agent | Instructions | Skills | MCP |
|---|---|---|---|
| Claude Code | `CLAUDE.md` → imports `AGENTS.md` | `.claude/skills/*` + plugins from `.claude/settings.json` | `.mcp.json` |
| Codex | `AGENTS.md` | `.agents/skills/*` | `.codex/config.toml` |
| Antigravity CLI | `AGENTS.md` | `.agents/skills/*` | `.agents/mcp_config.json` |

## Plugins & skills chosen (and why)
- **superpowers** (obra): brainstorm → plan → TDD → verify workflow skills.
- **rust-analyzer-lsp** (official): Claude sees type errors right after each edit and navigates symbols without grepping, which saves tokens.
- **security-guidance** (official): reviews each change for vulnerabilities. This project handles crypto and networking.
- **commit-commands**, **pr-review-toolkit** (official): commit hygiene and review agents at merge time.
- **herdr skill** (herdrdev): teaches every agent the herdr pane/agent CLI.
- **Context7 MCP**: current docs for `iroh`, `iroh-gossip`, and `rmcp`, which change quickly.
- **herd-handoff**, **token-budget-check** (custom, in this kit): a compact reporting format and enforcement of the token budgets that are this product's main feature.

Check each plugin's per-turn context cost in `/plugin` → Installed. Disable anything that isn't pulling its weight.
