//! When to re-sync with which peer (ADR-002 §3 "Repair").
//!
//! Sync runs on gossip `NeighborUp`, on `Lagged` (for every connected
//! neighbor), and every [`REPAIR_INTERVAL`] ± [`REPAIR_JITTER`] while a peer
//! stays connected. After a failure the next periodic attempt backs off
//! exponentially from the interval, capped at [`BACKOFF_CAP`]. At most one
//! sync per peer runs at a time.
//!
//! Pure state machine over caller-supplied `now` and jitter, so it is tested
//! without a network or a clock.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use hm_proto::NodeId;

/// Periodic re-sync interval while connected.
pub const REPAIR_INTERVAL: Duration = Duration::from_secs(60);
/// Maximum ± spread applied to every scheduled delay.
pub const REPAIR_JITTER: Duration = Duration::from_secs(10);
/// Upper bound for the backoff after repeated failures.
pub const BACKOFF_CAP: Duration = Duration::from_secs(600);

/// Delay before the next periodic sync after `failures` consecutive failed
/// syncs. `jitter` in `[-1, 1]` scales [`REPAIR_JITTER`]. Never above
/// [`BACKOFF_CAP`] and never zero.
pub fn next_delay(failures: u32, jitter: f64) -> Duration {
    let base = REPAIR_INTERVAL
        .checked_mul(1u32.checked_shl(failures).unwrap_or(u32::MAX))
        .unwrap_or(BACKOFF_CAP)
        .min(BACKOFF_CAP);
    let spread = REPAIR_JITTER.mul_f64(jitter.clamp(-1.0, 1.0).abs());
    let d = if jitter < 0.0 {
        base.saturating_sub(spread)
    } else {
        base.saturating_add(spread)
    };
    d.clamp(Duration::from_secs(1), BACKOFF_CAP)
}

#[derive(Debug, Default, Clone)]
struct PeerRepair {
    connected: bool,
    in_flight: bool,
    failures: u32,
    due: Option<Instant>,
}

/// Per-peer repair schedule.
#[derive(Debug, Default, Clone)]
pub struct Repair {
    peers: BTreeMap<NodeId, PeerRepair>,
}

impl Repair {
    /// Mark a sync with `peer` started. False if one is already running.
    pub fn start(&mut self, peer: NodeId) -> bool {
        let p = self.peers.entry(peer).or_default();
        if p.in_flight {
            return false;
        }
        p.in_flight = true;
        p.due = None;
        true
    }

    /// Gossip `NeighborUp`: connected now; sync at once if idle.
    pub fn up(&mut self, peer: NodeId) -> bool {
        self.peers.entry(peer).or_default().connected = true;
        self.start(peer)
    }

    /// Gossip `NeighborDown`: no periodic syncs until it comes back.
    pub fn down(&mut self, peer: NodeId) {
        if let Some(p) = self.peers.get_mut(&peer) {
            p.connected = false;
        }
    }

    /// Gossip `Lagged`: start a sync with every idle connected neighbor.
    pub fn lagged(&mut self) -> Vec<NodeId> {
        let idle: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && !p.in_flight)
            .map(|(n, _)| *n)
            .collect();
        for n in &idle {
            self.start(*n);
        }
        idle
    }

    /// A sync finished: reset or grow the backoff and schedule the next one.
    pub fn finish(&mut self, peer: NodeId, ok: bool, now: Instant, jitter: f64) {
        let p = self.peers.entry(peer).or_default();
        p.in_flight = false;
        p.failures = if ok { 0 } else { p.failures.saturating_add(1) };
        p.due = Some(now + next_delay(p.failures, jitter));
    }

    /// Consecutive failures with `peer`.
    pub fn failures(&self, peer: &NodeId) -> u32 {
        self.peers.get(peer).map_or(0, |p| p.failures)
    }

    /// Earliest due time among idle connected peers.
    pub fn next_due(&self) -> Option<Instant> {
        self.peers
            .values()
            .filter(|p| p.connected && !p.in_flight)
            .filter_map(|p| p.due)
            .min()
    }

    /// Start (and return) every idle connected peer due at `now`.
    pub fn take_due(&mut self, now: Instant) -> Vec<NodeId> {
        let due: Vec<NodeId> = self
            .peers
            .iter()
            .filter(|(_, p)| p.connected && !p.in_flight && p.due.is_some_and(|d| d <= now))
            .map(|(n, _)| *n)
            .collect();
        for n in &due {
            self.start(*n);
        }
        due
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    #[test]
    fn delay_is_interval_with_jitter_then_backs_off_to_cap() {
        assert_eq!(next_delay(0, 0.0), Duration::from_secs(60));
        assert_eq!(next_delay(0, 1.0), Duration::from_secs(70));
        assert_eq!(next_delay(0, -1.0), Duration::from_secs(50));
        assert_eq!(next_delay(1, 0.0), Duration::from_secs(120));
        assert_eq!(next_delay(2, 0.0), Duration::from_secs(240));
        assert_eq!(next_delay(3, 0.0), Duration::from_secs(480));
        assert_eq!(next_delay(4, 0.0), BACKOFF_CAP);
        assert_eq!(next_delay(4, 1.0), BACKOFF_CAP);
        assert_eq!(next_delay(u32::MAX, 0.0), BACKOFF_CAP);
        // Out-of-range jitter is clamped.
        assert_eq!(next_delay(0, 5.0), Duration::from_secs(70));
    }

    #[test]
    fn neighbor_up_syncs_once_at_a_time() {
        let mut r = Repair::default();
        assert!(r.up(n(1)));
        assert!(!r.up(n(1)), "second sync while one runs");
        assert!(!r.start(n(1)));
        r.finish(n(1), true, Instant::now(), 0.0);
        assert!(r.up(n(1)));
    }

    #[test]
    fn periodic_only_while_connected_and_due() {
        let t0 = Instant::now();
        let mut r = Repair::default();
        r.up(n(1));
        r.finish(n(1), true, t0, 0.0);
        assert_eq!(r.next_due(), Some(t0 + REPAIR_INTERVAL));
        assert!(r.take_due(t0 + Duration::from_secs(59)).is_empty());
        assert_eq!(r.take_due(t0 + REPAIR_INTERVAL), vec![n(1)]);
        assert_eq!(r.next_due(), None, "in flight: nothing due");
        r.finish(n(1), true, t0, 0.0);
        r.down(n(1));
        assert_eq!(r.next_due(), None);
        assert!(r.take_due(t0 + BACKOFF_CAP).is_empty());
    }

    #[test]
    fn failures_back_off_and_success_resets() {
        let t0 = Instant::now();
        let mut r = Repair::default();
        r.up(n(1));
        r.finish(n(1), false, t0, 0.0);
        assert_eq!(r.failures(&n(1)), 1);
        assert_eq!(r.next_due(), Some(t0 + Duration::from_secs(120)));
        for _ in 0..10 {
            r.start(n(1));
            r.finish(n(1), false, t0, 0.0);
        }
        assert_eq!(r.next_due(), Some(t0 + BACKOFF_CAP));
        r.start(n(1));
        r.finish(n(1), true, t0, 0.0);
        assert_eq!(r.failures(&n(1)), 0);
        assert_eq!(r.next_due(), Some(t0 + REPAIR_INTERVAL));
    }

    #[test]
    fn lagged_syncs_every_idle_connected_neighbor() {
        let mut r = Repair::default();
        r.up(n(1));
        r.up(n(2));
        r.up(n(3));
        r.finish(n(1), true, Instant::now(), 0.0);
        r.finish(n(2), true, Instant::now(), 0.0);
        r.down(n(2));
        // 1: idle+connected; 2: down; 3: still in flight.
        assert_eq!(r.lagged(), vec![n(1)]);
        assert!(r.lagged().is_empty());
    }

    #[test]
    fn bootstrap_sync_before_neighbor_up_is_not_periodic() {
        let t0 = Instant::now();
        let mut r = Repair::default();
        assert!(r.start(n(1)));
        r.finish(n(1), true, t0, 0.0);
        assert_eq!(r.next_due(), None, "not connected yet");
        assert!(r.up(n(1)));
    }
}
