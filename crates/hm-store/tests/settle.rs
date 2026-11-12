//! Lease settle semantics (ADR-001 note 2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use hm_store::settle::{ClaimState, SETTLE_MS, claim_state, claim_state_with, paths_overlap};
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
    Event::sign(
        &key(k),
        room(),
        format!("agent{k}"),
        Lamport(lamport),
        ts,
        EventBody::LeaseClaim {
            paths: paths.iter().map(|s| s.to_string()).collect(),
            ttl_secs,
        },
    )
    .unwrap()
}

fn release(k: u8, lamport: u64, ts: u64, paths: &[&str]) -> Event {
    Event::sign(
        &key(k),
        room(),
        format!("agent{k}"),
        Lamport(lamport),
        ts,
        EventBody::LeaseRelease {
            paths: paths.iter().map(|s| s.to_string()).collect(),
        },
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
    let a = claim(1, 4, T0, &["src/net/**"], 600);
    let b = claim(1, 4, T0 + 1, &["src/net/a.rs"], 600);
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
    let r = release(1, 2, T0 + 10, &["src/**"]);
    let evs = vec![a.clone(), r];
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED),
        Some(ClaimState::Released)
    );
}

#[test]
fn foreign_release_is_ignored() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let r = release(2, 2, T0 + 10, &["src/**"]);
    let evs = vec![a.clone(), r];
    assert_eq!(claim_state(&a.id, &evs, SETTLED), Some(ClaimState::Granted));
}

#[test]
fn release_before_claim_is_ignored() {
    let r = release(1, 1, T0, &["src/**"]);
    let a = claim(1, 2, T0 + 10, &["src/**"], 600);
    let evs = vec![r, a.clone()];
    assert_eq!(
        claim_state(&a.id, &evs, SETTLED + 10),
        Some(ClaimState::Granted)
    );
}

#[test]
fn released_winner_unblocks_loser() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let b = claim(2, 2, T0, &["src/a.rs"], 600);
    let r = release(1, 3, T0 + 100, &["src/**"]);
    let evs = vec![a, b.clone(), r];
    assert_eq!(claim_state(&b.id, &evs, SETTLED), Some(ClaimState::Granted));
}

#[test]
fn unknown_or_non_claim_id_is_none() {
    let a = claim(1, 1, T0, &["src/**"], 600);
    let r = release(1, 2, T0, &["src/**"]);
    let evs = vec![a, r.clone()];
    assert_eq!(claim_state(&EventId::from_bytes([0; 32]), &evs, T0), None);
    assert_eq!(claim_state(&r.id, &evs, T0), None);
}

// ---- order independence ----

fn scenario() -> Vec<Event> {
    vec![
        claim(1, 1, T0, &["src/net/**"], 600),
        claim(2, 1, T0, &["src/net/a.rs", "docs/**"], 600),
        claim(3, 2, T0 + 500, &["docs/x.md"], 600),
        claim(4, 3, T0 + 900, &["Cargo.toml"], 1),
        claim(1, 4, T0 + 1_500, &["crates/**"], 600),
        release(2, 5, T0 + 1_800, &["docs/**"]),
        release(3, 6, T0 + 1_900, &["src/**"]), // foreign for every src claim
    ]
}

proptest! {
    #[test]
    fn state_is_independent_of_event_order(
        perm in Just((0..7usize).collect::<Vec<_>>()).prop_shuffle(),
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
