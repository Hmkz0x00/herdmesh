# ADR-005: Threat model revision: cooperative leases, owner removal, setup races out of scope

Status: Accepted (2026-11-14) by the project owner after GATE-6. Supersedes ADR-002 Addenda 2-4 (lease admission).

## Context
Four review rounds (GATE-3..6) tried to make file leases hold against a *malicious member invited into the room*. Each fix (local admission, batch fixpoint, horizon index) added complexity and opened a new hole (N3 variants, N19). Leases never prevented anyone from editing a file: git is the source of truth. Separately, `setup` kept failing against an attacker who swaps files inside the user's own project folder *during* setup, who already controls that machine.

## Decision
1. **Leases are cooperative.** A lease is an advisory "I am editing these paths" marker that helps honest agents avoid stepping on each other. One simple, deterministic rule over the stored signed set:
   - Among live, overlapping claims the lowest `(lamport, author node, id)` wins; `ttl` capped at `MAX_LEASE_TTL`; renewal = same holder, same or subset paths; a loser stays a loser (deterministically, from the signed set alone); release by the holder frees the paths.
   - **No local admission** of claims/releases/twins (removes ADR-002 Addenda 2-4 machinery and the horizon cache). Views stay a pure function of the stored set + `now`.
   - Cost bound: index only live (unexpired) claims; expired claims are skipped in O(1) per event.
2. **Bad actors are handled by removal, not by lease cryptography.** The room owner runs `herdmesh room grant <room> <member> observer` (alias: `herdmesh room remove <member>`): every node then ignores that member's claims, decisions and tasks deterministically (ADR-002 §4 role enforcement). Signed events, sync auth, redaction, untrusted-text wrapping and IPC protections are unchanged and stay in scope.
3. **`setup` races are out of scope.** `setup` keeps refusing links/junctions/reparse points it finds, keeps atomic writes and the canonical-root check, but does not defend against an attacker concurrently modifying the user's own project tree during setup (that attacker already controls the machine).

## Consequences
- GATE-3..6 findings N3, N9, N12, N19 are closed by redesign (simpler code, deterministic, convergent); N13's remaining race is documented as out of scope.
- A malicious member *can* briefly hold or contest a lease until the owner demotes them; honest agents still see who holds what.
- Review gates now judge leases against the cooperative model and owner removal.

## Addendum 1 (2026-11-14, after GATE-7 N20-N22)
- **Removal is retroactive.** Views first resolve roles over the whole stored set: the pinned Genesis owner, then every `RoleGrant` in order-key order, each grantor judged by its role at that point. Only then are all other events applied, each judged by its author's **final** role. A removed (observer) member's claims, tasks, task updates and decisions are ignored, both the ones made before the grant and any backdated around it. Any newly stored role event triggers a full view rebuild. Raw signed history stays in the log.
- **Losers stay losers, including after renewals.** A renewal keeps its root's place in line. On applying it, open coexisting overlapping claims of other holders that sort after the root are marked dead. They do not win again when the renewal is released.
- **Expired history is cheap.** Expiry is checked before any renewal lookup, and the latest claim of each chain is indexed. Open claims are indexed per holder and per (holder, path), so replaying many coexisting claims from one holder does no pairwise scan. Per-query work is bounded by the claims live at `now`.
