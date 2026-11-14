# GATE-8 focused review

Target: `cx/astra-gate8` @ `74c7996defc9afb76691c9f942d8d9abd8b6112e`; diff `c868391..HEAD`. Scope: ADR-005 + Addendum 1 and N20-N22; excluded attacks remain excluded. HOLDS means the protection holds; incomplete fixes are BROKEN.

| Finding | Result | Evidence |
|---|---|---|
| N20 | BROKEN [MED], consumer gap | Store removal holds: `crates/hm-store/src/views.rs:124,173` resolves roles first and filters content by final role; `crates/hm-store/src/store.rs:283,815` rebuilds for late events, twins and role changes. `crates/hm-brief/src/card.rs:40,49` consumes corrected views; `brief.rs:121,141` and `digest.rs:35` filter final roles. Feed task/lease collections use corrected views, but its activity and event stream bypass that filter (`crates/hm-daemon/src/feed.rs:427,485`). |
| N21 | BROKEN [MED], replay gap | Original same-holder and expired-chain cases are fixed (`crates/hm-store/src/settle.rs:267,379,421`). Query walks visit unexpired claims (`:389`). Replay still scans every coexisting claim of every other holder (`:278`), including history expired at the eventual query time. |
| N22 | BROKEN [MED], opposite priority case | The original earlier-root renewal now permanently kills later rivals (`crates/hm-store/src/settle.rs:297,302`). A renewal whose root sorts AFTER a rival is still inserted into `open` without being marked dead (`:300,306`); its query-time loss (`:433`) disappears when that rival releases. |

Remaining traces / fixes:
- **N20:** after an owner demotes M at L10, M's fresh, unused-sequence TaskAdd/Decision/LeaseClaim at L9 is ignored by the store, but `feed.rs:474,485,499` publishes it as an ordinary event. For a member with a retained Join, `:427,447` also reports the forbidden write as current activity. Backdating no longer changes the board/card/brief/digest, but the live UI still displays removed-member content without an ignored marker.
  Fix: apply the final-role predicate to operational feed events and activity, advancing cursors over skipped arrivals; preserve the explicitly separate raw audit log. Add consumer regressions for removal, backdated arrivals and pagination. Severity falls from HIGH to MED because materialized state is now protected.
- **N21:** alternate 10,000 disjoint literal-path claims between just TWO holders, all ts=T/TTL60s, and rebuild/query at T+1day. `max_ts` stays T, so `settle.rs:253` never prunes them during replay; parent lookup is cheap, but `:279,282,284` performs 25,000,000 cross-holder overlap checks. No claims are live at query time. The new benchmark (`crates/hm-store/tests/lease.rs:260`) covers one holder only.
  Fix: bound/index cross-holder historical overlap work as well as renewal lookup; benchmark dense expired history with multiple holders through rebuild. The per-query/chain fixes hold; the requested expired-history replay bound remains incomplete.
- **N22:** all paths [a]. Bob W0=(L1, ts=T+3s, TTL2s), W1=(L2, ts=T+4s, TTL10s), renewing W0. Alice P=(L3, ts=T, TTL2s), R=(L4, ts=T+1s, TTL10s), renewing P. Each holder's timestamps increase; four-second inter-node skew suffices. P does not coexist with W1, but R does. R's later root cannot kill W1, yet R is left open rather than recorded dead.
  At T+6s R reports Lost to W1. Bob releases W1 at L5/T+6s; R becomes Granted without a fresh claim. Sorting the same set makes delivery permutations equivalent, including this incorrect outcome. This remains a cooperative loser-persistence defect.
  Fix: decide a renewal's own fate against earlier roots before killing later rivals, remove a losing renewal from `open`, and test both priority directions after release/expiry.

New findings outside these three: none in the scoped diff. No source files changed.
Validation: `just check` passed (fmt, clippy -D warnings, all workspace tests; exit 0), including removal/backdating/reopen/order, dense same-holder history, expired renewal chains and the original N22 regression. Remaining traces are source-validated; an independent in-memory transcription confirms the opposite-priority N22 transition, not execution of an added Rust test.
Counts: 0 HOLDS / 3 BROKEN (partial fixes); 0 NEW; unresolved 3 MED.
VERDICT: FAIL
