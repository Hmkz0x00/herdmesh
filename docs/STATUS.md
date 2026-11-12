# STATUS

Milestone: **M0-M5 code done on main · GATE-2 pending** (main 2fd60fb)
Updated: 2026-11-12

## Roster (workspace w3, tab 1; models per docs/team/model-routing.md)
- lead w3:p1 Opus 5.5 · hm-codex w3:p6 GPT-6-Astra medium (gatekeeper only)
- hm-agy w3:p2 Gemini 3.8 Flash high · hm-impl1 w3:p3 Sonnet 5.5 medium
- hm-impl2 w3:p4 Opus 5.5 / Sonnet 5.5

## Open / Remaining
- GATE-2 (codex): gate review of ADR-002 fixes, sync wire protocol & net hardening
- M2 manual cross-network test: two machines / NAT / relay fallback (docs/testing/cross-network.md)
- GitHub remote setup: push repo to remote for 3-OS CI workflow run
- M4-4 smoke test: end-to-end MCP integration test & script verification

## Done
- M0 workspace, doctor, CI · M1 proto, budget (+1.3%), store, lease settle
- M2 net (iroh endpoint, gossip, authed delta sync, tickets, converge test)
- M3 brief (L0 card ≤150 tok, L1 brief ≤400 tok, L2 detail, digests)
- M4 MCP stdio server (14 tools ≤600 tok budget), hm-daemon IPC, DaemonBackend
- M5 herdr plugin manifest, ratatui live board pane, presence mirror, idle-only pings
- M6 outbound secret redaction, roles (owner/member/observer), user docs, install scripts

## Decisions
- Repo local git; Windows native; MSVC linker pinned.
- Only lead merges/rebases. Libs: iroh 1.4.1, iroh-gossip 0.102.0, rmcp 3.9.0, interprocess 2.4.8.
- Untrusted peer text wrapped in `<<peer-untrusted>>`; outbound redaction on wire.

## Risks
- Lease TTL uses min(ts, received_at): time-based state computed at read time only.
- Windows key perms rely on profile ACL.
- Multi-agent concurrent cargo builds contend on target dir lock.

## Blockers
- Manual cross-network verification requires 2nd machine / mobile hotspot.
- Remote repo creation needed for GitHub Actions CI runs.
