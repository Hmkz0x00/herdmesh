# ADR-004: Easy setup, automatic names, deeper herdr integration

Status: Accepted (2026-11-14). Requested by the project owner after the first two-machine test.

## 1. Automatic names
- **Person name** = `person` in herdmesh.toml; default: the OS user name, lowercased, reduced to `[a-z0-9-]`, max 16 chars. Set once with `herdmesh setup --name <you>`.
- **Agent name** default = `<person>-<kind>` where kind is the agent CLI (`claude`, `codex`, `agy`, …); a second agent of the same kind on the machine gets `-2`, `-3`. `--agent` stays as an override.
- Every name (CLI flags, config, genesis, session signing, daemon) goes through ONE shared validator (`hm-proto`): `[a-z][a-z0-9-]{0,31}`, and `redact::contains_secret` must be false (fixes GATE-3 N7).
- `ask(to=…)` accepts `name`, `person/agent`, or a unique prefix of either. If nothing matches, it returns an error listing the room's agents instead of silently addressing a local agent.

## 2. `herdmesh setup` (one command per person per project)
Run in a project folder: `herdmesh setup [--join <ticket>] [--name <you>] [--yes]`.
1. Ensures the daemon. Creates a room named after the folder, or joins with `--join` (also reads a ticket from the clipboard if `--join` has no value).
2. Detects installed agent CLIs (`claude`, `codex`, `agy`) and writes **project-level** MCP config only: `.mcp.json` (Claude Code), `.codex/config.toml` (Codex), `.agents/mcp_config.json` (Antigravity), each running `herdmesh mcp --kind <kind> --room <room>`. Never edits user-level config. Merges with existing files; shows a diff and asks unless `--yes`.
3. Appends the collaboration snippet (docs/snippets/herdmesh-collab.md) to CLAUDE.md / AGENTS.md if not already present (asks unless `--yes`).
4. Prints the invite ticket (and copies it to the clipboard) when it created the room; prints next steps in 3 lines.
`herdmesh mcp --kind <kind>` derives the agent name per §1, so configs contain no hand-picked names.

## 3. herdr integration
- **Plugin actions** (ids use hyphens): `setup-project`, `join-from-clipboard`, `open-live-view` (runs `herdmesh view`), `open-conversation` (pane running `herdmesh room log --follow`), existing `board-open`, `room-invite` (copies ticket to clipboard).
- **Notifications:** when an Ask/Handoff arrives for a local agent, the daemon also calls `herdr notification show "<from> → <to>" --body "<kind>: first 80 chars, sanitized+redacted" --sound request` (only when herdr is present; rate-limited to 1 per 10 s per agent). Pings into agents stay idle-only as before.
- **Board pane** shows the conversation (last 10 messages) under members/leases/tasks.
- Agent names in herdr: when `herdr agent list` shows an agent whose name matches a herdmesh agent, presence mirrors it (unchanged).
