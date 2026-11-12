# ADR-001: herdmesh architecture

Status: Accepted (2026-11-12). Source: master prompt §2. Changes need a new ADR.

## Decision
- **Rust** workspace with one static binary `herdmesh` and a herdr plugin manifest. Apache-2.0.
- **Crates:** `hm-proto` (types, budget, no I/O) → `hm-store` (signed append-only log + SQLite views) → `hm-net` (iroh endpoint, gossip, req/resp, tickets) → `hm-brief` (L0/L1/L2 builders) → `hm-mcp` (rmcp stdio server) → `hm-herdr` (herdr CLI adapter) → `hm-daemon` → `herdmesh` CLI.
- **Network:** iroh QUIC (hole-punching, relay fallback, self-hostable relay). iroh-gossip topic = blake3(room secret).
- **Data:** event-sourced. `Event{id=blake3, room, author, lamport, ts, kind, body, sig}`. Views rebuild deterministically, ordered by (lamport, author). `refs` are pointers only.
- **Context protocol (the product):** L0 card ≤150 tok, L1 brief ≤400 tok (since cursor, relevance-filtered, older material via Digest), L2 detail on demand. Pushed pings ≤30 tok. The author writes Handoff summaries. Budgets are enforced in code by `hm-proto::budget`.
- **MCP tool names are a stable API.** The whole tool list must stay ≤600 tok.
- **herdr:** only ping agents that are idle/done; never interrupt a `working` agent.
- **Security:** signed events, encrypted transport, peer text wrapped as untrusted, outbound redaction, roles owner|member|observer.

## Stress-test notes (resolve inside the noted milestone, no re-architecture)
1. **Gossip is best-effort and has no history.** Late joiners and peers coming back online need anti-entropy: a direct `SyncSince{room, per-author lamport vector}` request/response in `hm-net`. → M2.
2. **Lease races.** Two concurrent claims resolve deterministically by (lamport, author), but the loser may already have started. Rule: a claim is *effective* only after a settle window (default 2 s with no conflicting claim seen), and the tool returns `pending|granted|lost`. → M3/M4.
3. **Daemon↔MCP IPC on Windows.** Use cross-platform local sockets (named pipe on Windows, UDS on unix), e.g. the `interprocess` crate. Verify first. → M0 research, M4 build.
4. **Budget reference tokenizer.** The ±15% test needs an offline reference tokenizer as a dev-dependency (e.g. `tiktoken-rs` o200k). This is approximate for non-OpenAI models; the tolerance absorbs it. → M1.
5. **Identity:** NodeId = machine key, which signs events. `agent_name` is a label under that key, not a separate key. Roles bind to NodeId. → M1.
6. **Clock:** `ts` is informational only. Ordering uses Lamport only.
