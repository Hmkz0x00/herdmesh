# ADR-002: Deterministic views, per-author sequence sync, anchored roles

Status: Accepted (2026-11-12). Trigger: GATE M1+M2 codex review (verdict FAIL). Refines ADR-001; no §2 item changes.

## 1. Views are a pure function of (signed event set, now)
- Materialized views use **only signed fields**. `received_at` never enters a view.
- Time-based states (lease expiry, settle) are computed **at read time** from `now`, never stored.
- Lease effective expiry = `claim.ts + min(ttl, MAX_LEASE_TTL)`; `MAX_LEASE_TTL = 8h`.
- **Ingestion skew guard (node-local, convergent):** an event with `ts > local_now + MAX_FUTURE_SKEW (5 min)` is not stored yet (kept in a bounded pending queue / re-fetched by later sync). Honest nodes with sane clocks converge on the same stored set.
- **Settle safety is local, not a view:** the `claim` tool returns Granted only when the claim is the deterministic winner AND no conflicting claim was *first seen locally* in the last `SETTLE_MS`. First-seen times live in a local table, never in views.
- **One lease model:** `settle.rs` is the single source of truth (overlap globs, atomic path sets, renewal = same author re-claims a superset, release targets a claim id). `views.rs` calls it.

## 2. Clocks
- Lamport: `checked_add`; events with `lamport > local_max + MAX_LAMPORT_JUMP (1_000_000)` are rejected (not stored). Exhaustion → error, never wrap/saturate.

## 3. Sync by per-author sequence numbers
- `Event` gains `seq: u64` per author **node**, starting at 1, +1 per event (single writer per node: the daemon/CLI session owns the counter, persisted in the store).
- Store tracks per author: contiguous prefix `hi` + set of holes. Request = paginated `[(author, have_hi, holes≤N)]` within `MAX_REQUEST_FRAME`; response = indexed range queries `(author, seq > have_hi | in holes)` with LIMIT, never a full-log scan.
- Duplicate `(author, seq)` with different ids = equivocation → keep the lowest id, record a misbehaviour mark against the author.
- Budgets: per-connection concurrent request limit (2), per-request deadline (10 s), per-frame read deadline (5 s), cumulative bytes/rounds caps on both sides.
- Repair: sync on NeighborUp, on gossip Lagged, and every 60 s ± jitter while connected (exponential backoff on failure).

## 4. Roles anchored to the room creator
- Room creation emits a signed `Genesis{owner: NodeId}` as the first event; its id is carried in the ticket (`genesis_id`) and verified on join.
- Roles bind to **NodeId**, not agent name. Only the owner (or nodes granted `owner` by the owner) may emit `RoleGrant{node, role}`. Default role for a joined node: `member`; observers come from grants.
- Enforcement is deterministic at view-apply time: an event whose author lacks permission (e.g. observer claiming/deciding, non-owner granting) is **stored but ignored** by views (so all nodes agree).

## 5. Local display hygiene (M2 scope, full wrapping in M4)
- All peer-authored strings are escaped (control chars, ANSI, newlines) and length-capped before terminal output.
- Secret files (node.key, room secrets) on Windows get an owner-only DACL; existing files with broader ACLs → warning + refuse unless `--allow-insecure-key`.

## Addendum (2026-11-12, after G3)
- **Equivocation convergence:** a sync response for an author also carries every stored twin (same author+seq, different id) for that author, capped at 64; receivers keep the lowest id deterministically and mark the author. Without this, a node holding only the higher twin never fetches the lower one.
- **Pre-Genesis rooms:** not migrated (pre-release). They fail with `NoGenesis` and must be recreated.
