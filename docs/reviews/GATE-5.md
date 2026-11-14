# GATE-5 focused adversarial security review

Target: `cx/astra-gate5` @ `3f3318872a7a7d28e2baec036d8f5f26fa63c783`; new-code scope: `git diff fd3bfe7..HEAD`. Inputs: GATE-4 and ADR-002 Addendum 3.
Threats: malicious signing room members and attacker-controlled project trees. HOLDS means the reviewed protection holds; BROKEN includes incomplete fixes. Paths are repository-relative.

| GATE-4 | Result | Evidence / assessment |
|---|---|---|
| N3 | BROKEN [HIGH] | `crates/hm-store/src/store.rs:375,397,846,874,885,911`: releases and twins are checked individually against one unchanged pre-batch book. Two individually harmless changes jointly revive a loser and displace a settled victim; trace below. `crates/hm-store/src/settle.rs:338,355` recomputes deadness from the resulting set. |
| N10 | HOLDS | `crates/hm-net/src/sync.rs:1220,1237,1285,1289`; `crates/hm-store/src/store.rs:1039,1059,1109,1146`: peer-scoped refused/deferred ID inventory, 60-second TTL, 1,024 entries/peer and 4,096 total; sparse continuation advances independently. Refused-tail, deferred-tail, restart, expiry and peer-isolation regressions pass. |
| N12 | BROKEN [MED] | `crates/hm-store/src/settle.rs:249,255,273,332,521,584`: unchanged-book caching removes repeated builds, but cheap mutations invalidate it and the next query rebuilds unbounded history. Signed-time windows do not bound attacker-chosen timestamp density. `crates/hm-daemon/src/core.rs:1110,1117` still holds global/store locks during feed handling. |
| N13 | BROKEN [HIGH] | `crates/herdmesh/src/setup.rs:333,425,429` rejects static links/reparse points and exclusively creates random temporary files; `:435,436,447,448` still separates pathname checks from pathname use. Concurrent swaps bypass containment, including secret reads through CLAUDE.md. |
| N14 | HOLDS | Original basename-collision/ignored-ticket cases fixed: `crates/herdmesh/src/setup.rs:649,665,670,691,699,709` parses explicit tickets first, checks room IDs and chooses unique names; `crates/herdmesh/src/project.rs:24,49,71` stores canonical-path bindings outside the checkout. New lossy-path collision is N18 below. |
| N15 | HOLDS | `crates/hm-daemon/src/core.rs:243,245,252`: hex node-qualified selectors return their narrowed result directly, including misses; no person-alias fallback. Qualified-miss regressions pass. |
| N16 | HOLDS | Original quadratic member enumeration under locks fixed: `crates/hm-daemon/src/core.rs:227,274,291,743,761,1104` indexes names, caps candidates before formatting and resolves after releasing snapshot locks. New name-length amplification is N17 below. |

N3 release-triggered and twin-swap counterexamples (source-validated):
- Distinct permitted attackers hold W1=[a], L1 and W2=[c], L2; victim V=[b], L10. All have signed ts=T and one-hour TTL, arrive at T, and settle. At T+60s another attacker submits A=[a,b,c], L4/ts=T. A loses to W1; V remains safe after the local conflict window.
- At T+63s one batch contains own-node releases R1(W1) and R2(W2), each L3. Each isolated admission check exempts its released target (`store.rs:409`) and leaves another blocker killing A. Both pass. Combined, both releases sort before A (`settle.rs:566`), so A revives and kills V. No partition or forged signature is needed.
- Twin variant: first release W1/W2 normally with keys above A, preserving A's deadness. Then batch two lower-ID seq1 Note twins replacing W1/W2. Each isolated removal leaves the other historical killer and removes no live blocker; together they erase both killers and revive A against V.
- Fix: validate the aggregate tentative batch, including replacements and narrowly scoped release exemptions, against settled winners before committing. Add two-blocker release and twin regressions; the existing single-blocker cases do not cover composition.

N12 residual denial of service:
- A member accumulates expired claims with dense/backdated timestamps and disjoint scopes; predecessor/root walks and killer scans still traverse history (`settle.rs:255,273,332`). An orphan release unconditionally invalidates the cache (`:511,521`); a subsequent claim-state lookup rebuilds it before checking expiry (`:584,588`). Repeated mutations force repeated work under feed locks (`crates/hm-daemon/src/feed.rs:360,365`). Admission can itself incur the same rebuild cost.
- Fix: incrementally maintain or explicitly bound history-dependent work, avoid invalidation for inert releases, and test dense hostile timestamps with a mutation before every poll. The passing spaced-history benchmark does not establish this bound.

N13 residual link race:
- A concurrent checkout writer swaps CLAUDE.md to a link after `setup.rs:447` validates it but before `:448` reads it, then restores a regular file before apply. A victim-readable text secret becomes the old content copied into the new project file (`:508,511,518`). Parent-directory swaps also race temporary-file open/rename (`:430,435,436`). This requires concurrent filesystem control, beyond merely planting a static link.
- Fix: anchor reads and writes to directory handles, reject links/reparse traversal at open time, and preserve component identities through use; test concurrent swaps. Atomic rename and random exclusive temp creation alone do not secure parent traversal.

New findings (changed code only):
- **N17 [MED] `crates/hm-daemon/src/core.rs:229,232` — peer-name prefix expansion is quadratic in name length.** Every dash inserts and hashes another growing prefix. A signed name containing about 60,000 dashes fits the event cap and causes roughly 1.8 GB of prefix hashing plus 60,000 alias entries on every recipient lookup. Peer ingestion does not enforce local name limits (`crates/hm-proto/src/event.rs:300`; `crates/hm-store/src/views.rs:124`). This CPU/memory amplification remains outside the global lock but runs synchronously in request handling. Fix: enforce bounded peer-name bytes/shape before indexing and bound alias expansion; add an oversized-name regression.
- **N18 [MED] `crates/herdmesh/src/project.rs:28,72,74` — lossy path hashing aliases distinct projects.** On Unix, sibling canonical paths ending in raw byte 0xff versus 0xfe both stringify to the same replacement character, producing the same binding key. Setup without an explicit ticket reuses the first room (`crates/herdmesh/src/setup.rs:665,681`), exposing the second project's context to its peers. Fix: hash and persist lossless OS path identity, verify it on lookup, and test non-UTF-8 siblings.

Addendum 3 judgment: advisory partition/same-instant divergence remains an explicitly limited guarantee. The later-arrival local-finality promise is still exploitable by N3; the signed-time cache window does not establish the bounded-cost claim in N12.
Validation: `just check` passed on this HEAD (fmt, clippy -D warnings, all workspace tests; exit 0). Counterexamples are source-validated, not executed exploit tests. Unix path behavior and concurrent link races were reviewed statically on this Windows host. No source files changed.
Counts: 4 HOLDS / 3 BROKEN of seven reviewed; 2 NEW; unresolved total 2 HIGH / 3 MED.
VERDICT: FAIL
