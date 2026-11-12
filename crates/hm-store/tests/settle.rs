//! Lease model (ADR-001 note 2, ADR-002 §1): settle, atomic sets, renewal,
//! release by claim id, TTL cap.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::limits::MAX_LEASE_TTL_SECS;
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use hm_store::settle::{
    ClaimState, SETTLE_MS, book_from_events, claim_state, claim_state_with, paths_overlap,
};
use proptest::prelude::*;

const T0: u64 = 1_000_000;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn room() -> RoomId {
    RoomId::from_bytes([7; 32])
}

fn claim(k: u8, lamport: u64, ts: u64, paths: &[&str], ttl_secs: u32) -> Event {
    claim_as(k, &format!("agent{k}"), lamport, ts, paths, ttl_secs)
}

fn claim_as(k: u8, name: &str, lamport: u64, ts: u64, paths: &[&str], ttl_secs: u32) -> Event {
    Event::sign(
        &key(k),
        room(),
        name,
        lamport,
        Lamport(lamport),
        ts,
        EventBody::LeaseClaim {
            paths: paths.iter().map(|s| s.to_string()).collect(),
            ttl_secs,
        },
    )
    .unwrap()
}

fn release(k: u8, lamport: u64, ts: u64, claim: &Event) -> Event {
    Event::sign(
        &key(k),
        room(),
        format!("agent{k}"),
        lamport,
        Lamport(lamport),
        ts,
        EventBody::LeaseRelease { claim: claim.id },
    )
    .unwrap()
}

const SETTLED: u64 = T0 + SETTLE_MS;

// ---- overlap ----

#[test]
fn glob_overlaps_file_under_it() {
    assert!(paths_overlap("src/net/**", "src/net/a.rs"));
    assert!(paths_overlap("src/net/a.rs", "src/net/**"));
}

#[test]
fn disjoint_paths_do_not_overlap() {
    assert!(!paths_overlap("src/net/**", "src/store/a.rs"));
    assert!(!paths_overlap("src/a.rs", "src/b.rs"));
    assert!(
        !paths_overlap("src/*.rs", "src/net/a.rs"),
        "* must not cross /"
    );
}

#[test]
fn identical_literals_overlap() {
    assert!(paths_overlap("Cargo.toml", "Cargo.toml"));
}

#[test]
fn two_globs_with_compatible_prefixes_overlap() {
    assert!(paths_overlap("src/**", "src/net/*.rs"));
    assert!(paths_overlap("src/*/a.rs", "src/net/*"));
    assert!(!paths_overlap("docs/**", "src/**"));
}

#[test]
fn non_overlapping_claims_are_both_granted() {
    let a = claim(1, 1, T0, &["src/net/**"], 600);
    let b = claim(2, 1, T0, &["src/store/**"], 600);
    let evs = vec![a.clone(), b.clone()];
    assert_eq!(claim_state(&a.id, &evs, SETTLED), Some(ClaimState::Granted));
    assert_eq!(claim_state(&b.id, &evs, SETTLED), Some(ClaimState::Granted));
}

// ---- tie-break ----

#[test]
fn lower_lamport_wins() {
    let a = claim(1, 5, T0, &["src/net/**"], 600);
    let b = claim(2, 3, T0, &["src/net/a.rs"], 600);
    let evs = vec![a.clone(), b.clone()];
    assert_eq!(claim_state(&b.id, &evs, SETTLED), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED),
        Some(ClaimState::Lost { winner: b.id })
    );
}

#[test]
fn equal_lamport_lower_author_node_wins() {
    let a = claim(1, 4, T0, &["src/net/**"], 600);
    let b = claim(2, 4, T0, &["src/net/**"], 600);
    let (lo, hi) = if a.author.node < b.author.node {
        (a, b)
    } else {
        (b, a)
    };
    let evs = vec![hi.clone(), lo.clone()];
    assert_eq!(
        claim_state(&lo.id, &evs, SETTLED),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        claim_state(&hi.id, &evs, SETTLED),
        Some(ClaimState::Lost { winner: lo.id })
    );
}

#[test]
fn equal_lamport_and_node_lower_id_wins() {
    let a = claim_as(1, "x", 4, T0, &["src/net/**"], 600);
    let b = claim_as(1, "y", 4, T0 + 1, &["src/net/a.rs"], 600);
    let (lo, hi) = if a.id < b.id { (a, b) } else { (b, a) };
    let evs = vec![hi.clone(), lo.clone()];
    assert_eq!(
        claim_state(&hi.id, &evs, SETTLED + 1),
        Some(ClaimState::Lost { winner: lo.id })
    );
}

#[test]
fn a_loser_does_not_block_a_third_claim() {
    // a beats b on src/net/**; b also covers docs/**; c wants docs/x.md.
    let a = claim(1, 1, T0, &["src/net/**"], 600);
    let b = claim(2, 2, T0, &["src/net/a.rs", "docs/**"], 600);
    let c = claim(3, 3, T0, &["docs/x.md"], 600);
    let evs = vec![a.clone(), b.clone(), c.clone()];
    assert_eq!(
        claim_state(&b.id, &evs, SETTLED),
        Some(ClaimState::Lost { winner: a.id })
    );
    assert_eq!(claim_state(&c.id, &evs, SETTLED), Some(ClaimState::Granted));
}

// ---- settle window ----

#[test]
fn pending_until_settle_window_passes() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let evs = vec![a.clone()];
    assert_eq!(claim_state(&a.id, &evs, T0), Some(ClaimState::Pending));
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED - 1),
        Some(ClaimState::Pending)
    );
    assert_eq!(claim_state(&a.id, &evs, SETTLED), Some(ClaimState::Granted));
}

#[test]
fn custom_settle_window() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let evs = vec![a.clone()];
    assert_eq!(
        claim_state_with(&a.id, &evs, T0 + 499, 500),
        Some(ClaimState::Pending)
    );
    assert_eq!(
        claim_state_with(&a.id, &evs, T0 + 500, 500),
        Some(ClaimState::Granted)
    );
}

#[test]
fn loser_is_lost_even_inside_window() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let b = claim(2, 2, T0, &["src/a.rs"], 600);
    let evs = vec![a.clone(), b.clone()];
    assert_eq!(
        claim_state(&b.id, &evs, T0),
        Some(ClaimState::Lost { winner: a.id })
    );
}

// ---- expiry ----

#[test]
fn claim_expires_after_ttl() {
    let a = claim(1, 1, T0, &["src/**"], 60);
    let evs = vec![a.clone()];
    assert_eq!(
        claim_state(&a.id, &evs, T0 + 59_999),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        claim_state(&a.id, &evs, T0 + 60_000),
        Some(ClaimState::Expired)
    );
}

#[test]
fn expired_winner_no_longer_blocks() {
    let a = claim(1, 1, T0, &["src/**"], 10);
    let b = claim(2, 2, T0 + 5_000, &["src/a.rs"], 600);
    let evs = vec![a.clone(), b.clone()];
    assert_eq!(
        claim_state(&b.id, &evs, T0 + 9_000),
        Some(ClaimState::Lost { winner: a.id })
    );
    assert_eq!(
        claim_state(&b.id, &evs, T0 + 10_000),
        Some(ClaimState::Granted)
    );
}

// ---- release ----

#[test]
fn author_release_releases() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let r = release(1, 2, T0 + 10, &a);
    let evs = vec![a.clone(), r];
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED),
        Some(ClaimState::Released)
    );
}

#[test]
fn foreign_release_is_ignored() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let r = release(2, 2, T0 + 10, &a);
    let evs = vec![a.clone(), r];
    assert_eq!(claim_state(&a.id, &evs, SETTLED), Some(ClaimState::Granted));
}

#[test]
fn release_targets_only_the_named_claim() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let b = claim(1, 2, T0 + 10, &["docs/**"], 600);
    let r = release(1, 3, T0 + 20, &a);
    let evs = vec![a.clone(), b.clone(), r];
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED),
        Some(ClaimState::Released)
    );
    assert_eq!(
        claim_state(&b.id, &evs, SETTLED + 10),
        Some(ClaimState::Granted)
    );
}

#[test]
fn released_winner_unblocks_loser() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let b = claim(2, 2, T0, &["src/a.rs"], 600);
    let r = release(1, 3, T0 + 100, &a);
    let evs = vec![a, b.clone(), r];
    assert_eq!(claim_state(&b.id, &evs, SETTLED), Some(ClaimState::Granted));
}

#[test]
fn unknown_or_non_claim_id_is_none() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let r = release(1, 2, T0, &a);
    let evs = vec![a, r.clone()];
    assert_eq!(claim_state(&EventId::from_bytes([0; 32]), &evs, T0), None);
    assert_eq!(claim_state(&r.id, &evs, T0), None);
}

// ---- order independence ----

fn scenario() -> Vec<Event> {
    let a = claim(1, 1, T0, &["src/net/**"], 600);
    let b = claim(2, 1, T0, &["src/net/a.rs", "docs/**"], 600);
    vec![
        claim(3, 2, T0 + 500, &["docs/x.md"], 600),
        claim(4, 3, T0 + 900, &["Cargo.toml"], 1),
        claim(1, 4, T0 + 1_500, &["crates/**"], 600),
        claim(1, 5, T0 + 1_600, &["src/net/**", "crates/**"], 600), // renewal
        release(2, 5, T0 + 1_800, &b),
        release(3, 6, T0 + 1_900, &a), // foreign
        a,
        b,
    ]
}

// ---- TTL cap ----

#[test]
fn ttl_is_capped_at_8h() {
    let a = claim(1, 1, T0, &["src/**"], u32::MAX);
    let evs = vec![a.clone()];
    let cap = u64::from(MAX_LEASE_TTL_SECS) * 1000;
    assert_eq!(
        claim_state(&a.id, &evs, T0 + cap - 1),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        claim_state(&a.id, &evs, T0 + cap),
        Some(ClaimState::Expired)
    );
    let active = book_from_events(&evs).active(T0);
    assert_eq!(active["src/**"].expires_at_ms, T0 + cap);
}

// ---- atomic sets ----

#[test]
fn claim_sets_are_atomic() {
    // b loses src/x.rs to a, so it gets docs/y.md neither; c then wins it.
    let a = claim(1, 1, T0, &["src/x.rs"], 600);
    let b = claim(2, 2, T0, &["src/x.rs", "docs/y.md"], 600);
    let c = claim(3, 3, T0, &["docs/y.md"], 600);
    let evs = vec![a.clone(), b.clone(), c.clone()];
    let active = book_from_events(&evs).active(SETTLED);
    assert_eq!(active.len(), 2);
    assert_eq!(active["src/x.rs"].claim, a.id);
    assert_eq!(active["docs/y.md"].claim, c.id);
    assert!(active.values().all(|l| l.claim != b.id));
}

// ---- renewal ----

#[test]
fn renewal_keeps_place_in_line_and_extends_ttl() {
    let a1 = claim(1, 1, T0, &["src/**"], 60);
    let b = claim(2, 2, T0 + 10, &["src/a.rs"], 600);
    // a re-claims a superset before a1 expires, after b's claim.
    let a2 = claim(1, 3, T0 + 50_000, &["src/**", "docs/**"], 60);
    let evs = vec![a1.clone(), b.clone(), a2.clone()];
    assert_eq!(
        claim_state(&a1.id, &evs, T0 + 55_000),
        Some(ClaimState::Renewed { by: a2.id })
    );
    // Past a1's own expiry, a2 still holds and b still loses.
    let now = T0 + 70_000;
    assert_eq!(claim_state(&a2.id, &evs, now), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&b.id, &evs, now),
        Some(ClaimState::Lost { winner: a2.id })
    );
    // A renewal is settled from the original claim's time.
    assert_eq!(
        claim_state(&a2.id, &evs, T0 + 50_000),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        book_from_events(&evs).active(now)["src/**"].expires_at_ms,
        T0 + 110_000
    );
}

#[test]
fn reclaim_after_expiry_is_a_new_claim() {
    let a1 = claim(1, 1, T0, &["src/**"], 10);
    let b = claim(2, 2, T0 + 15_000, &["src/a.rs"], 600);
    let a2 = claim(1, 3, T0 + 20_000, &["src/**"], 600);
    let evs = vec![a1.clone(), b.clone(), a2.clone()];
    let now = T0 + 30_000;
    assert_eq!(claim_state(&a1.id, &evs, now), Some(ClaimState::Expired));
    assert_eq!(claim_state(&b.id, &evs, now), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&a2.id, &evs, now),
        Some(ClaimState::Lost { winner: b.id })
    );
}

#[test]
fn subset_reclaim_or_other_agent_is_not_a_renewal() {
    let a1 = claim_as(1, "x", 1, T0, &["src/**", "docs/**"], 600);
    let a2 = claim_as(1, "x", 2, T0 + 10, &["src/**"], 600);
    let other = claim_as(1, "y", 3, T0 + 20, &["src/**", "docs/**"], 600);
    let evs = vec![a1.clone(), a2.clone(), other.clone()];
    // Same holder never blocks itself; a2 is a separate claim.
    assert_eq!(
        claim_state(&a1.id, &evs, SETTLED),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        claim_state(&a2.id, &evs, SETTLED + 10),
        Some(ClaimState::Granted)
    );
    // Another agent on the same node cannot renew x's claim.
    assert_eq!(
        claim_state(&other.id, &evs, SETTLED + 20),
        Some(ClaimState::Lost { winner: a1.id })
    );
}

#[test]
fn released_claim_cannot_be_renewed() {
    let a1 = claim(1, 1, T0, &["src/**"], 600);
    let r = release(1, 2, T0 + 10, &a1);
    let b = claim(2, 3, T0 + 20, &["src/**"], 600);
    let a2 = claim(1, 4, T0 + 30, &["src/**"], 600);
    let evs = vec![a1.clone(), r, b.clone(), a2.clone()];
    let now = SETTLED + 100;
    assert_eq!(claim_state(&a1.id, &evs, now), Some(ClaimState::Released));
    assert_eq!(claim_state(&b.id, &evs, now), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&a2.id, &evs, now),
        Some(ClaimState::Lost { winner: b.id })
    );
}

proptest! {
    #[test]
    fn state_is_independent_of_event_order(
        perm in Just((0..8usize).collect::<Vec<_>>()).prop_shuffle(),
        now in T0..T0 + 5_000,
    ) {
        let base = scenario();
        let shuffled: Vec<Event> = perm.iter().map(|&i| base[i].clone()).collect();
        for ev in &base {
            prop_assert_eq!(
                claim_state(&ev.id, &base, now),
                claim_state(&ev.id, &shuffled, now)
            );
        }
    }
}

// ---- GATE-2 N3: late claim with a low Lamport can't take over ----

#[test]
fn late_low_lamport_claim_cannot_take_over_a_settled_lease() {
    // Victim's lease is settled at T0 + SETTLE_MS. A minute later another
    // node signs an overlapping claim with a tiny Lamport.
    let victim = claim(1, 10, T0, &["src/net/**"], 3600);
    let late = claim(2, 2, T0 + 60_000, &["src/net/a.rs"], 3600);
    let evs = vec![victim.clone(), late.clone()];
    let now = T0 + 120_000;
    assert_eq!(
        claim_state(&victim.id, &evs, now),
        Some(ClaimState::Granted)
    );
    assert_eq!(
        claim_state(&late.id, &evs, now),
        Some(ClaimState::Lost { winner: victim.id })
    );
    let book = book_from_events(&evs);
    let active: Vec<_> = book.active(now).into_values().map(|l| l.claim).collect();
    assert_eq!(active, vec![victim.id]);
}

#[test]
fn concurrent_low_lamport_claim_still_wins_inside_the_window() {
    // Signed within the settle window of the other claim: truly concurrent,
    // so the Lamport order decides as before.
    let a = claim(1, 10, T0, &["src/net/**"], 3600);
    let b = claim(2, 2, T0 + SETTLE_MS - 1, &["src/net/a.rs"], 3600);
    let evs = vec![a.clone(), b.clone()];
    let now = T0 + 60_000;
    assert_eq!(claim_state(&b.id, &evs, now), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&a.id, &evs, now),
        Some(ClaimState::Lost { winner: b.id })
    );
}
