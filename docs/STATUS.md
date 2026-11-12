# STATUS

Milestone: **M1 Proto+Store → GATE** (all M1 tasks merged at 528418c; just check green)
Updated: 2026-11-12

## Roster (herdr workspace w3; names prefixed hm- to avoid clashes)
- tab 1: lead w3:p1 · hm-agy w3:p2 (skip-permissions) · hm-impl1 w3:p3 · hm-impl2 w3:p4
- tab hm-review: hm-codex w3:p6 (gatekeeper; 1 request per gate)

## Open tasks
- GATE M1 hm-codex: review proto/store (waiting on folder trust)
- M2-1 hm-impl2: iroh endpoint + room tickets (impl2/m2-net)
- hm-impl1: reserved for gate fixes

## Done
- M0: workspace, doctor, CI (6fb926c)
- M1-1 events+signing · M1-2 budget (agg err +1.3%) · M1-3 store · M1-4 lease settle · R-1 libs.md

## Decisions
- ADR-001 accepted. Repo E:\Codes\herdmesh, local git only, no remote.
- Windows native (not WSL). MSVC linker pinned in ~/.cargo/config.toml.
- Only lead merges/rebases (kit's ask-rule blocks agents anyway).
- Libs: iroh 1.4.1, iroh-gossip 0.102.0, rmcp 3.9.0, interprocess 2.4.8.

## Risks
- Lease TTL/settle use author-supplied ts → clamp to local receive time in M2 (store received_at).
- Out-of-order append = full view rebuild; sync must batch.

## Blockers
- Context7 key not set; agents use docs.rs.
- 3-OS CI needs a GitHub remote (human decision: push).
