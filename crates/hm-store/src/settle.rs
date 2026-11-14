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
//!    names the same paths or a subset of them (literal strings), was signed
//!    (`ts`) before it expired, and no release of it sorts before that
//!    renewal. The renewal takes over the original's place in line (its
//!    *root*); dropped paths are freed. `by` is the latest renewal. A claim
//!    naming any path the parent did not is a new claim with its own root
//!    (ADR-002 Addendum 2, GATE-3 N9), so renewal can't inherit priority for
//!    paths it never held.
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

use std::cell::RefCell;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, BinaryHeap, HashMap};
use std::sync::{Arc, OnceLock};

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

/// All claims and releases seen, as signed data. Answers are computed per
/// call from `now`; nothing time-based is stored. The derived index
/// (renewals, roots, deadness) is cached per book state, never serialized.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseBook {
    claims: BTreeMap<EventId, ClaimRec>,
    /// claim id → per releasing node, the lowest order key of its releases.
    releases: BTreeMap<EventId, BTreeMap<NodeId, OrderKey>>,
    #[serde(skip)]
    index: IndexCell,
}

/// Lazily built [`Index`]; reset on every change. Always compares equal,
/// so it never affects view equality.
#[derive(Default)]
struct IndexCell(OnceLock<Arc<Index>>);

impl Clone for IndexCell {
    fn clone(&self) -> Self {
        let cell = OnceLock::new();
        if let Some(i) = self.0.get() {
            let _ = cell.set(Arc::clone(i));
        }
        Self(cell)
    }
}

impl PartialEq for IndexCell {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl Eq for IndexCell {}

impl std::fmt::Debug for IndexCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IndexCell")
    }
}

/// Everything about the book that does not depend on `now`.
#[derive(Debug, Default)]
struct Index {
    /// child → parent it renews.
    parent: BTreeMap<EventId, EventId>,
    /// parent → latest child renewing it.
    renewed_by: BTreeMap<EventId, EventId>,
    /// parent → lowest order key among its renewals.
    first_renewal: BTreeMap<EventId, OrderKey>,
    /// claim → root of its renewal chain (absent: itself).
    root: BTreeMap<EventId, EventId>,
    /// dead claim → the claim that killed it (ADR-002 Addendum 3).
    dead: BTreeMap<EventId, EventId>,
    /// (expires_at, id), latest expiry first.
    by_expiry: Vec<(u64, EventId)>,
}

const MAX_LEASE_TTL_MS: u64 = MAX_LEASE_TTL_SECS as u64 * 1000;

impl Index {
    fn build(book: &LeaseBook) -> Self {
        // Candidate renewal links: most recent earlier claim of the same
        // holder this one can renew. A renewal never grows the path set
        // (GATE-3 N9): wider is a new claim.
        let mut parent: BTreeMap<EventId, EventId> = BTreeMap::new();
        let mut children: BTreeMap<EventId, BTreeSet<(OrderKey, EventId)>> = BTreeMap::new();
        let mut by_holder: BTreeMap<&AgentRef, Vec<&ClaimRec>> = BTreeMap::new();
        for c in book.claims.values() {
            by_holder.entry(&c.holder).or_default().push(c);
        }
        for list in by_holder.values_mut() {
            list.sort_by_key(|c| c.key());
            for (i, child) in list.iter().enumerate() {
                let found = list[..i].iter().rev().find(|p| {
                    child.ts < p.expires_at_ms()
                        && child.paths.iter().all(|x| p.paths.contains(x))
                        && !book.released_before(p, Some(child.key()))
                });
                if let Some(p) = found {
                    parent.insert(child.id, p.id);
                    children
                        .entry(p.id)
                        .or_default()
                        .insert((child.key(), child.id));
                }
            }
        }
        let mut root: BTreeMap<EventId, EventId> = BTreeMap::new();
        for c in book.claims.values() {
            let mut cur = c.id;
            // Parent keys strictly decrease, so this terminates.
            while let Some(p) = parent.get(&cur) {
                cur = *p;
            }
            root.insert(c.id, cur);
        }
        let rec = |id: &EventId| book.claims.get(id);
        let order = |root: &BTreeMap<EventId, EventId>, c: &ClaimRec| -> (OrderKey, OrderKey) {
            let r = root.get(&c.id).and_then(rec).map_or(c.key(), ClaimRec::key);
            (r, c.key())
        };
        let root_ts = |root: &BTreeMap<EventId, EventId>, c: &ClaimRec| -> u64 {
            root.get(&c.id).and_then(rec).map_or(c.ts, |r| r.ts)
        };

        // Deadness (ADR-002 Addendum 3), in walk order so every claim that
        // sorts earlier is decided first. A re-claim of a dead claim is cut
        // loose to start its own chain, which moves it (and its renewals)
        // later in the order.
        let mut by_ts: Vec<&ClaimRec> = book.claims.values().collect();
        by_ts.sort_by_key(|c| (c.ts, c.id));
        let mut heap: BinaryHeap<Reverse<((OrderKey, OrderKey), EventId)>> = book
            .claims
            .values()
            .map(|c| Reverse((order(&root, c), c.id)))
            .collect();
        let mut done: BTreeSet<EventId> = BTreeSet::new();
        let mut dead: BTreeMap<EventId, EventId> = BTreeMap::new();
        let globs = Globs::default();
        while let Some(Reverse((ord, id))) = heap.pop() {
            let Some(c) = rec(&id) else { continue };
            if done.contains(&id) || ord != order(&root, c) {
                continue;
            }
            if let Some(p) = parent.get(&id).copied().filter(|p| dead.contains_key(p)) {
                parent.remove(&id);
                if let Some(s) = children.get_mut(&p) {
                    s.remove(&(c.key(), id));
                }
                let old_root = root.get(&id).copied().unwrap_or(id);
                let mut stack = vec![id];
                while let Some(n) = stack.pop() {
                    if root.get(&n) == Some(&old_root) {
                        root.insert(n, id);
                        if let Some(d) = rec(&n) {
                            heap.push(Reverse((order(&root, d), n)));
                        }
                        if let Some(s) = children.get(&n) {
                            stack.extend(s.iter().map(|(_, k)| *k));
                        }
                    }
                }
                continue;
            }
            done.insert(id);
            let (c_key, c_exp, c_root_ts) = (c.key(), c.expires_at_ms(), root_ts(&root, c));
            // Coexisting claims were signed in (c.ts - MAX_TTL, c.expires).
            let from = c.ts.saturating_sub(MAX_LEASE_TTL_MS);
            let lo = by_ts.partition_point(|v| v.ts <= from);
            let hi = by_ts.partition_point(|v| v.ts < c_exp);
            let killer = by_ts[lo.min(hi)..hi]
                .iter()
                .filter(|v| {
                    v.id != id
                        && v.holder != c.holder
                        && c.ts < v.expires_at_ms()
                        && !book.released_before(v, Some(c_key))
                        && children
                            .get(&v.id)
                            .and_then(|s| s.first())
                            .is_none_or(|(k, _)| *k > c_key)
                })
                .filter(|v| {
                    if order(&root, v) < ord {
                        !dead.contains_key(&v.id)
                    } else {
                        root_ts(&root, v).saturating_add(SETTLE_MS) <= c_root_ts
                    }
                })
                .filter(|v| globs.sets_overlap(&v.paths, &c.paths))
                .min_by_key(|v| order(&root, v))
                .map(|v| v.id);
            if let Some(k) = killer {
                dead.insert(id, k);
            }
        }

        let mut ix = Self {
            dead,
            ..Self::default()
        };
        for (p, s) in &children {
            if let (Some((first, _)), Some((_, last))) = (s.first(), s.last()) {
                ix.first_renewal.insert(*p, *first);
                ix.renewed_by.insert(*p, *last);
            }
        }
        ix.parent = parent;
        ix.root = root.into_iter().filter(|(c, r)| c != r).collect();
        ix.by_expiry = book
            .claims
            .values()
            .map(|c| (c.expires_at_ms(), c.id))
            .collect();
        ix.by_expiry.sort_by(|a, b| b.cmp(a));
        ix
    }
}

/// [`paths_overlap`] with each glob compiled once.
#[derive(Default)]
struct Globs(RefCell<HashMap<String, Option<globset::GlobMatcher>>>);

impl Globs {
    fn matches(&self, pattern: &str, path: &str) -> bool {
        let mut cache = self.0.borrow_mut();
        let m = cache.entry(pattern.to_owned()).or_insert_with(|| {
            globset::GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .ok()
                .map(|g| g.compile_matcher())
        });
        match m {
            Some(m) => m.is_match(path),
            None => pattern == path,
        }
    }

    fn paths_overlap(&self, a: &str, b: &str) -> bool {
        if a == b {
            return true;
        }
        let (ma, mb) = (has_meta(a), has_meta(b));
        if !ma && !mb {
            return false;
        }
        if (ma && self.matches(a, b)) || (mb && self.matches(b, a)) {
            return true;
        }
        if ma && mb {
            let (pa, pb) = (literal_prefix(a), literal_prefix(b));
            return pa.starts_with(pb) || pb.starts_with(pa);
        }
        false
    }

    fn sets_overlap(&self, a: &[String], b: &[String]) -> bool {
        a.iter().any(|x| b.iter().any(|y| self.paths_overlap(x, y)))
    }
}

impl LeaseBook {
    fn index(&self) -> &Index {
        self.index.0.get_or_init(|| Arc::new(Index::build(self)))
    }

    fn touched(&mut self) {
        self.index = IndexCell::default();
    }

    fn root_rec<'a>(&'a self, c: &'a ClaimRec) -> &'a ClaimRec {
        self.index()
            .root
            .get(&c.id)
            .and_then(|r| self.claims.get(r))
            .unwrap_or(c)
    }

    /// Unexpired at `now`, latest expiry first; O(unexpired), not history.
    fn unexpired(&self, now: u64) -> impl Iterator<Item = &ClaimRec> {
        self.index()
            .by_expiry
            .iter()
            .take_while(move |(exp, _)| *exp > now)
            .filter_map(|(_, id)| self.claims.get(id))
    }

    fn live(&self, c: &ClaimRec, now: u64) -> bool {
        !self.released_before(c, None)
            && !self.index().renewed_by.contains_key(&c.id)
            && !c.expired(now)
    }

    /// Latest renewal in `id`'s chain from `id` on.
    fn head(&self, id: EventId) -> EventId {
        let mut cur = id;
        while let Some(n) = self.index().renewed_by.get(&cur) {
            cur = *n;
        }
        cur
    }

    /// Live, non-dead claims at `now` in priority order, each with the claim
    /// that beats it (if any).
    fn walk(&self, now: u64) -> Vec<(&ClaimRec, Option<EventId>)> {
        let ix = self.index();
        let mut live: Vec<(OrderKey, &ClaimRec)> = self
            .unexpired(now)
            .filter(|c| self.live(c, now) && !ix.dead.contains_key(&c.id))
            .map(|c| (self.root_rec(c).key(), c))
            .collect();
        live.sort_by_key(|(root, c)| (*root, c.key()));
        let root_ts = |c: &ClaimRec| self.root_rec(c).ts;
        let mut winners: Vec<&ClaimRec> = Vec::new();
        let mut out = Vec::with_capacity(live.len());
        for (i, &(_, c)) in live.iter().enumerate() {
            let c_ts = root_ts(c);
            let beaten = winners
                .iter()
                .find(|w| w.holder != c.holder && sets_overlap(&w.paths, &c.paths))
                .map(|w| w.id)
                // Late claim (GATE-2 N3): a later-sorted live claim that had
                // settled, by signed time, before this one was signed beats it.
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

    /// Record a claim. Idempotent.
    pub fn add_claim(&mut self, c: ClaimRec) {
        self.claims.insert(c.id, c);
        self.touched();
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
        self.touched();
    }

    /// Keep only the claims, and the releases, whose own event id passes
    /// `keep`. A dropped release removes that node's release of the claim.
    pub fn retain(&mut self, keep: impl Fn(&EventId) -> bool) {
        self.claims.retain(|id, _| keep(id));
        for by_node in self.releases.values_mut() {
            by_node.retain(|_, (_, _, id)| keep(id));
        }
        self.releases.retain(|_, by_node| !by_node.is_empty());
        self.touched();
    }

    /// Event ids of every claim and every recorded release.
    pub fn event_ids(&self) -> impl Iterator<Item = EventId> + '_ {
        let releases = self.releases.values().flat_map(|m| m.values().map(|k| k.2));
        self.claims.keys().copied().chain(releases)
    }

    /// Ids of the claims that win at `now` (pending or granted).
    pub fn winners(&self, now: u64) -> BTreeSet<EventId> {
        self.walk(now)
            .into_iter()
            .filter(|(_, beaten)| beaten.is_none())
            .map(|(c, _)| c.id)
            .collect()
    }

    /// The claim at the start of `id`'s renewal chain (`id` itself if it
    /// renews nothing). `None` if `id` is not a known claim.
    pub fn root_of(&self, id: &EventId) -> Option<EventId> {
        let c = self.claims.get(id)?;
        Some(self.root_rec(c).id)
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
        let ix = self.index();
        if let Some(by) = ix.renewed_by.get(id) {
            return Some(ClaimState::Renewed { by: *by });
        }
        if target.expired(now) {
            return Some(ClaimState::Expired);
        }
        if let Some(killer) = ix.dead.get(id) {
            return Some(ClaimState::Lost {
                winner: self.head(*killer),
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

    /// Claims by other holders whose paths overlap claim `id`'s, and that
    /// are not released, renewed or expired at `now` (dead ones included).
    pub fn conflicting(&self, id: &EventId, now: u64) -> Vec<EventId> {
        let Some(target) = self.claims.get(id) else {
            return Vec::new();
        };
        let mut out: Vec<EventId> = self
            .unexpired(now)
            .filter(|c| c.holder != target.holder && sets_overlap(&c.paths, &target.paths))
            .filter(|c| self.live(c, now))
            .map(|c| c.id)
            .collect();
        out.sort();
        out
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
