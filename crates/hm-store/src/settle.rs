//! The lease model (ADR-005 §1). Leases are cooperative: an advisory "I am
//! editing these paths" marker. Pure, no I/O, no clock: every time-based
//! answer takes `now` (unix millis) as an argument.
//!
//! This module is the single source of truth for leases. [`crate::Views`]
//! keeps a [`LeaseBook`] and asks it; [`claim_state`] builds one from a slice
//! of events for callers without a store.
//!
//! # Rule
//! Claims are applied in [`OrderKey`] order `(lamport, author node, id)`.
//! Each claim's fate is decided once, when it is applied, against the
//! claims applied before it that are still *open* (not dead, released or
//! renewed):
//!
//! - **Renewal**: the most recent open claim by the same holder (node +
//!   agent) whose paths include all of this one's (same or subset), signed
//!   (`ts`) before it expired. The renewal takes the original's place in line
//!   (its *root*); the original becomes `Renewed`; dropped paths are freed.
//! - **Dead**: otherwise, if an open claim of another holder overlaps it and
//!   their lifetimes `[ts, ts + min(ttl, MAX_LEASE_TTL))` overlap, it is dead:
//!   `Lost { winner }` for good. A release or expiry of the winner never
//!   promotes it; its holder must claim again. A re-claim of a dead claim is
//!   a new claim.
//! - A release from the claim's own node closes it (`Released`); releases
//!   from other nodes are ignored. A claim expires at `ts + min(ttl,
//!   MAX_LEASE_TTL)`.
//!
//! At `now` the open, unexpired claims are walked lowest (root key, own key)
//! first; a claim wins unless it overlaps an earlier winner of another holder
//! (sets are atomic). Winning claims are `Pending` until `root.ts +
//! settle_ms`, then `Granted`. Only open claims are indexed, and claims that
//! expired well before the latest signed claim time are dropped from that
//! index, so long histories cost nothing per query.
//!
//! Everything above reads only signed fields, so the result is a pure
//! function of the event set and `now`. Local arrival times (for the
//! "conflict seen recently" hint, `Store::claim_safe`) live in the store.
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

use hm_proto::limits::{MAX_FUTURE_SKEW_MS, MAX_LEASE_TTL_SECS};
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
    if a == b {
        return true;
    }
    if !has_meta(a) && !has_meta(b) {
        return false;
    }
    if glob_matches(a, b) || glob_matches(b, a) {
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

/// All claims and releases, plus the per-claim facts decided when each claim
/// was applied. Claims and releases must be added in [`OrderKey`] order
/// (as [`crate::Views`] does); [`book_from_events`] sorts for you.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBook {
    claims: BTreeMap<EventId, ClaimRec>,
    /// claim id → lowest order key of a release, per releasing node.
    releases: BTreeMap<EventId, BTreeMap<NodeId, OrderKey>>,
    /// claim → its direct renewal.
    renewed_by: BTreeMap<EventId, EventId>,
    /// renewal → root of its chain.
    root: BTreeMap<EventId, EventId>,
    /// root → latest claim of its chain.
    chain_head: BTreeMap<EventId, EventId>,
    /// dead claim → the claim that beat it.
    dead: BTreeMap<EventId, EventId>,
    /// `(expires_at, id)` of open claims: not dead, released or renewed.
    open: BTreeSet<(u64, EventId)>,
    /// The same open claims per holder.
    open_by_holder: BTreeMap<AgentRef, BTreeSet<(u64, EventId)>>,
    /// The same open claims per (holder, path), for renewal lookup.
    open_by_path: BTreeMap<(AgentRef, String), BTreeSet<(u64, EventId)>>,
    /// Latest signed `ts` of any applied claim.
    max_ts: u64,
}

/// Range start for "expires after `t`" in a `(expires_at, id)` set.
fn after(t: u64) -> (u64, EventId) {
    (t.saturating_add(1), EventId::from_bytes([0; 32]))
}

impl LeaseBook {
    fn open_insert(&mut self, c: &ClaimRec) {
        let k = (c.expires_at_ms(), c.id);
        self.open.insert(k);
        self.open_by_holder
            .entry(c.holder.clone())
            .or_default()
            .insert(k);
        for p in &c.paths {
            self.open_by_path
                .entry((c.holder.clone(), p.clone()))
                .or_default()
                .insert(k);
        }
    }

    fn open_remove(&mut self, id: &EventId) {
        let Some(c) = self.claims.get(id) else { return };
        let k = (c.expires_at_ms(), c.id);
        if !self.open.remove(&k) {
            return;
        }
        if let Some(s) = self.open_by_holder.get_mut(&c.holder) {
            s.remove(&k);
            if s.is_empty() {
                self.open_by_holder.remove(&c.holder);
            }
        }
        for p in &c.paths {
            let pk = (c.holder.clone(), p.clone());
            if let Some(s) = self.open_by_path.get_mut(&pk) {
                s.remove(&k);
                if s.is_empty() {
                    self.open_by_path.remove(&pk);
                }
            }
        }
    }

    /// Apply a claim. Idempotent. Call in [`OrderKey`] order.
    pub fn add_claim(&mut self, c: ClaimRec) {
        if self.claims.contains_key(&c.id) {
            return;
        }
        self.max_ts = self.max_ts.max(c.ts);
        // Drop open claims that ended well before the newest signed time.
        let stale = self.max_ts.saturating_sub(MAX_FUTURE_SKEW_MS);
        while let Some(&(exp, id)) = self.open.first() {
            if exp > stale {
                break;
            }
            self.open_remove(&id);
            // In case the record is gone; guarantees progress.
            self.open.remove(&(exp, id));
        }
        let (id, exp) = (c.id, c.expires_at_ms());
        let released = self.released(&c);
        let coexists = |v: &ClaimRec| v.ts < exp && c.ts < v.expires_at_ms();
        // Renewal: the most recent open, coexisting claim of the same
        // holder naming every path of this one.
        let parent = match c.paths.first() {
            Some(p0) => self.open_by_path.get(&(c.holder.clone(), p0.clone())),
            None => self.open_by_holder.get(&c.holder),
        }
        .into_iter()
        .flat_map(|s| s.range(after(c.ts)..))
        .filter_map(|(_, v)| self.claims.get(v))
        .filter(|p| coexists(p) && c.paths.iter().all(|x| p.paths.contains(x)))
        .max_by_key(|p| p.key())
        .map(|p| p.id);
        // Open, coexisting, overlapping claims of other holders.
        let rivals: Vec<EventId> = self
            .open_by_holder
            .iter()
            .filter(|(h, _)| **h != c.holder)
            .flat_map(|(_, s)| s.range(after(c.ts)..))
            .filter_map(|(_, v)| self.claims.get(v))
            .filter(|v| coexists(v) && sets_overlap(&v.paths, &c.paths))
            .map(|v| v.id)
            .collect();
        self.claims.insert(id, c);
        if let Some(p) = parent {
            let root = self.root.get(&p).copied().unwrap_or(p);
            self.renewed_by.insert(p, id);
            self.root.insert(id, root);
            self.chain_head.insert(root, id);
            self.open_remove(&p);
            // GATE-7 N22: the renewal keeps the root's place in line, so a
            // rival that sorts after the root loses to it for good.
            let root_key = self.claims.get(&root).map(ClaimRec::key);
            for v in rivals {
                let vk = self.claims.get(&v).map(|r| (self.root_key(r), r.key()));
                if let (Some(rk), Some(vk)) = (root_key, vk)
                    && vk.0 > rk
                {
                    self.dead.insert(v, id);
                    self.open_remove(&v);
                }
            }
            if !released && let Some(rec) = self.claims.get(&id).cloned() {
                self.open_insert(&rec);
            }
        } else if let Some(k) = rivals
            .iter()
            .filter_map(|v| self.claims.get(v))
            .min_by_key(|v| (self.root_key(v), v.key()))
            .map(|v| v.id)
        {
            self.dead.insert(id, k);
        } else if !released && let Some(rec) = self.claims.get(&id).cloned() {
            self.open_insert(&rec);
        }
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
        if self.claims.get(&claim).is_some_and(|c| c.holder.node == by) {
            self.open_remove(&claim);
        }
    }

    /// ADR-005 §2: a node demoted to observer no longer holds anything. Its
    /// open claims are dropped (claims they beat stay beaten).
    pub fn retire_node(&mut self, node: &NodeId) {
        let gone: Vec<EventId> = self
            .open_by_holder
            .iter()
            .filter(|(h, _)| h.node == *node)
            .flat_map(|(_, s)| s.iter().map(|(_, id)| *id))
            .collect();
        for id in gone {
            self.open_remove(&id);
            self.claims.remove(&id);
        }
    }

    pub fn claim(&self, id: &EventId) -> Option<&ClaimRec> {
        self.claims.get(id)
    }

    pub fn claims(&self) -> impl Iterator<Item = &ClaimRec> {
        self.claims.values()
    }

    /// Released by the claim's own node.
    fn released(&self, c: &ClaimRec) -> bool {
        self.releases
            .get(&c.id)
            .is_some_and(|m| m.contains_key(&c.holder.node))
    }

    fn root_rec<'a>(&'a self, c: &'a ClaimRec) -> &'a ClaimRec {
        self.root
            .get(&c.id)
            .and_then(|r| self.claims.get(r))
            .unwrap_or(c)
    }

    fn root_key(&self, c: &ClaimRec) -> OrderKey {
        self.root_rec(c).key()
    }

    /// Latest claim of `id`'s renewal chain (O(log n), no chain walk).
    fn head(&self, id: EventId) -> EventId {
        let root = self.root.get(&id).copied().unwrap_or(id);
        self.chain_head.get(&root).copied().unwrap_or(id)
    }

    /// Open claims unexpired at `now`, in priority order, each with the
    /// claim that beats it (if any). Visits only claims unexpired at `now`.
    fn walk(&self, now: u64) -> Vec<(&ClaimRec, Option<EventId>)> {
        let mut live: Vec<&ClaimRec> = self
            .open
            .range(after(now)..)
            .filter_map(|(_, id)| self.claims.get(id))
            .collect();
        live.sort_by_key(|c| (self.root_key(c), c.key()));
        let mut winners: Vec<&ClaimRec> = Vec::new();
        let mut out = Vec::with_capacity(live.len());
        for c in live {
            let beaten = winners
                .iter()
                .find(|w| w.holder != c.holder && sets_overlap(&w.paths, &c.paths))
                .map(|w| w.id);
            if beaten.is_none() {
                winners.push(c);
            }
            out.push((c, beaten));
        }
        out
    }

    /// State of claim `id` at `now` with the default settle window.
    pub fn state(&self, id: &EventId, now: u64) -> Option<ClaimState> {
        self.state_with(id, now, SETTLE_MS)
    }

    /// State of claim `id` at `now`. `None` if `id` is not a known claim.
    /// Order: released, expired (checked before any chain lookup), renewed,
    /// dead, then the walk over live claims.
    pub fn state_with(&self, id: &EventId, now: u64, settle_ms: u64) -> Option<ClaimState> {
        let target = self.claims.get(id)?;
        if self.released(target) {
            return Some(ClaimState::Released);
        }
        if target.expired(now) {
            return Some(ClaimState::Expired);
        }
        if self.renewed_by.contains_key(id) {
            return Some(ClaimState::Renewed { by: self.head(*id) });
        }
        if let Some(k) = self.dead.get(id) {
            return Some(ClaimState::Lost {
                winner: self.head(*k),
            });
        }
        let (_, beaten) = self.walk(now).into_iter().find(|(c, _)| c.id == *id)?;
        if let Some(winner) = beaten {
            return Some(ClaimState::Lost { winner });
        }
        let root_ts = self.root_rec(target).ts;
        Some(if now >= root_ts.saturating_add(settle_ms) {
            ClaimState::Granted
        } else {
            ClaimState::Pending
        })
    }

    /// Path → lease for every winning claim at `now` (pending or granted).
    pub fn active(&self, now: u64) -> BTreeMap<String, Lease> {
        let mut out: BTreeMap<String, Lease> = BTreeMap::new();
        for (c, beaten) in self.walk(now) {
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

    /// Open claims of other holders, unexpired at `now`, whose paths overlap
    /// claim `id`'s.
    pub fn conflicting(&self, id: &EventId, now: u64) -> Vec<EventId> {
        let Some(target) = self.claims.get(id) else {
            return Vec::new();
        };
        let mut out: Vec<EventId> = self
            .walk(now)
            .into_iter()
            .map(|(c, _)| c)
            .filter(|c| c.holder != target.holder && sets_overlap(&c.paths, &target.paths))
            .map(|c| c.id)
            .collect();
        out.sort();
        out
    }
}

/// Build a book from raw events (no role checks), applied in order-key
/// order. Releases count only from the claim's node, as everywhere.
pub fn book_from_events(events: &[Event]) -> LeaseBook {
    let mut sorted: Vec<&Event> = events.iter().collect();
    sorted.sort_by_key(|e| order_key(e));
    let mut book = LeaseBook::default();
    for e in sorted {
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
