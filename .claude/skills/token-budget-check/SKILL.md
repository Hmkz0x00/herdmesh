---
name: token-budget-check
description: Use when writing or changing anything an agent will read at runtime in herdmesh (briefs, digests, room card, MCP tool names/descriptions, notifications) to make sure it stays within its token budget and is covered by a test.
---

# token-budget-check

herdmesh's product is "shared context in minimal tokens". Every agent-facing string has a budget.

## Budgets (named consts in `hm-proto::budget`)
| Surface | Const | Cap |
|---|---|---|
| Room card (L0) | `ROOM_CARD_MAX` | 150 |
| Brief (L1, default) | `BRIEF_DEFAULT_MAX` | 400 |
| Push notification | `PING_MAX` | 30 |
| Decision rationale | `RATIONALE_MAX` | 60 |
| Whole MCP tool list (names + descriptions + schemas) | `TOOLS_TOTAL_MAX` | 600 |

## Checklist before handoff
1. Is the new or changed text produced by a builder function with a `max_tokens` parameter, or a const checked in a test?
2. Is there a golden test (fixture → output) asserting `estimate_tokens(out) <= CAP`?
3. Does truncation drop the **least relevant** items first (old, off-scope, done) and never cut mid-item?
4. Are references used instead of content (`git:<sha>:<path>#Lx-y`, `event:<id>`)?
5. Is peer-authored text wrapped in the untrusted-content delimiter?
6. Did the tool-list snapshot test (`mcp_tools_budget`) still pass?

If any answer is no, fix it before you hand off. Report the measured token counts in the HANDOFF `TESTS:` line.
