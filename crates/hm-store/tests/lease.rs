//! ADR-005 §1: cooperative leases. One deterministic rule over the stored
//! signed set; no local admission.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, EventId, Lamport, NodeId, Role, RoomId};
use hm_store::settle::{ClaimState, SETTLE_MS, book_from_events, claim_state};
use hm_store::{AppendOutcome, Store};
use proptest::prelude::*;

const T: u64 = 1_700_000_000_000;
const HOUR: u32 = 3600;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn node(n: u8) -> NodeId {
    NodeId::from_bytes(key(n).verifying_key().to_bytes())
}

fn room() -> RoomId {
    RoomId::from_bytes([4; 32])
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

fn holder(s: &Store, path: &str, now: u64) -> Option<EventId> {
    s.views()
        .active_leases(now)
        .find(|(p, _)| p == path)
        .map(|(_, l)| l.claim)
}

#[test]
fn lowest_key_wins_even_if_signed_later() {
    // Cooperative: no late-claim or admission protection. L2 beats L10.
    let a = claim(1, 1, 10, T, &["src/**"], HOUR);
    let b = claim(2, 1, 2, T + 60_000, &["src/a.rs"], HOUR);
    let evs = vec![a.clone(), b.clone()];
    let now = T + 120_000;
    assert_eq!(claim_state(&b.id, &evs, now), Some(ClaimState::Granted));
    assert_eq!(
        claim_state(&a.id, &evs, now),
        Some(ClaimState::Lost { winner: b.id })
    );
}

#[test]
fn store_has_no_local_admission() {
    // A late low-Lamport claim is stored like any other event, and every
    // node with the same set agrees on the winner.
    let mut s = Store::open_in_memory(room()).unwrap();
    let a = claim(1, 1, 10, T, &["src/**"], HOUR);
    s.append_at(&a, T).unwrap();
    let b = claim(2, 1, 2, T, &["src/a.rs"], HOUR);
    assert_eq!(
        s.append_at(&b, T + 60_000).unwrap(),
        AppendOutcome::Inserted
    );
    assert_eq!(holder(&s, "src/a.rs", T + 70_000), Some(b.id));
}

#[test]
fn loser_stays_dead_after_the_winner_is_released() {
    let a = claim(1, 1, 1, T, &["src/**"], HOUR);
    let b1 = claim(2, 1, 2, T, &["src/a.rs"], HOUR);
    let r = release(1, 2, 3, T + 100, &a);
    let b2 = claim(2, 2, 4, T + 200, &["src/a.rs"], HOUR);
    let mut evs = vec![a.clone(), b1.clone(), r];
    let now = T + 200 + SETTLE_MS;
    assert_eq!(
        claim_state(&b1.id, &evs, now),
        Some(ClaimState::Lost { winner: a.id })
    );
    assert!(book_from_events(&evs).active(now).is_empty());
    // The holder claims again after the release and wins.
    evs.push(b2.clone());
    assert_eq!(claim_state(&b2.id, &evs, now), Some(ClaimState::Granted));
}

#[test]
fn loser_stays_dead_after_the_winner_expires() {
    let a = claim(1, 1, 1, T, &["src/**"], 10);
    let b = claim(2, 1, 2, T + 5_000, &["src/a.rs"], HOUR);
    let evs = vec![a.clone(), b.clone()];
    assert_eq!(
        claim_state(&b.id, &evs, T + 20_000),
        Some(ClaimState::Lost { winner: a.id })
    );
}

#[test]
fn removed_member_claims_are_ignored() {
    // ADR-005 §2: the owner demotes a member to observer; its live claim
    // stops counting on every node.
    let genesis = sign(1, 1, 1, T, EventBody::Genesis { owner: node(1) });
    let mut s = Store::open_in_memory(room()).unwrap();
    s.pin_genesis(genesis.id).unwrap();
    let bad = claim(2, 1, 2, T, &["src/**"], HOUR);
    let obs_claim = claim(3, 2, 5, T, &["docs/**"], HOUR);
    let demote = sign(
        1,
        2,
        3,
        T,
        EventBody::RoleGrant {
            node: node(2),
            role: Role::Observer,
        },
    );
    let demote3 = sign(
        1,
        3,
        4,
        T,
        EventBody::RoleGrant {
            node: node(3),
            role: Role::Observer,
        },
    );
    s.append_batch_at(&[genesis, bad.clone()], T).unwrap();
    assert_eq!(holder(&s, "src/**", T + 10_000), Some(bad.id));
    s.append_batch_at(&[demote, demote3, obs_claim.clone()], T)
        .unwrap();
    assert_eq!(s.views().active_leases(T + 10_000).count(), 0);
    assert_eq!(s.claim_state(&obs_claim.id, T + 10_000).unwrap(), None);
}

/// 10k long-expired claims: building and querying stay cheap, including a
/// mutation before every query.
#[test]
fn ten_thousand_expired_claims_are_cheap() {
    let mut evs = Vec::with_capacity(10_100);
    for i in 0..10_000u64 {
        let k = (i % 16) as u8 + 1;
        let p = format!("old/{}.rs", i % 64);
        // One a minute for a week, each long expired.
        evs.push(claim(
            k,
            i / 16 + 1,
            i + 1,
            T + i * 60_000,
            &[p.as_str()],
            60,
        ));
    }
    let start = Instant::now();
    let mut book = book_from_events(&evs);
    let now = T + 10_000 * 60_000 + 30 * 3_600_000;
    for i in 0..256u64 {
        let p = format!("live/{i}.rs");
        let c = claim(17, i + 1, 20_000 + i, now - 5_000, &[p.as_str()], HOUR);
        book.add_claim(hm_store::ClaimRec::from_event(&c).unwrap());
        assert_eq!(book.state(&c.id, now), Some(ClaimState::Granted));
        assert!(book.state(&evs[(i * 37) as usize].id, now).is_some());
    }
    let took = start.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}");
    assert_eq!(book.active(now).len(), 256);
}

// ---- order independence ----

const PATHS: [&str; 3] = ["a", "b", "d/**"];

/// (node, lamport, ts offset, paths, release-of-earlier index).
type Spec = (u8, u64, u64, Vec<&'static str>, Option<usize>);

fn events(specs: &[Spec]) -> Vec<Event> {
    let mut seq = [0u64; 5];
    let mut out: Vec<(u8, Event)> = Vec::new();
    for (k, l, ts, p, rel) in specs {
        let target = rel
            .and_then(|i| out.get(i))
            .filter(|(_, e)| matches!(e.body, EventBody::LeaseClaim { .. }));
        let e = match target {
            Some((by, t)) => {
                let by = *by;
                seq[by as usize] += 1;
                release(by, seq[by as usize], *l + 5, T + ts, t)
            }
            None => {
                seq[*k as usize] += 1;
                claim(*k, seq[*k as usize], *l, T + ts, p, 120)
            }
        };
        out.push((target.map_or(*k, |(b, _)| *b), e));
    }
    out.into_iter().map(|(_, e)| e).collect()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn leases_depend_only_on_the_event_set(
        specs in prop::collection::vec(
            (1u8..5, 1u64..12, 0u64..200_000,
             prop::collection::vec(prop::sample::select(PATHS.to_vec()), 1..3),
             prop::option::weighted(0.25, 0usize..10)),
            1..14),
        perm in any::<prop::sample::Index>(),
        now in 0u64..400_000,
    ) {
        let evs = events(&specs);
        let mut shuffled = evs.clone();
        let n = shuffled.len();
        shuffled.rotate_left(perm.index(n));
        shuffled.reverse();
        let (a, b) = (book_from_events(&evs), book_from_events(&shuffled));
        let now = T + now;
        prop_assert_eq!(a.active(now), b.active(now));
        for e in &evs {
            prop_assert_eq!(a.state(&e.id, now), b.state(&e.id, now));
        }
        // Store path: one-by-one in two orders, same stored set and leases.
        let run = |evs: &[Event]| {
            let mut s = Store::open_in_memory(room()).unwrap();
            for e in evs {
                let _ = s.append_at(e, T + 400_000);
            }
            (s.events_since(Lamport(0)).unwrap(), s.views().active_leases(now).collect::<Vec<_>>())
        };
        prop_assert_eq!(run(&evs), run(&shuffled));
    }
}

// ---- GATE-7 N21: expired history stays cheap ----

#[test]
fn dense_expired_same_holder_claims_are_cheap() {
    // N same-holder, disjoint claims, all ts=T, TTL 60s; queried a day later.
    let n = 10_000u64;
    let evs: Vec<Event> = (0..n)
        .map(|i| claim(1, i + 1, i + 1, T, &[format!("f{i}.rs").as_str()], 60))
        .collect();
    let start = Instant::now();
    let book = book_from_events(&evs);
    let day = T + 86_400_000;
    for e in &evs {
        assert_eq!(book.state(&e.id, day), Some(ClaimState::Expired));
    }
    assert!(book.active(day).is_empty());
    let took = start.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}");
}

#[test]
fn long_expired_renewal_chain_is_cheap() {
    // One holder renews the same path every 30s for 10k rounds.
    let n = 10_000u64;
    let evs: Vec<Event> = (0..n)
        .map(|i| claim(1, i + 1, i + 1, T + i * 30_000, &["src/**"], 60))
        .collect();
    let start = Instant::now();
    let book = book_from_events(&evs);
    let later = T + n * 30_000 + 86_400_000;
    for e in &evs {
        assert_eq!(book.state(&e.id, later), Some(ClaimState::Expired));
    }
    let took = start.elapsed();
    assert!(took < Duration::from_secs(3), "took {took:?}");
    // While live, the first claim reports the chain's latest renewal.
    let mid = T + 40_000;
    let small = book_from_events(&evs[..20]);
    assert_eq!(
        small.state(&evs[0].id, mid),
        Some(ClaimState::Renewed { by: evs[19].id })
    );
}

// ---- GATE-7 N22: a renewal's loser stays lost after the renewal ends ----

#[test]
fn renewal_loser_does_not_revive_after_release() {
    // Alice P=(L1, T, 2s); Bob B=(L2, T+3s, 10s); Alice R=(L3, T+1s, 10s)
    // renews P. At T+6s B has lost to R; Alice releases R. B stays lost.
    let p = claim(1, 1, 1, T, &["a"], 2);
    let b = claim(2, 1, 2, T + 3_000, &["a"], 10);
    let r = claim(1, 2, 3, T + 1_000, &["a"], 10);
    let rel = release(1, 3, 4, T + 6_000, &r);
    let now = T + 6_000;
    let before = vec![p.clone(), b.clone(), r.clone()];
    assert_eq!(
        claim_state(&b.id, &before, now),
        Some(ClaimState::Lost { winner: r.id })
    );
    let mut after = before.clone();
    after.push(rel);
    for evs in [after.clone(), after.iter().rev().cloned().collect()] {
        assert!(matches!(
            claim_state(&b.id, &evs, now),
            Some(ClaimState::Lost { .. })
        ));
        assert!(book_from_events(&evs).active(now).is_empty());
    }
}
