//! Lease settle semantics (ADR-001 note 2). Pure functions, no I/O.
//!
//! # Rule
//! Given the room's `LeaseClaim` / `LeaseRelease` events and a wall-clock
//! `now` (unix millis), a claim is, in this order of precedence:
//!
//! 1. **Released**: a `LeaseRelease` exists from the *same author node*, with
//!    a higher Lamport than the claim, whose paths overlap the claim's.
//!    Releases from other nodes are ignored.
//! 2. **Expired**: `now >= claim.ts + ttl_secs * 1000`.
//! 3. Otherwise the claim is *live* and competes with every other live claim:
//!    live claims are sorted by `(lamport, author node, event id)` and walked
//!    lowest-first; a claim **wins** unless it overlaps a claim that already
//!    won. A claim that lost does not block anyone. So the result depends only
//!    on the set of events, never on the order they arrived.
//!    - not a winner → **Lost { winner }** (the lowest-keyed winner it overlaps)
//!    - winner and `now >= claim.ts + settle_ms` → **Granted**
//!    - winner inside the settle window → **Pending**
//!
//! "Granted" is relative to the events seen so far: a lower-keyed conflicting
//! claim that arrives late still flips it to Lost; the settle window
//! ([`SETTLE_MS`]) makes that unlikely in practice.
//!
//! # Path overlap
//! Paths are globs (`globset`, `*` does not cross `/`). Two patterns overlap
//! if either one, compiled as a glob, matches the other as a literal path.
//! When *both* contain glob metacharacters this is incomplete
//! (`src/*/a.rs` vs `src/net/*`), so we also treat them as overlapping if
//! their literal prefixes (text before the first metacharacter) are
//! prefix-compatible. That can over-report overlap between two globs, which
//! is the safe direction for a lease.

use hm_proto::{Event, EventBody, EventId};

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

struct Claim<'a> {
    ev: &'a Event,
    paths: &'a [String],
    ttl_secs: u32,
}

fn as_claim(ev: &Event) -> Option<Claim<'_>> {
    match &ev.body {
        EventBody::LeaseClaim { paths, ttl_secs } => Some(Claim {
            ev,
            paths,
            ttl_secs: *ttl_secs,
        }),
        _ => None,
    }
}

fn is_released(c: &Claim<'_>, events: &[Event]) -> bool {
    events.iter().any(|ev| match &ev.body {
        EventBody::LeaseRelease { paths } => {
            ev.room == c.ev.room
                && ev.author.node == c.ev.author.node
                && ev.lamport > c.ev.lamport
                && sets_overlap(paths, c.paths)
        }
        _ => false,
    })
}

fn is_expired(c: &Claim<'_>, now: u64) -> bool {
    now >= c
        .ev
        .ts
        .saturating_add(u64::from(c.ttl_secs).saturating_mul(1000))
}

/// [`claim_state_with`] using the default [`SETTLE_MS`].
pub fn claim_state(claim_id: &EventId, events: &[Event], now: u64) -> Option<ClaimState> {
    claim_state_with(claim_id, events, now, SETTLE_MS)
}

/// State of the `LeaseClaim` event `claim_id` at time `now` (unix millis).
///
/// Returns `None` if `claim_id` is not a `LeaseClaim` in `events`.
pub fn claim_state_with(
    claim_id: &EventId,
    events: &[Event],
    now: u64,
    settle_ms: u64,
) -> Option<ClaimState> {
    let target = events
        .iter()
        .filter(|e| e.id == *claim_id)
        .find_map(as_claim)?;
    if is_released(&target, events) {
        return Some(ClaimState::Released);
    }
    if is_expired(&target, now) {
        return Some(ClaimState::Expired);
    }

    let mut live: Vec<Claim<'_>> = events
        .iter()
        .filter(|e| e.room == target.ev.room)
        .filter_map(as_claim)
        .filter(|c| !is_released(c, events) && !is_expired(c, now))
        .collect();
    live.sort_by(|a, b| {
        (a.ev.lamport, a.ev.author.node, a.ev.id).cmp(&(b.ev.lamport, b.ev.author.node, b.ev.id))
    });
    live.dedup_by(|a, b| a.ev.id == b.ev.id);

    let mut winners: Vec<&Claim<'_>> = Vec::new();
    for c in &live {
        if let Some(w) = winners.iter().find(|w| sets_overlap(w.paths, c.paths)) {
            if c.ev.id == *claim_id {
                return Some(ClaimState::Lost { winner: w.ev.id });
            }
            continue;
        }
        if c.ev.id == *claim_id {
            let settled = now >= c.ev.ts.saturating_add(settle_ms);
            return Some(if settled {
                ClaimState::Granted
            } else {
                ClaimState::Pending
            });
        }
        winners.push(c);
    }
    // Unreachable: target is live and therefore in `live`.
    None
}
