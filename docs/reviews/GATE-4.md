# GATE-4 adversarial security review

Target: `cx/astra-gate4` @ `e17607c74f527807dfe8686320f4a48d16103b8f`; compared with GATE-3, ADR-002 Addendum 2, ADR-003 and ADR-004.
Threats: malicious signing room members and attacker-controlled project trees. Source-validated traces below; no new exploit tests or source edits. Paths are repository-relative.
HOLDS means the protection holds for the reviewed finding; BROKEN includes incomplete fixes. Previously closed controls were checked for relevant regressions.

| GATE-3 | Result | Evidence / assessment |
|---|---|---|
| N1 | HOLDS | `crates/hm-daemon/src/ipc.rs:195,216,591,606,643`: role-separated nonce proofs, owner check before authentication, owner-only pipe DACL; IPC/peer-owner code unchanged since GATE-3. |
| N2 | HOLDS | `crates/hm-net/src/sync.rs:337,348,1218,1225`; `crates/hm-store/src/store.rs:1015`: bounded sequence/ID inventories compare sparse twins even with hi=0. Regression: `sync_twins::sparse_twins_without_any_prefix_converge`. |
| N3 | BROKEN [HIGH] | `crates/hm-store/src/store.rs:361,382,841,852`: direct late takeovers, including claim twins, are refused; losing claims and releases still bypass the promised local finality. Concrete release-triggered takeover below. |
| N4 | HOLDS | `crates/hm-store/src/store.rs:955,962,1015`; `crates/hm-net/src/sync.rs:321,337`: author pages and sparse scans retain SQL LIMITs and server work caps. |
| N5 | HOLDS | `crates/hm-store/src/store.rs:697,794`; `crates/hm-net/src/sync.rs:76,1126,1131`: 64 KiB event cap on single/batch ingestion and framed sync input. |
| N6 | HOLDS | `crates/hm-daemon/src/core.rs:30,134,591,592`: independent scan positions, at most 128 events per author and 64 remembered holes remain. |
| N7 | HOLDS | `crates/hm-proto/src/names.rs:27`; `crates/herdmesh/src/session.rs:157,287`, `room.rs:66,127`, `data.rs:342`; `crates/hm-daemon/src/core.rs:288,1026`: shared shape/secret checks cover CLI, config-to-session, genesis and daemon signing paths. |
| N8 | HOLDS | `crates/hm-daemon/src/ipc.rs:130`; `crates/hm-net/src/secure_file.rs:334`: existing token ACL and Windows owner checks remain unchanged. |
| N9 | HOLDS | `crates/hm-store/src/settle.rs:203,206`: renewal requires literal subset membership; the original [a] -> [a,b] expansion gets a fresh root. `store.rs:382` also rejects shrinking renewals that immediately displace a settled rival. |
| N10 | BROKEN [MED] | The original 128-hole case is fixed (`sync_seq::sparse_gap_after_128_unfillable_holes_is_fetched`), but `crates/hm-net/src/sync.rs:337,344,1277,1306,1318` prevents ALL sparse progress whenever one event is rejected/deferred. A refused takeover in the first eight held sequences repeatedly blocks later gaps; trace below. `crates/hm-store/src/store.rs:1015,1029,1035` persists cursors but cannot advance them past this poison event. |
| N11 | HOLDS | `crates/hm-daemon/src/feed.rs:26,29,371,438,546`: paths/references, member lists and serialized JSON are capped (128 KiB), with truncation metadata; the prior oversized IPC response is prevented. |
| N12 | BROKEN [MED] | `crates/hm-daemon/src/feed.rs:360,365` calls `claim_state` up to 256 times; `crates/hm-store/src/settle.rs:195,203,376,380` rebuilds renewal relationships over ALL historical claims before even checking expiry. `crates/hm-daemon/src/core.rs:1029,1036` holds global/store locks. Indexed history/detail reads are fixed (`store.rs:1450,1466`), but peer-grown lease history still makes polling unbounded. `crates/herdmesh/src/room.rs:317` also retains an unlimited initial log read. Fix: bounded/incremental lease resolution, compute once per snapshot, and SQL-level initial-log limits. |

Addendum 2 residual-risk judgment:
- N3 counterexample: attacker X holds W=[a], L1/ts=T; victim V=[b], L10/ts=T arrives at T and is safe at T+3s. At T+60s a second attacker identity submits A=[a,b], L2/ts=T (all TTLs one hour). A loses to W, so `displaced_by` finds no displaced winner and admission accepts it.
- At T+63s X releases W. `store.rs:361` exempts releases; `settle.rs:255,272,358` removes W and promotes A, revoking V while V is live. This needs neither a partition nor forged signatures. It remains possible after V has again passed `claim_safe` following A's arrival.
- Therefore the documented "loser revival" is an active HIGH-severity bypass of local finality, not an acceptable release residual. Fix: preserve settled winners across release/expiry/equivocation transitions; a dormant loser must freshly compete before activation. Expiry alone must not bypass the protection.
- Partition and same-instant bootstrap divergence are acceptable only for explicitly advisory coordination, not global exclusion. A malicious peer can exploit selective delivery; rejected events are absent from those replicas and indefinite renewal can prolong divergence. The ADR must not promise unconditional eventual convergence or local finality with the above bypass outstanding.
- N10 interaction: client knows attacker seq1; server holds seq1..10, with seq2 a claim refused against the client's settled victim. First sweep inserts seq3..8 but refuses seq2, so cursor stays zero. Every subsequent sweep reoffers only seq2 from the same first-eight range; seq9..10 never cross while refusal persists (up to the live lease's hours-long expiry), even between honest replicas with different admission histories.
- Fix N10: track refused/deferred IDs in a bounded retry mechanism independently of the sparse scan continuation, so rejecting an event neither loses it forever nor blocks unrelated later events. Add a combined lease-admission/sparse-sync regression.

New findings (N13-N16):
- **N13 [HIGH] `crates/herdmesh/src/setup.rs:335,387,391,407`; `crates/herdmesh/src/project.rs:17,22` — setup writes outside the project through links.**
  An attacker-supplied `.codex` directory symlink/junction redirects the approved project config update into a user config directory. A pre-existing `CLAUDE.herdmesh-tmp` symlink makes `fs::write` truncate its external target before rename; `.herdmesh/room` is also written directly through links, even if config edits are declined.
  Fix: reject links/reparse points through every destination component, enforce project containment with race-resistant handles, and use exclusive unpredictable temporary files; include the room binding in the reviewed write plan.
- **N14 [MED] `crates/herdmesh/src/setup.rs:429,466,470,472,510,537` — basename collisions silently bind a project to the wrong room.**
  Two unrelated directories named `app` (or names colliding after sanitization/truncation) share the first room. Even an explicit `--join <different-ticket>` is ignored before parsing, while generated MCP configs and `.herdmesh/room` select the old room: subsequent project context reaches its existing peers.
  Fix: honor and verify explicit ticket room IDs first; persist an unambiguous project-to-room-ID binding, and require explicit reuse when only a display name matches.
- **N15 [MED] `crates/hm-daemon/src/core.rs:221,228,231,237` — failed node-qualified addressing falls back to attacker-controlled person names.**
  With no exact `deadbe/alice` member, a peer joining as `deadbe-alice` receives an Ask/Handoff addressed to `deadbe/alice`; the key-prefix selector silently becomes a name selector. This is recipient identity confusion, not a claim of private-message confidentiality inside a shared room.
  Fix: give node-qualified addresses distinct syntax and fail closed when that node/name is absent; never reinterpret an identity-qualified miss as a person alias.
- **N16 [MED] `crates/hm-daemon/src/core.rs:254,259,271,275,699,702` — unknown/ambiguous recipient errors perform quadratic work under global locks.**
  A member can accumulate arbitrary author names (`crates/hm-store/src/views.rs:124`); `names_list` scans all N agents once per agent, then sorts everything before keeping 20. An ordinary mistyped Ask/Handoff triggers O(N^2) work and stalls other rooms (`core.rs:1029,1036`).
  Fix: maintain bounded/indexed name lookup and duplicate counts, cap error enumeration before formatting, and avoid history-wide work while holding global locks.

Other reviewed boundaries: setup uses serde JSON/TOML values (`setup.rs:133,181,198`) and a compiled-in instruction snippet (`:28,367`), so no string-to-config injection found. Clipboard data is stdin/stdout, never PowerShell source (`clipboard.rs:25,95,115`); tickets use the normal parser (`room.rs:128`). Notifications use sanitized/redacted separate arguments (`herdr_notify.rs:51,63`; `crates/hm-herdr/src/cli.rs:92,93`). Doctor executes only the selected herdr binary with fixed `--version` / `plugin list --json` arguments (`doctor.rs:103,179,275`); no peer-derived command execution found. Naming validates generated filenames (`naming.rs:81,103`).
Validation: `just check` passed (fmt, clippy -D warnings, all workspace tests, exit 0), including sparse twins/gaps, lease admission and feed-budget regressions. New attack traces are source-reviewed, not executed; Unix link behavior was reviewed statically on this Windows host.
Counts: 9 HOLDS / 3 BROKEN (of the seven previously open: 4 HOLDS / 3 BROKEN); 4 new; unresolved total 2 HIGH / 5 MED.
VERDICT: FAIL
