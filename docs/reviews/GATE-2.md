# GATE-2 independent security review (branch review/gate2 = main @ 7d7d39f)

Scope: re-check of the 12 GATE-1 findings against ADR-002 (+Addendum), plus new findings with focus on
local IPC, outbound redaction, sync v2 and notify/presence. Baseline: `cargo test -p hm-store -p hm-daemon` green.

## GATE-1 findings

| # | Sev | Status | Evidence |
|---|-----|--------|----------|
| 1 | HIGH | FIXED | views.rs:1-5 no `received_at`; expiry `ts + min(ttl, MAX_LEASE_TTL)` settle.rs:142-145, computed at read time |
| 2 | HIGH | FIXED | per-author seq + holes: store.rs:731-768 `seq_summary`, :771-810 `events_for`; sync.rs:194-232 paging (see N2 for twins gap) |
| 3 | HIGH | FIXED | pinned Genesis owner views.rs:111-118; `Role::may` gate views.rs:119-123; grants bind NodeId views.rs:183-192 |
| 4 | HIGH | FIXED | local first-seen + quiet window store.rs:711-727 (`claim_safe`); first_seen never read by views (see N3 for a sibling) |
| 5 | HIGH | FIXED | single model: views.rs:15,163-170 delegate to `LeaseBook`; `book_from_events` settle.rs:363 uses the same book |
| 6 | HIGH | PARTIAL | `events_for` is indexed+LIMIT, but server still calls `seq_summary(0)` per request (sync.rs:245), which reads every author's whole tail above `hi` unbounded (store.rs:740-745) — see N4 |
| 7 | HIGH | FIXED | request/frame/total deadlines sync.rs:108-114,581-590; 2-slot semaphore :688; round cap :700; byte budget :620-660 |
| 8 | HIGH | FIXED | `checked_add` views.rs:26-29; jump guard store.rs:617 (`MAX_LAMPORT_JUMP` 1e6) |
| 9 | MED | FIXED | ≤256 authors × ≤16 holes per request sync.rs:166-178,76-78; hole windows :82-84 |
| 10 | MED | FIXED | sync on NeighborUp/Lagged/periodic±jitter with backoff session.rs:441-455, repair.rs:27-99 |
| 11 | MED | FIXED | Windows protected owner-only DACL at create secure_file.rs:233-272; validated on load :275-330, data.rs:239. Nit: file *owner* SID not checked (attacker-owned file with victim-only ACE passes) |
| 12 | MED | FIXED | `sanitize_display` display.rs:17-60 (C0/C1/bidi/zero-width, caps); used in session.rs:61-72,494-559, board.rs, room.rs |

## New findings

- **N1 [HIGH] hm-daemon/src/ipc.rs:399-409, herdmesh/src/daemon.rs:81-82,273-280 — client trusts any listener; token sent raw.**
  `request_inner` connects to the name and immediately writes `Hello{token}`; the server is never authenticated.
  `is_serving` treats *any* listener as a healthy daemon, so `ensure_running` never starts the real one.
  Windows: another local user creates `\\.\pipe\herdmesh-<USERNAME>` (permissive DACL) before the daemon starts or
  while it restarts; the daemon's bind then fails (interprocess sets FILE_FLAG_FIRST_PIPE_INSTANCE — good) but every
  MCP client hands the squatter the token + all requests (Ask/Answer/Handoff text). With the token the attacker can
  later drive the real daemon as any agent. Unix: when `XDG_RUNTIME_DIR` is unset, the client connects into
  `$TMP/herdmesh-…/` without `ensure_private_dir` or peer-cred checks (ipc.rs:282-287,338-350), so a pre-created
  attacker dir/socket gets the token (daemon refuses to start, clients still connect).
  Fix: (1) never send the token: challenge-response (server nonce → client HMAC(token, nonce‖"c"); server proves
  HMAC(token, client_nonce‖"s") first or in the same exchange); (2) Windows client: `GetNamedPipeServerProcessId`
  (interprocess `server_process_id()`) → `OpenProcessToken` → `TokenUser` SID == current user SID, else refuse;
  (3) Unix client: `ensure_private_dir(socket_dir())` + `peer_creds().euid()==getuid()` before writing;
  (4) `is_serving` must use the authenticated handshake, and report "name squatted by another user" instead of OK.
- **N2 [MED] hm-store/src/store.rs:812-820, hm-net/src/sync.rs:268-280 — equivocation never detected if no node holds both twins.**
  Twins are served only for seqs already in `equivocations`. Author sends X (seq k) to A and Y to B (e.g. via direct sync);
  both report seq k as held, nothing is exchanged, no mark is ever made → A and B apply different events forever (views diverge).
  Fix: add a per-author digest (e.g. hash of ids over `[1..=hi]`, or of the last N (seq,id)) to `AuthorWant`; on mismatch
  serve that author's ids in the range so the twin is found and marked.
- **N3 [HIGH] hm-store/src/settle.rs:236-238 (+ store.rs:617) — lease takeover with a low Lamport.**
  Only an upper Lamport bound exists. Any secret holder (or a fresh node key) signs a `LeaseClaim` with `lamport=2`;
  it sorts ahead of a long-Granted lease and flips it to `Lost` once its own settle window passes. `claim_safe` only
  blocks for SETTLE_MS after first-seen, so the original holder keeps editing while views say someone else owns it.
  Fix: per-author Lamport must strictly increase with seq (enforce in views, deterministic); a claim cannot beat a
  conflicting claim whose signed `ts + SETTLE_MS` precedes its own `ts`; add a past-skew guard on claim `ts` at ingest.
- **N4 [MED] hm-net/src/sync.rs:245, hm-store/src/store.rs:731-768 — per-request O(all authors × sparse tail) under the store mutex.**
  Authors are unbounded (any key can sign) and an author that skips seq 1 keeps `hi=0`, so every request decodes its whole
  log. `spawn_blocking` is not cancelled by the request deadline, so the lock stays held. Fix: compute summary only for
  `req.want` authors in `(from,to]`; persist `max`/hole counts in `authors`; LIMIT the tail query; cap distinct authors/room.
- **N5 [MED] hm-net/src/sync.rs:67,779 vs gossip.rs:27 — sync accepts events up to 9 MiB; gossip caps 64 KiB.**
  No size cap in `Event::decode`/`Store::append` (event.rs:323, store.rs:503), so oversized events enter via sync, persist
  and are re-served to everyone. Fix: enforce `MAX_EVENT_BYTES` in `Event::decode`/store ingest and per-field caps.
- **N6 [MED] hm-daemon/src/core.rs:338-349 — `pending_pings` scans `events_since(cursor)` per local agent every 5 s tick.**
  Cursor moves only on `inbox ack`; a remote flood makes each tick decode the whole unacked log while holding the store lock.
  Fix: index Ask/Handoff by addressee, or keep a separate ping cursor advanced after offering; LIMIT the query.
- **N7 [LOW] hm-daemon/src/core.rs:187-225 — redaction misses some outbound strings.**
  Not scrubbed: `TaskAdd.task/paths`, `TaskUpdate.task`, `LeaseClaim.paths`, `GitRef` paths in refs, author/`to` agent names.
  Both signing paths do call it (core.rs:419-420, session.rs:369). Fix: scrub or charset-restrict every string field; test per variant.
- **N8 [LOW] hm-daemon/src/ipc.rs:82-87 — existing `ipc.token` read without `check_private`.**
  Fix: use `read_private` (as for node.key) so a broad-ACL token file is refused. Notify path itself is clean:
  `agent_label` charset-restricted, `re` = task id only, `strip_control_chars` before `herdr agent prompt`.

Summary: GATE-1 11 FIXED, 1 PARTIAL, 0 OPEN. New: 2 HIGH (N1, N3), 4 MED, 2 LOW. N1 and N3 block release.

VERDICT: PASS-WITH-FIXES
