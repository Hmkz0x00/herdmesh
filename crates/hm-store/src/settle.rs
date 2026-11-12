//! The lease model (ADR-001 note 2, ADR-002 §1). Pure, no I/O, no clock:
//! every time-based answer takes `now` (unix millis) as an argument.
//!
//! This module is the single source of truth for leases. [`crate::Views`]
//! keeps a [`LeaseBook`] and asks it; [`claim_state`] builds one from a slice
//! of events for callers without a store.
//!
//! # Rule
//! A `LeaseClaim` names a set of path globs and a TTL. At time `now` a claim
//! is, in this order of precedence:
//!
//! 1. **Released**: a `LeaseRelease { claim }` naming it exists from the
//!    claim's *author node*. Releases from other nodes are ignored.
//! 2. **Renewed { by }**: a later claim by the same holder (node + agent)
//!    covers a superset of its paths, was signed (`ts`) before it expired,
//!    and no release of it sorts before that renewal. The renewal takes over
//!    the original's place in line (its *root*). `by` is the latest renewal.
//! 3. **Expired**: `now >= ts + min(ttl, MAX_LEASE_TTL)`.
//! 4. Otherwise it is *live*. Live claims are sorted by (root's order key,
//!    own order key) and walked lowest-first; a claim **wins** unless one of
//!    its paths overlaps a winner held by someone else. Sets are atomic: a
//!    claim wins all of its paths or none, and a loser blocks nobody.
//!    **Late claims** also lose: a claim loses to any later-sorted live
//!    overlapping claim by another holder whose root `ts + SETTLE_MS` is at
//!    or before its own root `ts` (that lease had settled before this claim
//!    was signed), so a low Lamport can't take over a settled lease. This
//!    only ever adds losers. A signer that also backdates `ts` is not caught
//!    by signed data alone (needs a causal-head field in the claim).
//!    - not a winner → **Lost { winner }** (the first winner it overlaps)
//!    - winner and `now >= root.ts + settle_ms` → **Granted**
//!    - winner inside the settle window → **Pending**
//!
//! Everything above reads only signed fields, so the result depends on the
//! event set and `now`, never on arrival order. Local arrival times (for the
//! "conflict seen recently" safety check) live in the store, not here.
//!
//! # Path overlap
//! Paths are globs (`globset`, `*` does not cross `/`). Two patterns overlap
//! if either one, compiled as a glob, matches the other as a literal path.
//! When *both* contain glob metacharacters this is incomplete
//! (`src/*/a.rs` vs `src/net/*`), so we also treat them as overlapping if
//! their literal prefixes (text before the first metacharacter) are
//! prefix-compatible. That can over-report overlap between two globs, which
//! is the safe direction for a lease.

use std::collections::{BTreeMap, BTreeSet};

use hm_proto::limits::MAX_LEASE_TTL_SECS;
use hm_proto::{AgentRef, Event, EventBody, EventId, Lamport, NodeId};
use serde::{Deserialize, Serialize};

use crate::views::{OrderKey, order_key};

/// Default settle window in milliseconds.
pub const SETTLE_MS: u64 = 2000;

/// Outcome of a lease claim at a given time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimState {
    /// Winning, but still inside the settle window.
    Pending,
    /// Winning and settled.
    Granted,
    /// Overlaps a claim that wins.
    Lost {
        /// The winning claim's event id.
        winner: EventId,
    },
    /// TTL elapsed.
    Expired,
    /// Released by the claim's author node.
    Released,
    /// Superseded by a renewal from the same holder.
    Renewed {
        /// The latest renewing claim.
        by: EventId,
    },
}

const GLOB_META: &[char] = &['*', '?', '[', '{', '\\'];

fn has_meta(p: &str) -> bool {
    p.contains(GLOB_META)
}

fn glob_matches(pattern: &str, path: &str) -> bool {
    match globset::GlobBuilder::new(pattern)
        .literal_separator(true)
        .build()
    {
        Ok(g) => g.compile_matcher().is_match(path),
        // Invalid glob: fall back to exact comparison.
        Err(_) => pattern == path,
    }
}

fn literal_prefix(p: &str) -> &str {
    p.find(GLOB_META).map_or(p, |i| &p[..i])
}

/// Whether two lease path patterns can name a common file. See module docs.
pub fn paths_overlap(a: &str, b: &str) -> bool {
    if a == b || glob_matches(a, b) || glob_matches(b, a) {
        return true;
    }
    if has_meta(a) && has_meta(b) {
        let (pa, pb) = (literal_prefix(a), literal_prefix(b));
        return pa.starts_with(pb) || pb.starts_with(pa);
    }
    false
}

fn sets_overlap(a: &[String], b: &[String]) -> bool {
    a.iter().any(|x| b.iter().any(|y| paths_overlap(x, y)))
}

/// The signed facts of one `LeaseClaim`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimRec {
    pub id: EventId,
    pub holder: AgentRef,
    pub lamport: Lamport,
    pub ts: u64,
    pub paths: Vec<String>,
    pub ttl_secs: u32,
}

impl ClaimRec {
    /// `Some` if `e` is a `LeaseClaim`.
    pub fn from_event(e: &Event) -> Option<Self> {
        match &e.body {
            EventBody::LeaseClaim { paths, ttl_secs } => Some(Self {
                id: e.id,
                holder: e.author.clone(),
                lamport: e.lamport,
                ts: e.ts,
                paths: paths.clone(),
                ttl_secs: *ttl_secs,
            }),
            _ => None,
        }
    }

    pub fn key(&self) -> OrderKey {
        (self.lamport, self.holder.node, self.id)
    }

    /// `ts + min(ttl, MAX_LEASE_TTL)`.
    pub fn expires_at_ms(&self) -> u64 {
        let ttl = self.ttl_secs.min(MAX_LEASE_TTL_SECS);
        self.ts.saturating_add(u64::from(ttl) * 1000)
    }

    fn expired(&self, now: u64) -> bool {
        now >= self.expires_at_ms()
    }
}

/// A lease currently won by a claim; see [`LeaseBook::active`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: AgentRef,
    pub claim: EventId,
    pub lamport: Lamport,
    pub expires_at_ms: u64,
}

/// All claims and releases seen, as signed data. Answers are computed per
/// call from `now`; nothing time-based is stored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBook {
    claims: BTreeMap<EventId, ClaimRec>,
    /// claim id → per releasing node, the lowest order key of its releases.
    releases: BTreeMap<EventId, BTreeMap<NodeId, OrderKey>>,
}

/// Derived, per-call structure: renewal links and roots.
struct Resolved<'a> {
    book: &'a LeaseBook,
    /// child → parent it renews.
    parent: BTreeMap<EventId, EventId>,
    /// parent → latest child renewing it.
    renewed_by: BTreeMap<EventId, EventId>,
}

impl<'a> Resolved<'a> {
    fn new(book: &'a LeaseBook) -> Self {
        let mut parent = BTreeMap::new();
        let mut renewed_by: BTreeMap<EventId, (OrderKey, EventId)> = BTreeMap::new();
        let mut by_holder: BTreeMap<&AgentRef, Vec<&ClaimRec>> = BTreeMap::new();
        for c in book.claims.values() {
            by_holder.entry(&c.holder).or_default().push(c);
        }
        for list in by_holder.values_mut() {
            list.sort_by_key(|c| c.key());
            for (i, child) in list.iter().enumerate() {
                let have: BTreeSet<&String> = child.paths.iter().collect();
                // Most recent earlier claim this one can renew.
                let found = list[..i].iter().rev().find(|p| {
                    child.ts < p.expires_at_ms()
                        && p.paths.iter().all(|x| have.contains(x))
                        && !book.released_before(p, Some(child.key()))
                });
                if let Some(p) = found {
                    parent.insert(child.id, p.id);
                    let e = renewed_by.entry(p.id).or_insert((child.key(), child.id));
                    if child.key() > e.0 {
                        *e = (child.key(), child.id);
                    }
                }
            }
        }
        Self {
            book,
            parent,
            renewed_by: renewed_by.into_iter().map(|(k, (_, v))| (k, v)).collect(),
        }
    }

    fn root(&self, id: &EventId) -> Option<&'a ClaimRec> {
        let mut cur = *id;
        // Parent keys strictly decrease, so this terminates.
        while let Some(p) = self.parent.get(&cur) {
            cur = *p;
        }
        self.book.claims.get(&cur)
    }

    fn live(&self, c: &ClaimRec, now: u64) -> bool {
        !self.book.released_before(c, None)
            && !self.renewed_by.contains_key(&c.id)
            && !c.expired(now)
    }

    /// Winners at `now` in priority order, and the full sorted live list.
    fn walk(&self, now: u64) -> Vec<(&'a ClaimRec, Option<EventId>)> {
        let mut live: Vec<(OrderKey, &'a ClaimRec)> = self
            .book
            .claims
            .values()
            .filter(|c| self.live(c, now))
            .map(|c| (self.root(&c.id).map_or(c.key(), ClaimRec::key), c))
            .collect();
        live.sort_by_key(|(root, c)| (*root, c.key()));
        let root_ts = |c: &ClaimRec| self.root(&c.id).map_or(c.ts, |r| r.ts);
        let mut winners: Vec<&ClaimRec> = Vec::new();
        let mut out = Vec::with_capacity(live.len());
        for (i, &(_, c)) in live.iter().enumerate() {
            let c_ts = root_ts(c);
            let beaten = winners
                .iter()
                .find(|w| w.holder != c.holder && sets_overlap(&w.paths, &c.paths))
                .map(|w| w.id)
                // Late claim (GATE-2 N3): a later-sorted live claim that had
                // settled, by signed time, before this one was signed beats
                // it, so a low Lamport can't retroactively take a lease.
                .or_else(|| {
                    live[i + 1..]
                        .iter()
                        .find(|(_, v)| {
                            v.holder != c.holder
                                && root_ts(v).saturating_add(SETTLE_MS) <= c_ts
                                && sets_overlap(&v.paths, &c.paths)
                        })
                        .map(|(_, v)| v.id)
                });
            if beaten.is_none() {
                winners.push(c);
            }
            out.push((c, beaten));
        }
        out
    }
}

impl LeaseBook {
    /// Record a claim. Idempotent.
    pub fn add_claim(&mut self, c: ClaimRec) {
        self.claims.insert(c.id, c);
    }

    /// Record that node `by` released `claim` with an event of order `key`.
    pub fn add_release(&mut self, by: NodeId, key: OrderKey, claim: EventId) {
        let e = self
            .releases
            .entry(claim)
            .or_default()
            .entry(by)
            .or_insert(key);
        if key < *e {
            *e = key;
        }
    }

    pub fn claim(&self, id: &EventId) -> Option<&ClaimRec> {
        self.claims.get(id)
    }

    pub fn claims(&self) -> impl Iterator<Item = &ClaimRec> {
        self.claims.values()
    }

    /// Released by the claim's own node (before `before`, if given).
    fn released_before(&self, c: &ClaimRec, before: Option<OrderKey>) -> bool {
        self.releases
            .get(&c.id)
            .and_then(|m| m.get(&c.holder.node))
            .is_some_and(|k| before.is_none_or(|b| *k < b))
    }

    /// State of claim `id` at `now` with the default settle window.
    pub fn state(&self, id: &EventId, now: u64) -> Option<ClaimState> {
        self.state_with(id, now, SETTLE_MS)
    }

    /// State of claim `id` at `now`. `None` if `id` is not a known claim.
    pub fn state_with(&self, id: &EventId, now: u64, settle_ms: u64) -> Option<ClaimState> {
        let target = self.claims.get(id)?;
        if self.released_before(target, None) {
            return Some(ClaimState::Released);
        }
        let r = Resolved::new(self);
        if let Some(by) = r.renewed_by.get(id) {
            return Some(ClaimState::Renewed { by: *by });
        }
        if target.expired(now) {
            return Some(ClaimState::Expired);
        }
        let (_, beaten) = r.walk(now).into_iter().find(|(c, _)| c.id == *id)?;
        if let Some(winner) = beaten {
            return Some(ClaimState::Lost { winner });
        }
        let root_ts = r.root(id).map_or(target.ts, |c| c.ts);
        Some(if now >= root_ts.saturating_add(settle_ms) {
            ClaimState::Granted
        } else {
            ClaimState::Pending
        })
    }

    /// Path → lease for every winning claim at `now` (pending or granted).
    pub fn active(&self, now: u64) -> BTreeMap<String, Lease> {
        let mut out: BTreeMap<String, Lease> = BTreeMap::new();
        for (c, beaten) in Resolved::new(self).walk(now) {
            if beaten.is_some() {
                continue;
            }
            for p in &c.paths {
                let lease = Lease {
                    holder: c.holder.clone(),
                    claim: c.id,
                    lamport: c.lamport,
                    expires_at_ms: c.expires_at_ms(),
                };
                match out.get(p) {
                    Some(cur) if cur.expires_at_ms >= lease.expires_at_ms => {}
                    _ => {
                        out.insert(p.clone(), lease);
                    }
                }
            }
        }
        out
    }

    /// Claims by other holders whose paths overlap claim `id`'s, and that
    /// are not released, renewed or expired at `now`.
    pub fn conflicting(&self, id: &EventId, now: u64) -> Vec<EventId> {
        let Some(target) = self.claims.get(id) else {
            return Vec::new();
        };
        let r = Resolved::new(self);
        self.claims
            .values()
            .filter(|c| c.holder != target.holder && sets_overlap(&c.paths, &target.paths))
            .filter(|c| r.live(c, now))
            .map(|c| c.id)
            .collect()
    }
}

/// Build a book from raw events (no role checks). Releases count only from
/// the claim's node, as everywhere.
pub fn book_from_events(events: &[Event]) -> LeaseBook {
    let mut book = LeaseBook::default();
    for e in events {
        match &e.body {
            EventBody::LeaseClaim { .. } => {
                if let Some(c) = ClaimRec::from_event(e) {
                    book.add_claim(c);
                }
            }
            EventBody::LeaseRelease { claim } => {
                book.add_release(e.author.node, order_key(e), *claim);
            }
            _ => {}
        }
    }
    book
}

/// [`claim_state_with`] using the default [`SETTLE_MS`].
pub fn claim_state(claim_id: &EventId, events: &[Event], now: u64) -> Option<ClaimState> {
    claim_state_with(claim_id, events, now, SETTLE_MS)
}

/// State of the `LeaseClaim` event `claim_id` among `events` at `now`.
/// Returns `None` if `claim_id` is not a `LeaseClaim` in `events`.
pub fn claim_state_with(
    claim_id: &EventId,
    events: &[Event],
    now: u64,
    settle_ms: u64,
) -> Option<ClaimState> {
    book_from_events(events).state_with(claim_id, now, settle_ms)
}
