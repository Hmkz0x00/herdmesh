# ADR-002: Deterministic views, per-author sequence sync, anchored roles

Status: Accepted (2026-11-12). Trigger: GATE M1+M2 codex review (verdict FAIL). Refines ADR-001; no §2 item changes.

## 1. Views are a pure function of (signed event set, now)
- Materialized views use **only signed fields**. `received_at` never enters a view.
- Time-based states (lease expiry, settle) are computed **at read time** from `now`, never stored.
- Lease effective expiry = `claim.ts + min(ttl, MAX_LEASE_TTL)`; `MAX_LEASE_TTL = 8h`.
- **Ingestion skew guard (node-local, convergent):** an event with `ts > local_now + MAX_FUTURE_SKEW (5 min)` is not stored yet (kept in a bounded pending queue / re-fetched by later sync). Honest nodes with sane clocks converge on the same stored set.
- **Settle safety is local, not a view:** the `claim` tool returns Granted only when the claim is the deterministic winner AND no conflicting claim was *first seen locally* in the last `SETTLE_MS`. First-seen times live in a local table, never in views.
- **One lease model:** `settle.rs` is the single source of truth (overlap globs, atomic path sets, renewal = same holder re-claims the same paths or a subset (Addendum 2), release targets a claim id). `views.rs` calls it.

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

## Addendum 2 (2026-11-14, after GATE-3 N3/N9): lease admission and narrow renewal
**Why signed data alone can't fix N3.** Members can mint fresh node keys for free, and both `lamport` and `ts` are chosen by the signer (only future skew is bounded). A fresh seq-1 claim with a low Lamport and a backdated `ts` looks exactly like an honest claim from a node that was cut off. So every deterministic rule over signed fields, including a signed "seen frontier", can be met by lying about what was seen. The only signal an attacker cannot forge is *when a claim reached this node*. We use that at **admission** (which events get stored), never inside views.

1. **Views stay a pure function of the stored signed set** (§1 unchanged): same stored set + `now` → same leases.
2. **Lease admission (ingest, node-local).** A `LeaseClaim` C offered at local time `now` is **refused** (not stored: `StoreError::LeaseTakeover`, counted as rejected in batches) when adding it would make some claim V of another holder stop winning, and V's root was first seen here at least `SETTLE_MS` before `now`, i.e. V is *locally settled*. The "before" book only holds claims and releases first seen strictly before `now`, so events that arrive in the same instant (one batch) never refuse each other. Arrival order inside one instant therefore cannot change the stored set (the GATE-1/2 order-independence proptests still apply). This applies to fresh inserts and to lower-id equivocation twins alike.
   - Honest races are unaffected: a conflicting claim that arrives inside V's local settle window still competes by the deterministic rule.
   - Consequence: `claim_safe` ⇒ final *at this node*. A lease this node reported as safe can no longer be taken retroactively by a claim that arrives later (N3), whatever its Lamport, `ts`, seq or renewal root.
   - Missing `first_seen` (legacy rows) counts as settled (the safe direction).
3. **Renewal cannot grow (N9).** A renewal is a later claim by the same holder (node + agent), signed before the parent expired and not after a release of the parent, whose paths are a **subset** (literal strings) of the parent's. It keeps the parent's place in line, and dropped paths are freed. Any wider claim is a **new claim** with its own root, signed time and Lamport, and it competes normally. Narrowing is still checked by (2), so a "shrink" renewal can't take a locally settled lease either.
4. **Not adopted: monotonic per-author Lamport.** It does not stop N3 as written (a fresh node has no history). Rejecting at ingest depends on which twin arrives first, so it diverges, and a claims-only view rule needs a new `ClaimRec` field that other crates construct. Revisit together with a signed causal-frontier field if signed proof is ever needed.

**Residual risks (documented, accepted for now):**
- *Partitions / delivery slower than `SETTLE_MS`*: if two honest conflicting claims race and one reaches a node only after the other has settled there, that node refuses it and others don't. The stored sets differ until the surviving lease chain ends (expiry or release; re-offered by sync, then admitted). This is the price of (b): a settled lease is never revoked by a later arrival.
- *Returning or new nodes* that receive the victim's claim and a backdated attacker claim in the same instant can't tell them apart, and fall back to the deterministic rule. Nodes that witnessed the lease settle refuse the attacker claim, so it is not propagated by them.
- *Loser revival*: a claim that was losing because of a third claim can win again when that blocker is released or expires, and can then take a settled lease (a pre-existing atomic-set property; it needs no later-arriving claim). Tracked as a follow-up.

## Addendum 3 (2026-11-14, after GATE-4 N3/N12): losers stay dead; bounded lease resolution
GATE-4 turned the Addendum 2 "loser revival" residual into an exploit. Attacker W=[a] holds a, and victim V=[b] settles. A backdated A=[a,b] then loses to W, so admission lets it in. Once W is released, A wins b and revokes V. Fix: a claim's fate is decided **once, from signed data, against the claims it coexisted with**. Releases and expiry never promote it later.

1. **Coexist.** Two claims coexist if their signed lifetimes `[ts, expires_at)` overlap. Claim V *holds* its paths at claim C if V coexists with C, V has no release from its node with an order key below C's, and V has no renewal with an order key below C's (dropped paths are freed by the renewal).
2. **Dead.** C is *dead* if some claim V of another holder with overlapping paths holds them at C and either (a) V is not dead and sorts before C in the walk order (root key, own key), or (b) V's root settled by signed time before C's root (`root(V).ts + SETTLE_MS <= root(C).ts`, the GATE-2 late rule). A renewal of a dead claim is dead. Deadness reads no clock and no local data, so the same stored set gives the same answer on every node.
3. **Dead claims never win and never block.** State: `Lost { winner }`, where `winner` is the killer's latest renewal. When the killer is released or expires, the loser stays dead: its holder must claim again. A claim signed after the release (key above it), or after the expiry (`ts >= expires_at`), is not killed by it and competes normally. This replaces "a loser waits in line". Live, non-dead claims are then walked as before.
4. **Admission covers every lease-book change** (extends Addendum 2 §2): a claim, a release, or a lower-id twin that replaces a stored claim or release is refused when it would make a locally settled winner stop winning. The claim's own holder, and the claim a release targets, are exempt. This stops backdated releases (lamport below the dead claim's) from un-killing a loser, and stops twin swaps that delete the killer.
5. **Bounded cost (N12).** Renewal links, roots, deadness and an expiry-ordered index are computed once per book change and cached (not serialized). Killers are found within a signed-time window of ±`MAX_LEASE_TTL`. Each `claim_state`/`active`/`conflicting` call only visits claims unexpired at `now`, so polling is not O(history) per call.

Residual (unchanged from Addendum 2, restated per GATE-4): partitions and same-instant bootstrap can still leave replicas with different stored sets, so leases are **advisory coordination**, not a global exclusion guarantee. Local finality holds against later-arriving claims, releases and twins at a node that saw the lease settle.
