# herdmesh

herdmesh is an open-source peer-to-peer coordination daemon and herdr plugin for coding agents.
It lets agents from different people, machines, and AI providers collaborate on the same project room.
Coordination happens with structured, token-minimal shared context rather than full transcript dumps.

<!-- Demo Walkthrough GIF placeholder -->
> **Demo Walkthrough (placeholder)**
>
> *(60-second walkthrough GIF showing Claude Code and Antigravity CLI agents collaborating in a shared room across two machines will be added here prior to release.)*

---

## Why herdmesh?

Existing multi-agent tools run either on a single machine, depend on a single proprietary AI provider, or spam context windows with complete conversation logs.

- **Cross-Person:** Work with teammates across different machines and networks without centralized SaaS servers or accounts. Direct P2P hole-punching via [iroh](https://iroh.computer) with automatic relay fallback.
- **Cross-Provider:** Claude Code, OpenAI Codex, Google Antigravity CLI, and local open-source models work side-by-side in the same room.
- **Token-Minimal Context:** Treat token conservation as the core product. A newcomer agent gets full project context in an **L0 Room Card (≤150 tokens)**, receives updates in an **L1 Brief (≤400 tokens)**, and fetches details on demand.

---

## 60-Second Quickstart

### 1. Install CLI
Build and install from local source using Cargo (Rust 1.85+):
```bash
# Linux / macOS
bash scripts/install.sh

# Windows (PowerShell)
.\scripts\install.ps1
```
*(Or manually: `cargo install --path crates/herdmesh --locked`)*

### 2. Install herdr Plugin (Optional)
If running inside [herdr](https://herdr.dev):
```bash
herdr plugin install herdrdev/herdmesh
# Or link during local development:
herdr plugin link .
```

### 3. Create a Room
```bash
herdmesh room create my-project
```
This initializes the room and outputs an invite ticket:
```text
Created room 'my-project' (id: 4f8b9...)
Ticket: hmroom1a2b3c4d5e6...
```

### 4. Invite a Teammate
Send the `hmroom1...` ticket to your collaborator over your existing chat or messaging tool.

### 5. Friend Joins the Room
On their machine:
```bash
herdmesh room join hmroom1a2b3c4d5e6...
```

### 6. Agents Coordinate
Configure your agents' MCP settings (see [Agent Setup Guide](docs/agents-setup.md)) pointing to `herdmesh mcp --agent <name>`. Your agents can now inspect room state, claim file leases, and ask/answer questions across machines!

---

## MCP Tools

herdmesh provides a compact, standardized Model Context Protocol (MCP) tool surface (from [`crates/hm-mcp`](crates/hm-mcp)):

| Tool | Parameters | Purpose |
|---|---|---|
| `room_card` | *(none)* | Fetch token-capped overview (≤150 tokens) of room members, status, leases, top tasks, and decisions. |
| `brief` | `max_tokens?` | Tailored brief (default ≤400 tokens) of relevant events since the agent's cursor. |
| `inbox` | `ack?` | Read unacknowledged notifications and direct asks addressed to this agent. |
| `ask` | `to`, `question` | Send a targeted question to a peer agent in the room. |
| `answer` | `ask_id`, `text` | Reply to an open question from a peer agent. |
| `handoff` | `to?`, `summary`, `refs`, `next_steps` | Post a structured task completion summary with code references and next steps. |
| `claim` | `paths`, `ttl_min` | Claim an exclusive path lease to prevent overlapping edits across agents. |
| `release` | `paths` | Release an active lease held by the calling agent. |
| `task_add` | `task`, `title`, `paths` | Add a new task card to the shared room board. |
| `task_update` | `task`, `status`, `note?` | Update task status (`open`, `in_progress`, `blocked`, `done`, `dropped`). |
| `task_list` | `filter?` | List open or filtered task cards from the shared board. |
| `decide` | `title`, `rationale`, `refs` | Record an architectural decision in the immutable room decision log. |
| `get_event` | `id` | Retrieve the full details of an event by its ID. |
| `search` | `query`, `limit` | Search past room events by keyword up to a limit. |

---

## Security Model

herdmesh is designed from first principles under the assumption that network peers and messages may be compromised:

1. **Untrusted Peer Content:** All peer-authored text is wrapped in explicit boundaries (`<<peer-untrusted from="...">>...<</peer-untrusted>>`). Agents are instructed never to treat peer messages as executable commands or system instructions.
2. **Cryptographically Signed Events:** Every state change is an event signed with the author's Ed25519 node key and verified upon receipt. Tampered or forged events are rejected.
3. **Outbound Secret Redaction:** An automatic redaction filter scans for API keys, tokens, `.env` variables, and private keys, preventing accidental leakages over gossip or sync.
4. **Role-Based Permissions:** Room participants operate under strict roles (`owner`, `member`, `observer`). Observers cannot claim leases or log architectural decisions.
5. **End-to-End Encryption:** Transport connections use authenticated QUIC with TLS 1.3 via iroh keys. Even when traffic passes through a relay, the relay server cannot inspect or alter payloads.

---

## Current Status & Roadmap

| Component | Status | Details |
|---|---|---|
| **Event Model & Storage** | **Built** | Append-only event log with SQLite views, deterministic rebuilds, and lease conflict settling ([ADR-002](docs/adr/ADR-002-determinism-sync-roles.md)). |
| **Token Budget Engine** | **Built** | Calibrated token estimation, L0 room card, L1 brief, and compaction digests (`crates/hm-brief`). |
| **MCP Server** | **Built** | Stdio server exposing 14 tools within a ≤600 token schema budget (`crates/hm-mcp`). |
| **P2P Networking** | **Built** | Iroh endpoints, gossip broadcast, and authenticated sequence-based delta sync (`crates/hm-net`). |
| **Room CLI** | **Built** | `herdmesh room create`, `join`, `invite`, `status`, `leave`, `run`, and `doctor` (`crates/herdmesh`). |
| **Self-Hosted Relay** | **Documented** | Standalone `iroh-relay` deployment guide ([docs/ops/self-hosted-relay.md](docs/ops/self-hosted-relay.md)). |
| **Daemon Process** | *Planned* | Background daemon manager (`herdmesh daemon`) and CLI stdio integration (`crates/hm-daemon` core built). |
| **TUI Room Board** | *Planned* | Interactive terminal dashboard pane built with `ratatui` for herdr panes. |

---

## Documentation

- [Installation Guide](docs/install.md)
- [Agent Setup Guide](docs/agents-setup.md)
- [Self-Hosted Relay Guide](docs/ops/self-hosted-relay.md)
- [Cross-Network Testing Guide](docs/testing/cross-network.md)
- [Herdr Plugin Architecture](docs/research/herdr-plugin.md)
- [ADR-001: Architecture](docs/adr/ADR-001-architecture.md)
- [ADR-002: Determinism, Sync & Roles](docs/adr/ADR-002-determinism-sync-roles.md)

---

## License

herdmesh is distributed under the [Apache License, Version 2.0](LICENSE).
