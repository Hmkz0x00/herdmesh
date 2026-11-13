# ADR-003: Live room view (`herdmesh view`)

Status: Accepted (2026-11-13). Requested by the project owner after the first two-machine test.

## Context
People in a room want to see what the other person's agents are doing, and to watch the agents work together. Screens and transcripts are never shared (they hold secrets and cost tokens); that does not change. The v1 non-goal "no GUI" is relaxed for one optional, local, read-only page.

## Decision
- `herdmesh view [--room R] [--no-open]` runs a foreground HTTP server that reads room state from the daemon over the existing local IPC and serves one page. Nothing new is sent over the network between peers; no new event kinds; the MCP tool surface is unchanged.
- **Server rules (all mandatory):** bind `127.0.0.1` on a random port only; every URL starts with a random 128-bit token path segment (`/<token>/…`), wrong or missing token → 404; `Host` must be `127.0.0.1:<port>` or `localhost:<port>`, else 403 (DNS rebinding); GET only, else 405; no CORS headers; every response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `Cache-Control: no-store`.
- **Assets** are embedded in the binary from `crates/herdmesh/assets/view/{index.html,view.css,view.js}`. No inline script or style, no external requests (no CDN, no web fonts).
- **Untrusted text:** every string in the API is peer-authored unless stated. The server truncates text to 200 chars and applies `sanitize_display` + `redact`; the page renders strings with `textContent` only (never `innerHTML`, `insertAdjacentHTML`, `eval`, or inline handlers).
- **Activity line** per member is derived deterministically from views + recent events (leases held, task in progress, last action and its age). No LLM.
- **Demo mode:** opened from disk (`file://`) or with `?demo=1`, the page plays a scripted room so the design can be reviewed without a daemon.

## API: `GET /<token>/api/state?since=<cursor>` (page uses the relative URL `api/state`)
```json
{ "room": "test", "you": "3d053a/alice", "now_ms": 0, "cursor": 12,
  "members": [{ "id": "3d053a/alice", "node": "3d053a", "name": "alice", "role": "owner|member|observer",
                "local": true, "present": true, "status": "idle|working|blocked|done|unknown",
                "activity": "holds src/net/** · t-3 doing · answered bob 2m ago", "last_ms": 0 }],
  "leases":  [{ "id": "ab12", "paths": ["src/net/**"], "holder": "3d053a/alice", "state": "pending|granted", "expires_ms": 0 }],
  "tasks":   [{ "id": "t-3", "title": "fix sync", "status": "open|doing|blocked|done", "owner": "1e048f/bob" }],
  "events":  [{ "id": "ab12cd", "lamport": 12, "ts_ms": 0, "kind": "join|leave|presence|ask|answer|handoff|claim|release|task_add|task_update|decide|note",
                "from": "3d053a/alice", "to": "1e048f/bob", "text": "short text", "ref": "t-3" }] }
```
`events` holds only events after `since` (max 200, oldest first); `to`, `ref`, `owner` may be null. `id` values are short prefixes for display.

## Consequences
- One more local surface to review at the next security gate (token, Host check, CSP, text handling).
- Status is `unknown` for members whose machine has no herdr. Follow-up idea, not in this ADR: a `note` tool so an agent can post a one-line "what I'm doing".
