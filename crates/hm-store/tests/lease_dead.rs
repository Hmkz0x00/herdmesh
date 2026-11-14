//! ADR-002 Addendum 3: losers stay dead (GATE-4 N3) and lease resolution
//! cost is bounded (GATE-4 N12).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use hm_store::settle::{ClaimState, SETTLE_MS, book_from_events, claim_state};
use hm_store::{Store, StoreError};

const T: u64 = 1_700_000_000_000;
const HOUR: u32 = 3600;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn room() -> RoomId {
    RoomId::from_bytes([6; 32])
}

fn sign(k: u8, seq: u64, lamport: u64, ts: u64, body: EventBody) -> Event {
    Event::sign(&key(k), room(), "ag", seq, Lamport(lamport), ts, body).unwrap()
}

fn claim(k: u8, seq: u64, lamport: u64, ts: u64, paths: &[&str], ttl_secs: u32) -> Event {
    sign(
        k,
        seq,
        lamport,
        ts,
        EventBody::LeaseClaim {
            paths: paths.iter().map(|p| p.to_string()).collect(),
            ttl_secs,
        },
    )
}

fn release(k: u8, seq: u64, lamport: u64, ts: u64, of: &Event) -> Event {
    sign(
        k,
        seq,
        lamport,
        ts,
        EventBody::LeaseRelease { claim: of.id },
    )
}

fn store() -> Store {
    Store::open_in_memory(room()).unwrap()
}

fn holder(s: &Store, path: &str, now: u64) -> Option<EventId> {
    s.views()
        .active_leases(now)
        .find(|(p, _)| p == path)
        .map(|(_, l)| l.claim)
}

/// GATE-4 setup: X's W=[a] L1/ts=T and victim V=[b] L10/ts=T arrive at T;
/// V is safe at T+3s; at T+60s a second attacker identity's A=[a,b]
/// L2/ts=T arrives and loses to W. All TTLs one hour.
fn gate4() -> (Store, Event, Event, Event) {
    let mut s = store();
    let w = claim(1, 1, 1, T, &["a"], HOUR);
    let v = claim(2, 1, 10, T, &["b"], HOUR);
    s.append_batch_at(&[w.clone(), v.clone()], T).unwrap();
    assert!(s.claim_safe(&v.id, T + 3_000).unwrap());
    let a = claim(3, 1, 2, T, &["a", "b"], HOUR);
    s.append_at(&a, T + 60_000).unwrap();
    assert_eq!(
        s.claim_state(&a.id, T + 60_000).unwrap(),
        Some(ClaimState::Lost { winner: w.id })
    );
    (s, w, v, a)
}

#[test]
fn gate4_release_of_the_blocker_does_not_promote_a_dead_loser() {
    let (mut s, w, v, a) = gate4();
    // X releases W at T+63s with an honest (current) Lamport.
    let r = release(1, 2, 11, T + 63_000, &w);
    s.append_at(&r, T + 63_000).unwrap();
    let now = T + 70_000;
    assert_eq!(
        s.claim_state(&v.id, now).unwrap(),
        Some(ClaimState::Granted)
    );
    assert!(s.claim_safe(&v.id, now).unwrap());
    assert_eq!(holder(&s, "b", now), Some(v.id));
    assert!(matches!(
        s.claim_state(&a.id, now).unwrap(),
        Some(ClaimState::Lost { .. })
    ));
}

#[test]
fn gate4_expiry_of_the_blocker_does_not_promote_a_dead_loser() {
    // W expires before A and V do: A must still not take b.
    let mut s = store();
    let w = claim(1, 1, 1, T, &["a"], 120);
    let v = claim(2, 1, 10, T, &["b"], HOUR);
    s.append_batch_at(&[w.clone(), v.clone()], T).unwrap();
    let a = claim(3, 1, 2, T, &["a", "b"], HOUR);
    s.append_at(&a, T + 60_000).unwrap();
    let now = T + 200_000;
    assert_eq!(holder(&s, "b", now), Some(v.id));
    assert_eq!(holder(&s, "a", now), None);
}

#[test]
fn backdated_release_cannot_unkill_a_loser() {
    // A release of W ordered below A would make W "released before A".
    let (mut s, w, v, _a) = gate4();
    let r = release(1, 2, 1, T + 63_000, &w);
    assert!(matches!(
        s.append_at(&r, T + 63_000),
        Err(StoreError::LeaseTakeover)
    ));
    assert_eq!(holder(&s, "b", T + 70_000), Some(v.id));
}

#[test]
fn twin_swap_that_deletes_the_killer_is_refused() {
    let (mut s, w, v, _a) = gate4();
    // A lower-id seq-1 twin of W from X's node would replace (delete) W.
    let twin = (0u32..)
        .map(|i| {
            sign(
                1,
                1,
                1,
                T,
                EventBody::Note {
                    scope: "s".into(),
                    text: format!("n{i}"),
                },
            )
        })
        .find(|n| n.id < w.id)
        .unwrap();
    assert!(matches!(
        s.append_at(&twin, T + 63_000),
        Err(StoreError::LeaseTakeover)
    ));
    assert_eq!(holder(&s, "b", T + 70_000), Some(v.id));
}

#[test]
fn freed_path_goes_to_a_claim_issued_after_the_release() {
    let a = claim(1, 1, 1, T, &["src/**"], HOUR);
    let b1 = claim(2, 1, 2, T, &["src/a.rs"], HOUR);
    let r = release(1, 2, 3, T + 100, &a);
    let b2 = claim(2, 2, 4, T + 200, &["src/a.rs"], HOUR);
    let evs = vec![a.clone(), b1.clone(), r, b2.clone()];
    let now = T + 200 + SETTLE_MS;
    // The queued loser stays dead; the re-issued claim wins.
    assert_eq!(
        claim_state(&b1.id, &evs, now),
        Some(ClaimState::Lost { winner: a.id })
    );
    assert_eq!(claim_state(&b2.id, &evs, now), Some(ClaimState::Granted));
}

#[test]
fn freed_path_after_expiry_goes_to_a_claim_issued_after_it() {
    let a = claim(1, 1, 1, T, &["src/**"], 10);
    let b1 = claim(2, 1, 2, T + 5_000, &["src/a.rs"], HOUR);
    let b2 = claim(2, 2, 3, T + 10_000, &["src/a.rs"], HOUR);
    let evs = vec![a.clone(), b1.clone(), b2.clone()];
    let now = T + 20_000;
    assert_eq!(
        claim_state(&b1.id, &evs, now),
        Some(ClaimState::Lost { winner: a.id })
    );
    assert_eq!(claim_state(&b2.id, &evs, now), Some(ClaimState::Granted));
}

/// GATE-4 N12: 10k historical claims, 256 state queries.
#[test]
fn claim_state_cost_is_bounded_with_long_history() {
    let mut evs = Vec::with_capacity(10_016);
    for i in 0..10_000u64 {
        let k = (i % 16) as u8 + 1;
        let p = format!("old/{}/f{}.rs", i % 64, i % 7);
        // One claim per minute, all long expired by `now`.
        evs.push(claim(k, i + 1, i + 1, T + i * 60_000, &[p.as_str()], 600));
    }
    let base = T + 10_000 * 60_000 + 10 * 3_600_000;
    for i in 0..16u64 {
        let p = format!("live/{i}.rs");
        evs.push(claim(17, i + 1, 20_000 + i, base + i, &[p.as_str()], HOUR));
    }
    let book = book_from_events(&evs);
    let now = base + 60_000;
    let ids: Vec<EventId> = evs.iter().rev().take(256).map(|e| e.id).collect();
    let start = Instant::now();
    for id in &ids {
        assert!(book.state(id, now).is_some());
    }
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "256 claim_state calls took {took:?}"
    );
    assert_eq!(book.active(now).len(), 16);
}
