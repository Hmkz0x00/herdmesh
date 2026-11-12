# Model routing (token optimisation)

Pick the cheapest model that reliably passes the task's acceptance tests. Switch at task boundaries (restart the pane's agent with flags; never use `/model` inside a pane, because it changes the user's saved default).

| Work | Claude panes | Codex | Antigravity (agy) |
|---|---|---|---|
| Lead: orchestration, merge review | Opus 5.5 (lead only) | — | — |
| Security/protocol-critical code (crypto, sync, auth, ACLs) | Opus 5.5, effort high | gpt default, `-c model_reasoning_effort=high` | — |
| Well-specified implementation with tests (crates, CLI, MCP tools) | **Sonnet 5.5, effort medium** | gpt default, effort medium | — |
| Mechanical edits, renames, fmt/clippy fixes, conflict cleanup | Haiku 5.5, effort low | effort low | gemini-3.8-flash-low |
| Library/API research, docs, fixtures, golden tests | — | — | gemini-3.8-flash-medium (high for research) |
| Milestone gate reviews (adversarial) | — | effort high | — |

Start commands:
- Claude: `herdr agent start <name> --kind claude --pane <id> -- --model sonnet --effort medium`
- agy: `herdr agent start <name> --kind agy --pane <id> -- --dangerously-skip-permissions --model gemini-3.8-flash-medium`
- Codex: restart with `-c model_reasoning_effort=<low|medium|high>` (bypass flag only via the user's allow rule).

Other savings: task cards ≤200 tok; HANDOFF only; `/compact` between tasks; lead reads diffs by `--stat` + targeted ranges.
