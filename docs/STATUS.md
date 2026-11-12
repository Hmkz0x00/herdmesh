# STATUS

Milestone: **M0–M6 code complete; security gates passed, all fixes merged** (just check green)
Updated: 2026-11-13

## Done
- M0 workspace, doctor, CI workflow · M1 signed events, budget (+1.3%), store, leases, anchored roles
- M2 iroh P2P + relay, gossip, authenticated per-author-seq sync (v2), 3-node convergence test
- M3 hm-brief: card 149/150, brief 392/400, digest · M4 hm-mcp 14 tools 511/600 tok, hm-daemon, `herdmesh mcp`
- M4 smoke: Claude Code, Antigravity, Codex all PASS calling room_card + ask (docs/testing/cli-smoke.md)
- M5 hm-herdr (idle-only pings, presence), plugin manifest, `herdmesh board`; plugin linked locally (`herdr plugin link`)
- M6 redaction on all outbound text, owner-only key files + owner check, same-user IPC with challenge-response, sanitize_display, roles CLI, README, install/smoke scripts
- Reviews: GATE-1 FAIL → ADR-002 fixes; GATE-2 PASS-WITH-FIXES (docs/reviews/GATE-2.md) → all 8 new findings fixed (N1–N8)

## Remaining (human)
- Manual cross-network test, two machines, one behind NAT (docs/testing/cross-network.md)
- GitHub remote + push → 3-OS CI run, `herdr plugin install <owner>/herdmesh`
- Demo GIF for README
- Optional: Codex re-check of the GATE-2 fixes when quota allows

## Decisions
- Only lead merges. Libs: iroh 1.4.1, iroh-gossip 0.102.0, rmcp 3.9.0, interprocess 2.4.8.
- Peer text wrapped as untrusted; outbound redaction before signing. Model routing: docs/team/model-routing.md.
- Plugin action ids must not contain dots (herdr rejects them): room-create etc.

## Environment
- Repo E:\Codes\herdmesh, local git only. Shared cargo target E:\Codes\hm-target (jobs=4) + pinned MSVC linker in ~/.cargo/config.toml.
- herdmesh installed at ~/.cargo/bin/herdmesh.exe.
