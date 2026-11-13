# STATUS

Milestone: **M0–M6 complete, ADR-003 live view in progress** (just check green)
Updated: 2026-11-14

## Done & In Progress
- M0 workspace, doctor, CI workflow · M1 signed events, budget, store, leases, anchored roles
- M2 iroh P2P + relay, gossip, authenticated per-author-seq sync (v2), 3-node convergence test
- M3 hm-brief: card 149/150, brief 392/400, digest · M4 hm-mcp 14 tools, hm-daemon, `herdmesh mcp`
- M4 smoke: Claude Code, Antigravity, Codex all PASS calling room_card + ask (docs/testing/cli-smoke.md)
- M5 hm-herdr (idle-only pings, presence), plugin manifest, `herdmesh board`; plugin linked locally
- M6 outbound redaction, owner-only key files, same-user IPC challenge, sanitize_display, roles CLI, docs
- Reviews: GATE-1 passed with fixes, GATE-2 passed with fixes (N1–N8 resolved)
- Live room view (ADR-003, in progress): animated browser view (V1 frontend merged, demo mode), local HTTP token-gated server, `herdmesh view` and `herdmesh room log`
- Agent collaboration snippet (D1): <=120-word imperative prompt for CLAUDE.md/AGENTS.md

## Remaining (human)
- Manual cross-network test, two machines, one behind NAT (docs/testing/cross-network.md)
- GitHub remote + push → 3-OS CI run (Linux/macOS later), `herdr plugin install <owner>/herdmesh`
- Demo GIF for README

## Decisions & Team
- Only lead merges. Libs: iroh 1.4.1, iroh-gossip 0.102.0, rmcp 3.9.0, interprocess 2.4.8.
- Peer text wrapped as untrusted; outbound redaction before signing.
- Model routing: Codex panes cx-astra, cx-sol, cx-luna per docs/team/model-routing.md.
- Plugin action ids must not contain dots (herdr rejects them): room-create etc.

## Environment
- Repo E:\Codes\herdmesh, local git. Shared cargo target E:\Codes\hm-target (jobs=4) + MSVC linker.
- herdmesh installed at ~/.cargo/bin/herdmesh.exe.
