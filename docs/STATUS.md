# STATUS

Milestone: **M1 Proto+Store** (M0 merged at 6fb926c; 3-OS CI run pending a remote)
Updated: 2026-11-12

## Roster (herdr workspace w3; names prefixed hm- to avoid clashes)
- lead w3:p1 · hm-agy w3:p2 (agy, skip-permissions) · hm-impl1 w3:p3 · hm-impl2 w3:p4
- codex: not spawned until GATE M1 (quota)

## Open tasks
- M1-1 hm-impl1: proto events, Ref, signing (impl1/m1-proto)
- M1-2 hm-impl2: budget estimator + tiktoken reference test (impl2/m1-budget)
- R-1 hm-agy: libs.md (agy/research)
- Next: M1-3 store (impl1), M1-4 lease settle (impl2), GATE M1 codex

## Done
- M0-1/2 workspace + doctor (b3d9187); M0-3 CI workflow (8ca4588)

## Decisions
- ADR-001 accepted. Repo E:\Codes\herdmesh, local git only, no remote.
- Windows native (not WSL). MSVC linker pinned in ~/.cargo/config.toml.

## Blockers
- Context7 key not set; agents use docs.rs instead.
- 3-OS CI needs a GitHub remote (human decision: push).
