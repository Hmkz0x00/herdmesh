# GATE-3 adversarial security review

Target: `cx/astra-gate3` @ `69afddbc8208e3b9242b1a7968e55e8969516a41`; scoped source review against GATE-2 and ADR-003.
Threats: malicious room-secret holders/signing peers, foreign local users, and hostile browser requests to loopback.
Findings below are source-validated counterexamples, not newly executed exploit tests. Paths are repository-relative.

| Prior | Result | Evidence / remaining issue and fix |
|---|---|---|
| N1 | HOLDS | `crates/hm-daemon/src/ipc.rs:195,216,585,643`: role-separated proofs bind both nonces; owner checked before writing; owner-only pipe DACL. `crates/hm-daemon/src/peer_owner.rs:42,104` verifies process SID. First-instance/DACL tests pass; `crates/herdmesh/src/daemon.rs:352` authenticates before returning MCP backend. |
| N2 | BROKEN | [MED] `crates/hm-net/src/sync.rs:247,912,1083`: A/B hold different signed seq=2 events, nobody holds seq=1; max=2, hi=0 disables digest, holes request only 1, range requests >2. No twin crosses, so views diverge indefinitely. Fix: reconcile sparse held ranges/IDs as well as contiguous prefixes. |
| N3 | BROKEN | [HIGH] `crates/hm-store/src/settle.rs:244,262`; `crates/hm-store/src/store.rs:720,756`: victim L10/ts=T, fresh attacker seq1/L2/ts=T arriving T+60s passes ingest and takes the still-live lease. Signed time is attacker-controlled; only future skew and upper Lamport are guarded. Local first-seen delays two seconds, then permits takeover. Fix: monotonic author clocks plus a bounded past-skew/admission policy that cannot retroactively revoke settled claims. |
| N4 | HOLDS | `crates/hm-store/src/store.rs:892,899`: author page and sparse-tail SQL LIMITs; `crates/hm-net/src/sync.rs:281,1075`: bounded server/client summary pages. Recovery correctness remains separate (N10). |
| N5 | HOLDS | `crates/hm-store/src/store.rs:640,736` rejects encoded events over 64 KiB on append/batch paths; `crates/hm-net/src/sync.rs:76,1013` bounds response frames and checks event bytes before decode. |
| N6 | HOLDS | `crates/hm-daemon/src/core.rs:481,509,515`: per-author scan cursor advances independently of inbox acknowledgement; indexed reads capped at 128 events/author, remembered holes capped at 64 (:135). No repeated whole unacked-log decode per agent. |
| N7 | BROKEN | [LOW] `crates/hm-daemon/src/core.rs:259` scrubs body fields and :198 rejects secret-like daemon names, but `crates/herdmesh/src/session.rs:401,412` signs raw `cfg.agent_name`; `crates/herdmesh/src/room.rs:57,95` accepts CLI agent strings without that validation. A credential accidentally supplied as `--agent` still leaves in signed authors. Fix: share name validation across CLI config, genesis and session signing. |
| N8 | HOLDS | `crates/hm-daemon/src/ipc.rs:127,130` checks existing token permissions before returning it; `crates/hm-net/src/secure_file.rs:337,341` also rejects untrusted Windows owners. Broad-ACL token regression passes. |

New findings (N9-N12):

- **N9 [HIGH] `crates/hm-store/src/settle.rs:199,242,245` — renewal scope expansion steals settled paths.**
  A claims [a] at T/L1; B claims [b] at T+60s/L10 and settles; A claims [a,b] at T+120s/L20 while its first lease lives.
  Superset renewal inherits T/L1 for both paths; the signed-time guard uses root T, so A wins b and B loses. No forged clock is needed.
  Fix: preserve priority only for previously held scope; treat added paths as fresh claims, or reject expansions conflicting with settled leases.
- **N10 [MED] `crates/hm-store/src/store.rs:924`; `crates/hm-net/src/sync.rs:1078,1156` — bounded hole lists permanently starve later gaps.**
  A holds seq129,131; B holds129,130,131; seq1..128 never existed. Every sync repeats holes1..128, advertises max131, and omits130; hi=0 also disables prefix checks.
  A malicious author can selectively distribute task/lease events that honest replicas never reconcile. Fix: paginate sparse ranges with a persistent continuation independent of whether earlier holes fill.
- **N11 [MED] `crates/hm-daemon/src/feed.rs:306,352` — peer-controlled collections exceed the IPC response budget and disable live view.**
  Six same-holder live claims, each containing one distinct path plus 60,000 empty paths, each fit 64 KiB yet produce >1 MiB JSON together; distinct paths prevent renewal collapse.
  Unbounded historical member names are another persistent trigger. `crates/hm-daemon/src/ipc.rs:345` refuses the response; `crates/herdmesh/src/view.rs:220` returns 502 on every poll.
  Fix: cap nested paths/members and total serialized response bytes below MAX_FRAME, with deterministic pagination/truncation metadata; validate input collection counts.
- **N12 [MED] `crates/hm-daemon/src/feed.rs:217,287,389` — polling/detail queries decode unbounded peer history under daemon locks.**
  Feed reads the whole log and all matching arrivals before its 200-event cut; detail scans the whole log for one ID. `crates/hm-daemon/src/core.rs:933,940` holds global room/store locks throughout.
  A room peer accumulating valid events turns normal one-second view polling into repeated memory/CPU work blocking daemon operations; async IPC deadlines do not preempt synchronous work.
  `crates/herdmesh/src/room.rs:279,283` likewise loads/renders all history before --limit. Fix: bounded indexed queries, incremental activity state, and SQL-level log/detail limits.

Other reviewed controls: `crates/herdmesh/src/view.rs:66,113,123,130,133,168,189,212,243,258` provides random 128-bit token, exact Host allowlist, GET-only routes, static asset allowlist, security headers, 16 KiB heads, five-second header deadline, loopback bind and 32 slots.
No filesystem path resolution, CORS allowance, JSON-to-HTML interpolation, or peer-text execution sink found. Response writes (:237) lack a deadline; a large authenticated response is required to pin them, so this is not an unauthenticated slowloris finding.
`crates/hm-daemon/src/feed.rs:33,205,389,449` redacts/sanitizes text, caps detail text at 8 KiB and uses insertion cursors; `crates/herdmesh/assets/view/view.js` uses textContent and encoded event IDs. Room-log controls/bidi are escaped.
Validation: `just check` passed (fmt, clippy -D warnings, workspace tests); `node --test crates/herdmesh/assets/view/tests/view.test.js` passed 13/13. Windows runtime checks; Unix ownership paths reviewed statically.
Counts: prior fixes 5 HOLDS / 3 BROKEN; 4 new findings; unresolved total 2 HIGH / 4 MED / 1 LOW. Lease integrity failures block release.
VERDICT: FAIL
