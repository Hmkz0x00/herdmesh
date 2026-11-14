//! ADR-002 Addendum 4: admission judges a batch's combined effect (GATE-5
//! N3) and lease work is bounded by a horizon, not history (GATE-5 N12).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use hm_store::settle::ClaimState;
use hm_store::{ClaimRec, LeaseBook, Store};

const T: u64 = 1_700_000_000_000;
const HOUR: u32 = 3600;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn room() -> RoomId {
    RoomId::from_bytes([8; 32])
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

/// A seq-`seq` Note from node `k` whose id sorts below `below`.
fn lower_twin(k: u8, seq: u64, below: &Event) -> Event {
    (0u32..)
        .map(|i| {
            sign(
                k,
                seq,
                1,
                T,
                EventBody::Note {
                    scope: "s".into(),
                    text: format!("n{i}"),
                },
            )
        })
        .find(|n| n.id < below.id)
        .unwrap()
}

fn holder(s: &Store, path: &str, now: u64) -> Option<EventId> {
    s.views()
        .active_leases(now)
        .find(|(p, _)| p == path)
        .map(|(_, l)| l.claim)
}

/// GATE-5: W1=[a] L1 and W2=[c] L2 (two attacker nodes), victim V=[b] L10,
/// all ts=T, 1h TTL, arrive at T and settle. At T+60s A=[a,b,c] L4/ts=T
/// arrives and is dead (killed by W1).
fn gate5() -> (Store, Event, Event, Event, Event) {
    let mut s = Store::open_in_memory(room()).unwrap();
    let w1 = claim(1, 1, 1, T, &["a"], HOUR);
    let w2 = claim(2, 1, 2, T, &["c"], HOUR);
    let v = claim(3, 1, 10, T, &["b"], HOUR);
    s.append_batch_at(&[w1.clone(), w2.clone(), v.clone()], T)
        .unwrap();
    let a = claim(4, 1, 4, T, &["a", "b", "c"], HOUR);
    s.append_at(&a, T + 60_000).unwrap();
    assert!(matches!(
        s.claim_state(&a.id, T + 60_000).unwrap(),
        Some(ClaimState::Lost { .. })
    ));
    assert!(s.claim_safe(&v.id, T + 62_000).unwrap());
    (s, w1, w2, v, a)
}

#[test]
fn gate5_two_releases_in_one_batch_cannot_revive_a_loser() {
    let (mut s, w1, w2, v, _a) = gate5();
    let r1 = release(1, 2, 3, T + 63_000, &w1);
    let r2 = release(2, 2, 3, T + 63_000, &w2);
    let rep = s.append_batch_at(&[r1, r2], T + 63_000).unwrap();
    assert!(rep.rejected >= 1, "{rep:?}");
    let now = T + 70_000;
    assert_eq!(holder(&s, "b", now), Some(v.id));
    assert!(s.claim_safe(&v.id, now).unwrap());
}

#[test]
fn gate5_two_releases_in_separate_calls_cannot_revive_a_loser() {
    let (mut s, w1, w2, v, _a) = gate5();
    let _ = s.append_at(&release(1, 2, 3, T + 63_000, &w1), T + 63_000);
    let _ = s.append_at(&release(2, 2, 3, T + 63_000, &w2), T + 63_000);
    assert_eq!(holder(&s, "b", T + 70_000), Some(v.id));
}

#[test]
fn gate5_two_twin_swaps_in_one_batch_cannot_revive_a_loser() {
    let (mut s, w1, w2, v, a) = gate5();
    // Release both blockers with keys above A: A stays dead.
    s.append_batch_at(
        &[
            release(1, 2, 20, T + 61_000, &w1),
            release(2, 2, 20, T + 61_000, &w2),
        ],
        T + 61_000,
    )
    .unwrap();
    assert!(matches!(
        s.claim_state(&a.id, T + 62_000).unwrap(),
        Some(ClaimState::Lost { .. })
    ));
    // Then swap both historical killers for lower-id Note twins.
    let rep = s
        .append_batch_at(&[lower_twin(1, 1, &w1), lower_twin(2, 1, &w2)], T + 63_000)
        .unwrap();
    assert!(rep.rejected >= 1, "{rep:?}");
    assert_eq!(holder(&s, "b", T + 70_000), Some(v.id));
}

#[test]
fn batch_admission_does_not_depend_on_batch_order() {
    let run = |rev: bool| {
        let (mut s, w1, w2, _v, _a) = gate5();
        let mut evs = vec![
            release(1, 2, 3, T + 63_000, &w1),
            release(2, 2, 3, T + 63_000, &w2),
        ];
        if rev {
            evs.reverse();
        }
        s.append_batch_at(&evs, T + 63_000).unwrap();
        s.events_since(Lamport(0)).unwrap()
    };
    assert_eq!(run(false), run(true));
}

#[test]
fn harmless_release_in_a_batch_is_still_stored() {
    let (mut s, w1, _w2, v, _a) = gate5();
    // Releasing W1 alone (key above A) frees a, revives nobody.
    let r = release(1, 2, 20, T + 63_000, &w1);
    let rep = s.append_batch_at(&[r], T + 63_000).unwrap();
    assert_eq!((rep.inserted, rep.rejected), (1, 0));
    assert_eq!(holder(&s, "b", T + 70_000), Some(v.id));
}

fn rec(id: u8, n: u64, node: u8, ts: u64, path: String, ttl_secs: u32) -> ClaimRec {
    let e = claim(node, n, n, ts, &[path.as_str()], ttl_secs);
    let _ = id;
    ClaimRec::from_event(&e).unwrap()
}

/// GATE-5 N12: dense, backdated, disjoint, expired history; every query
/// follows a mutation.
#[test]
fn every_query_after_a_mutation_stays_cheap_over_10k_dense_history() {
    let mut book = LeaseBook::default();
    for i in 0..10_000u64 {
        // All signed within ten seconds, long expired.
        let node = (i % 16) as u8 + 1;
        book.add_claim(rec(0, i + 1, node, T + i, format!("old/{i}.rs"), 60));
    }
    let now = T + 30 * 3_600_000;
    let live = rec(0, 20_000, 17, now - 1_000, "live.rs".into(), HOUR);
    let live_id = live.id;
    book.add_claim(live);
    let orphan = EventId::from_bytes([9; 32]);
    let start = Instant::now();
    for i in 0..256u64 {
        match i % 3 {
            // An inert release (unknown claim).
            0 => book.add_release(
                hm_proto::NodeId::from_bytes([1; 32]),
                (Lamport(1), hm_proto::NodeId::from_bytes([1; 32]), orphan),
                orphan,
            ),
            // Another long-expired claim.
            1 => book.add_claim(rec(
                0,
                30_000 + i,
                18,
                T + 20_000 + i,
                format!("old2/{i}.rs"),
                60,
            )),
            // A new live claim: the index must be rebuilt.
            _ => book.add_claim(rec(
                0,
                40_000 + i,
                19,
                now - 500,
                format!("live/{i}.rs"),
                HOUR,
            )),
        }
        assert!(book.state(&live_id, now).is_some());
    }
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "256 mutate+query rounds took {took:?}"
    );
}
