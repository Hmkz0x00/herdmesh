# GATE-6 focused adversarial security review

Target: `cx/astra-gate6` @ `bd76ce8744048549844305fca26f346731e5a3c4`; new-code range `84de232..HEAD`. Inputs: GATE-5 and ADR-002 Addendum 4.
Threats: malicious signing room members and concurrently attacker-controlled project trees. HOLDS means the reviewed protection holds; BROKEN includes incomplete fixes. Paths are repository-relative.

| GATE-5 | Result | Evidence / assessment |
|---|---|---|
| N3 | BROKEN [HIGH] | `crates/hm-store/src/store.rs:404,470,903,910,935,946`: admission models one final candidate per slot, but storage processes every offered event and applies another rejection rule afterward. The committed set can differ from the set the fixpoint approved. |
| N12 | BROKEN [MED] | `crates/hm-store/src/settle.rs:293,308,326,384,607`: the horizon excludes old history and inert orphan releases preserve the cache, but dense history inside the horizon still causes quadratic rebuilds, even after semantically unchanged releases. `crates/hm-store/src/store.rs:358,399` also clones/scans complete lease history during ingestion. |
| N13 | BROKEN [HIGH] | `crates/herdmesh/src/safefs.rs:83,111,185,228` secures opened descendants; `crates/herdmesh/src/setup.rs:334,410` acquires separate roots for planning and application. `safefs.rs:285,287` canonicalizes afresh, allowing replacement of the root entry to redirect the second acquisition. |
| N17 | HOLDS | `crates/hm-daemon/src/core.rs:232,238,339` validates before prefix expansion; `crates/hm-proto/src/names.rs:33` rejects names above 32 bytes before scanning the remaining characters or checking secrets. Oversized names get no aliases or resolver-error entries. |
| N18 | HOLDS | `crates/herdmesh/src/project.rs:27,31,36,48,103` hashes Unix bytes / Windows UTF-16 without lossy conversion and verifies the full path hash on binding lookup. Display text is no longer identity. |

N3 combined-batch recheck and counterexamples:
- The original two-release and two-twin sets, and a mixed release/twin set with one eligible candidate per slot, are caught by aggregate displacement checks (`store.rs:439,458`). Sorting (`:438,897`) makes these checks independent of input permutation; the regression suite covers the original pairs and batch permutation.
- Twin fallback: settle V=[b], L10/ts=T. Offer two attacker claims in the same new (node, seq) slot, both L1/ts=T/[b], differing only in TTL (3,600 versus 3,601 seconds). Both individually steal V. Admission selects and refuses only the lower ID; the other ID is still inserted by `store.rs:946,950`, displacing V. Both input permutations yield the same unsafe result.
- Post-admission filtering: with that settled V and local max L10, offer A=[a,b], L1/ts=T and B=[a], L2,000,000/ts=T-3s from distinct attacker holders, both TTL one hour. In the simulated book B's older signed root kills A (`settle.rs:400`), leaving V safe. Storage inserts A but drops B for its Lamport jump (`store.rs:910`); A then wins b. Reversing the input does not help. A malicious sync response reaches batch ingestion (`crates/hm-net/src/sync.rs:1145,1282`).
- Fix: stage the exact final slots after all admission/eligibility rules, commit only that staged set, and recheck the settled-winner invariant before commit. Refusal must not expose an unchecked higher twin. Add both counterexamples with permutations alongside the combined release/twin tests.

N12 remaining resource attack:
- Keep N disjoint claims from one holder at signed time now-1h, TTL 60s: expired, but inside the roughly 16-hour horizon. The predecessor search (`settle.rs:308`) alone does N(N-1)/2 failed subset checks. Repeated higher-key releases of an already released own claim still invalidate the index (`:600,607`), despite changing no effective release; the next admission/feed rebuild repeats the work. Feed keeps global/store locks (`crates/hm-daemon/src/core.rs:1119,1126`).
- The new 30-hour-old-history benchmark (`crates/hm-store/tests/lease_batch.rs:184`) does not exercise that density. Also, `store.rs:399` collects all historical claim/release IDs even for non-lease batches, contradicting the no-history-scans claim. Fix: bound admitted horizon density/work, avoid no-op invalidation, and remove whole-history ingestion scans/clones. Addendum 4's density residual remains an exploitable denial of service; rebuilds are not merely O(N).

N13 remaining root check-versus-use race:
- Descendant handles now stop the prior CLAUDE.md and subdirectory link swaps. However, `plan_files` drops its Root when returning the Plan, and `apply` opens it again. An attacker able to rename the root entry (for example, a project nested inside an attacker-writable checkout) can replace it with a link/junction to another victim-writable directory between these calls. Fresh canonicalization accepts that destination; approved project config/snippet contents overwrite files there. No root identity from the plan is verified.
- Fix: acquire and verify one root for the operation, carry the same handle through planning, confirmation and application, and bind the room identity to that anchor. Cover root/ancestor replacement, not only swaps below an already opened root. The explicit linked-root test (`setup.rs:1053`) verifies initial links, not identity continuity.

New finding (changed code):
- **N19 [HIGH] `crates/hm-store/src/settle.rs:263,293,397,483` — horizon advancement revives dead claims and revokes settled leases without any event.** Deadness depends transitively on other claims' deadness; two TTLs of direct overlap are insufficient to preserve that dependency chain.
- Concrete trace: choose an hour-aligned T; five distinct holders, all TTL 8h. Y=[a], L1/ts=T+8h; Z=[a], L2/ts=T+15h; W=[a], L3/ts=T+22h; attacker A=[a,b], L4/ts=T+29h; victim V=[b], L10/ts=T+29h. Receive Y,Z,W,V at T+31h, then A one minute later; wait for local settling. At horizon T+15h, Y kills Z, W kills A, and V safely wins b.
- At T+32h the horizon becomes T+16h and drops Y. Z revives, kills W, A revives and kills V, which remains unexpired until T+37h. No ingestion occurs, so batch admission cannot intervene. Fix: retain the transitive dependency closure or an equivalent deterministic summary; test clock-boundary stability of settled winners and dead claims.

Validation: `just check` passed (fmt, clippy -D warnings, all workspace tests; exit 0), including combined-release/twin, batch-order, horizon-cost, filesystem, name-limit and raw-path regressions. New counterexamples are source-validated, not injected Rust exploit tests; independent in-memory transcriptions confirm the horizon and Lamport-shield traces. Unix filesystem behavior was reviewed statically on this Windows host. No source changes.
Counts: 2 HOLDS / 3 BROKEN of five reviewed; 1 NEW; unresolved total 3 HIGH / 1 MED.
VERDICT: FAIL
