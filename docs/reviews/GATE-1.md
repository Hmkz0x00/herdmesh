# GATE-1 (M1+M2) codex review — VERDICT: FAIL (original 12 findings)

1. [HIGH] hm-store/views.rs:22 — min(ts, received_at) made lease expiry/winners node-local. Fix: reduce only signed data; protocol TTL cap.
2. [HIGH] hm-store/store.rs:529 — per-author max-Lamport sync skips missing earlier events / same-Lamport siblings. Fix: reconcile ids or seq ranges with holes; paginate.
3. [HIGH] hm-store/views.rs:130 — roles self-declared; anyone can announce Owner; no permission enforcement. Fix: anchored owner, grants bound to NodeId, deterministic enforcement.
4. [HIGH] hm-store/settle.rs:174 — settle measured from author ts; backdated claims Granted immediately. Fix: local first-seen + quiet window.
5. [HIGH] hm-store/views.rs:164 — views leases vs settle.rs implement different rules. Fix: one claim-resolution model.
6. [HIGH] hm-store/store.rs:526 — sync loads/decodes whole log under mutex. Fix: indexed bounded pagination, cap work/concurrency.
7. [HIGH] hm-net/sync.rs:326 — no work budget/deadlines; malicious peer can stall. Fix: concurrency limits, deadlines, byte/work budgets.
8. [HIGH] hm-store/views.rs:28 — u64::MAX Lamport saturates honest clocks. Fix: checked advancement, bounded jumps.
9. [MEDIUM] hm-net/sync.rs:379 — 'have' vector grows past 64 KiB request cap. Fix: bound/paginate summaries.
10. [MEDIUM] herdmesh/session.rs:312 — gossip loss only warned; no periodic/lag repair. Fix: sync on Lagged + periodic with backoff.
11. [MEDIUM] herdmesh/data.rs:272 — secret-file protection Unix-only. Fix: owner-only Windows DACL + validate on load.
12. [MEDIUM] herdmesh/session.rs:58 — peer strings reach terminal unescaped. Fix: escape controls, cap lengths.

Fix plan: docs/adr/ADR-002-determinism-sync-roles.md (+ Addendum). Line numbers above refer to the pre-fix code.
