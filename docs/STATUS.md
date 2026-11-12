# STATUS

Milestone: **M1+M2 code complete → GATES pending** (main 32966e2; just check green)
Updated: 2026-11-12

## Roster (herdr workspace w3; names prefixed hm- to avoid clashes)
- tab 1: lead w3:p1 · hm-agy w3:p2 (skip-permissions) · hm-impl1 w3:p3 · hm-impl2 w3:p4
- tab hm-review: hm-codex w3:p6 (gatekeeper; blocked on folder trust by human)

## Open
- GATE M1 + GATE M2 (codex): one combined review card once trusted
- M2 manual: two machines / NAT / relay fallback (docs/testing/cross-network.md) — needs human + 2nd machine
- M2 acceptance "3 in-process nodes converge": DONE (tests/converge.rs)

## Done
- M0 workspace, doctor, CI · M1-1..4 proto, budget (+1.3%), store, lease settle · R-1 libs
- M2-1 endpoint+tickets · M2-2 gossip · M2-3 authed sync (+received_at) · M2-4/4b room CLI + sync wiring · M2-5 converge test · relay + cross-net docs

## Decisions
- Repo E:\Codes\herdmesh, local git only, no remote. Windows native; MSVC linker pinned in ~/.cargo/config.toml.
- Only lead merges/rebases. Libs: iroh 1.4.1, iroh-gossip 0.102.0, rmcp 3.9.0, interprocess 2.4.8.
- Sync auth = keyed_hash(derive_key(ctx, secret), requester||server||room); request frame cap 64 KiB.

## Risks (for codex gate)
- Lease TTL uses min(ts, received_at): node-local → lease views may briefly differ across nodes. Decide: views deterministic, time-based state computed at read time only.
- Out-of-order append = full rebuild; sync batches mitigate.
- Windows key file perms rely on profile ACL (M6).
- Machine memory is tight with 4 agents + 3 target dirs.

## Blockers
- Codex trust prompt (human). Context7 key unset. 3-OS CI needs a remote (human).
