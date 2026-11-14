//! ADR-002 Addendum 2: lease admission. A claim that reaches this node after
//! a conflicting lease settled here can't take that lease (GATE-3 N3).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use hm_store::settle::SETTLE_MS;
use hm_store::{AppendOutcome, ClaimState, Store, StoreError};

const T: u64 = 1_700_000_000_000;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn room() -> RoomId {
    RoomId::from_bytes([5; 32])
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

#[test]
fn n3_backdated_low_lamport_fresh_claim_cannot_take_a_settled_lease() {
    // Exactly GATE-3 N3: victim L10/ts=T; a fresh attacker node signs
    // seq1/L2/ts=T and it arrives at T+60s.
    let mut s = store();
    let victim = claim(1, 1, 10, T, &["src/net/**"], 3600);
    s.append_at(&victim, T).unwrap();
    let attacker = claim(2, 1, 2, T, &["src/net/a.rs"], 3600);
    let at = T + 60_000;
    assert!(matches!(
        s.append_at(&attacker, at),
        Err(StoreError::LeaseTakeover)
    ));
    // Local first-seen no longer lets it through two seconds later.
    let later = at + SETTLE_MS;
    assert_eq!(s.claim_state(&attacker.id, later).unwrap(), None);
    assert_eq!(
        s.claim_state(&victim.id, later).unwrap(),
        Some(ClaimState::Granted)
    );
    assert!(s.claim_safe(&victim.id, later).unwrap());
    assert_eq!(holder(&s, "src/net/**", later), Some(victim.id));
}

#[test]
fn n3_takeover_is_refused_on_the_batch_path_too() {
    let mut s = store();
    let victim = claim(1, 1, 10, T, &["src/net/**"], 3600);
    s.append_at(&victim, T).unwrap();
    let attacker = claim(2, 1, 2, T, &["src/net/a.rs"], 3600);
    let r = s
        .append_batch_at(std::slice::from_ref(&attacker), T + 60_000)
        .unwrap();
    assert_eq!((r.inserted, r.rejected), (0, 1));
    assert_eq!(s.claim_state(&attacker.id, T + 61_000).unwrap(), None);
}

#[test]
fn conflicting_claim_inside_the_local_settle_window_still_competes() {
    // An honest race: the first claim isn't settled here yet, so the Lamport
    // order decides as before and every node converges.
    let mut s = store();
    let a = claim(1, 1, 10, T, &["src/net/**"], 3600);
    s.append_at(&a, T).unwrap();
    let b = claim(2, 1, 2, T, &["src/net/a.rs"], 3600);
    assert_eq!(
        s.append_at(&b, T + SETTLE_MS - 1).unwrap(),
        AppendOutcome::Inserted
    );
    assert_eq!(
        s.claim_state(&a.id, T + 10_000).unwrap(),
        Some(ClaimState::Lost { winner: b.id })
    );
}

#[test]
fn claim_that_takes_nothing_is_stored_however_late() {
    let mut s = store();
    let a = claim(1, 1, 10, T, &["src/net/**"], 3600);
    s.append_at(&a, T).unwrap();
    // Old, low Lamport, but disjoint paths.
    let b = claim(2, 1, 2, T - 3_000_000, &["docs/**"], 3600);
    assert_eq!(
        s.append_at(&b, T + 60_000).unwrap(),
        AppendOutcome::Inserted
    );
    // Late and overlapping, but it loses anyway: nothing is taken.
    let c = claim(3, 1, 50, T, &["src/net/b.rs"], 3600);
    assert_eq!(
        s.append_at(&c, T + 60_000).unwrap(),
        AppendOutcome::Inserted
    );
}

#[test]
fn backdated_renewal_of_an_expired_claim_cannot_take_a_settled_lease() {
    let mut s = store();
    let a1 = claim(1, 1, 1, T, &["x"], 60);
    s.append_at(&a1, T).unwrap();
    // a1 expired at T+60s; b takes x and settles.
    let b = claim(2, 1, 5, T + 70_000, &["x"], 3600);
    s.append_at(&b, T + 70_000).unwrap();
    // A "renewal" of a1, backdated to just before a1 expired.
    let a2 = claim(1, 2, 6, T + 59_000, &["x"], 3600);
    assert!(matches!(
        s.append_at(&a2, T + 100_000),
        Err(StoreError::LeaseTakeover)
    ));
    assert_eq!(holder(&s, "x", T + 100_000), Some(b.id));
}

#[test]
fn shrinking_renewal_cannot_take_a_settled_lease() {
    let mut s = store();
    // w holds b, so a1's atomic set [a, b] loses and x takes a.
    let w = claim(3, 1, 1, T, &["b"], 3600);
    let a1 = claim(1, 1, 2, T, &["a", "b"], 3600);
    let x = claim(2, 1, 5, T + 10_000, &["a"], 3600);
    s.append_batch_at(&[w, a1, x.clone()], T + 10_000).unwrap();
    assert_eq!(holder(&s, "a", T + 20_000), Some(x.id));
    // a1's holder drops b: the renewal would inherit a1's place in line,
    // but a1 is dead (Addendum 3), so its renewal is dead too.
    let a2 = claim(1, 2, 6, T + 20_000, &["a"], 3600);
    let _ = s.append_at(&a2, T + 20_000);
    assert!(!matches!(
        s.claim_state(&a2.id, T + 30_000).unwrap(),
        Some(ClaimState::Granted | ClaimState::Pending)
    ));
    assert_eq!(holder(&s, "a", T + 30_000), Some(x.id));
}

#[test]
fn equivocating_twin_cannot_smuggle_a_takeover() {
    let mut s = store();
    let victim = claim(1, 1, 10, T, &["src/**"], 3600);
    s.append_at(&victim, T).unwrap();
    let attacker = claim(2, 1, 2, T, &["src/a.rs"], 3600);
    // A harmless seq-1 twin with a higher id, stored first.
    let note = (0u32..)
        .map(|i| {
            sign(
                2,
                1,
                3,
                T,
                EventBody::Note {
                    scope: "s".into(),
                    text: format!("n{i}"),
                },
            )
        })
        .find(|n| n.id > attacker.id)
        .unwrap();
    s.append_at(&note, T + 1).unwrap();
    assert!(matches!(
        s.append_at(&attacker, T + 60_000),
        Err(StoreError::LeaseTakeover)
    ));
    assert_eq!(holder(&s, "src/**", T + 60_000), Some(victim.id));
}

#[test]
fn same_instant_arrivals_do_not_depend_on_order() {
    // Only events first seen strictly before `now` can protect a lease, so
    // a release and a claim arriving together give one result in any order,
    // one by one or batched.
    let victim = claim(1, 1, 10, T, &["src/**"], 3600);
    let rel = release(1, 2, 11, T + 30_000, &victim);
    let attacker = claim(2, 1, 2, T, &["src/a.rs"], 3600);
    let now = T + 60_000;
    let run = |evs: &[&Event], batch: bool| {
        let mut s = store();
        s.append_at(&victim, T).unwrap();
        if batch {
            let owned: Vec<Event> = evs.iter().map(|e| (*e).clone()).collect();
            s.append_batch_at(&owned, now).unwrap();
        } else {
            for e in evs {
                let _ = s.append_at(e, now);
            }
        }
        s.events_since(Lamport(0)).unwrap()
    };
    let base = run(&[&rel, &attacker], false);
    assert_eq!(run(&[&attacker, &rel], false), base);
    assert_eq!(run(&[&rel, &attacker], true), base);
    assert_eq!(run(&[&attacker, &rel], true), base);
}

// ---- property: admission never depends on arrival order within an instant ----

use proptest::prelude::*;

const PATHS: [&str; 3] = ["a", "b", "d/**"];

/// (node, lamport, ts offset, paths, release-target index).
type Spec = (u8, u64, u64, Vec<&'static str>, Option<usize>);

fn arb_claims(n: std::ops::Range<usize>) -> impl Strategy<Value = Vec<Spec>> {
    prop::collection::vec(
        (
            1u8..4,
            1u64..20,
            0u64..90_000,
            prop::collection::vec(prop::sample::select(PATHS.to_vec()), 1..3),
            prop::option::weighted(0.2, 0usize..8),
        ),
        n,
    )
}

/// Earlier events (stored and settled before `NOW`), then events offered at
/// `NOW`: claims, plus releases of earlier claims.
fn build(old: &[Spec], new: &[Spec]) -> (Vec<Event>, Vec<Event>) {
    let mut seq = [0u64; 4];
    let mut next = |k: u8| {
        seq[k as usize] += 1;
        seq[k as usize]
    };
    let old: Vec<(u8, Event)> = old
        .iter()
        .map(|(k, l, ts, p, _)| (*k, claim(*k, next(*k), *l, T + ts, p, 600)))
        .collect();
    let mut fresh = Vec::new();
    for (k, l, ts, p, rel) in new {
        fresh.push(match rel.and_then(|i| old.get(i)) {
            // The claim's own node releases it.
            Some((by, target)) => release(*by, next(*by), l + 20, T + 100_000, target),
            None => claim(*k, next(*k), *l, T + ts, p, 600),
        });
    }
    let old = old.into_iter().map(|(_, e)| e).collect();
    (old, fresh)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn same_instant_admission_is_order_independent(
        old in arb_claims(1..6),
        new in arb_claims(1..8),
        seed in any::<u64>(),
        batch in any::<bool>(),
    ) {
        let (old, fresh) = build(&old, &new);
        let now = T + 120_000;
        let run = |evs: &[Event], batch: bool| {
            let mut s = store();
            for e in &old {
                let _ = s.append_at(e, T + 100_000);
            }
            if batch {
                s.append_batch_at(evs, now).unwrap();
            } else {
                for e in evs {
                    let _ = s.append_at(e, now);
                }
            }
            (s.events_since(Lamport(0)).unwrap(), s.views().active_leases(now).collect::<Vec<_>>())
        };
        let mut perm = fresh.clone();
        // Deterministic shuffle from the seed.
        let mut x = seed | 1;
        for i in (1..perm.len()).rev() {
            x ^= x << 13; x ^= x >> 7; x ^= x << 17;
            perm.swap(i, (x % (i as u64 + 1)) as usize);
        }
        prop_assert_eq!(run(&fresh, false), run(&perm, batch));
    }
}
