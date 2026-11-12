---
name: herd-handoff
description: Use when finishing (or blocking on) a task card in the herdmesh build team, to report back to the lead in a compact HANDOFF block instead of prose or code.
---

# herd-handoff

When your task is done, or you are blocked, your **entire reply** is one block in this shape. Max 12 lines. No code. No file contents.

```
HANDOFF <task-id> <DONE|BLOCKED|PARTIAL>
BRANCH: <branch> @ <short-sha>
CHANGED: <crate/paths, comma-separated>   (+<added>/-<removed> lines)
TESTS: <new/changed test names> — `just check`: <green|red: reason>
DECISIONS: <≤2 bullets, each ≤15 words; "none" if none>
RISKS: <≤2 bullets; "none" if none>
NEXT: <≤2 concrete follow-ups for the lead>
ASK: <single question if BLOCKED, else omit>
```

Rules:
- Point to things (`path#L10-40`, `sha`, `ADR-###`), never paste them.
- If a test is red, say which one and the one-line cause.
- Never write "everything works". Name the tests that prove it.
- If you changed anything outside your LEASE, say so in RISKS.
