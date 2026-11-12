//! Protocol limits shared by every node (ADR-002). Changing one changes which
//! events a node stores or how views read them, so all nodes must agree.

/// An event whose lamport exceeds the local max by more than this is rejected.
pub const MAX_LAMPORT_JUMP: u64 = 1_000_000;

/// An event with `ts` further than this in the local future is not stored yet.
pub const MAX_FUTURE_SKEW_MS: u64 = 5 * 60 * 1000;

/// Lease TTLs are capped at 8 h when computing expiry.
pub const MAX_LEASE_TTL_SECS: u32 = 8 * 60 * 60;

/// Largest encoded event (`Event::encode`) any node accepts, stores or sends,
/// on gossip and sync alike.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_match_adr_002() {
        assert_eq!(MAX_LAMPORT_JUMP, 1_000_000);
        assert_eq!(MAX_FUTURE_SKEW_MS, 300_000);
        assert_eq!(MAX_LEASE_TTL_SECS, 28_800);
        assert_eq!(MAX_EVENT_BYTES, 64 * 1024);
    }
}
