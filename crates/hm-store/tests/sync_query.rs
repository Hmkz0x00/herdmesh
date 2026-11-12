//! Store support for anti-entropy sync: batch append, have-vector, missing
//! events with caps, and received_at clamping of lease times.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, Lamport, NodeId, RoomId};
use hm_store::settle::ClaimState;
use hm_store::{BatchReport, Store, SyncCaps};

fn key_from(seed: u8) -> SigningKey {
    SigningKey::from_bytes(blake3::hash(&[b's', seed]).as_bytes())
}

fn node(k: &SigningKey) -> NodeId {
    NodeId::from_verifying_key(&k.verifying_key())
}

fn room() -> RoomId {
    RoomId::from_bytes([4; 32])
}

fn note(k: &SigningKey, lamport: u64, text: &str) -> Event {
    note_seq(k, lamport, lamport, text)
}

fn note_seq(k: &SigningKey, seq: u64, lamport: u64, text: &str) -> Event {
    Event::sign(
        k,
        room(),
        "ag",
        seq,
        Lamport(lamport),
        1,
        EventBody::Note {
            scope: "s".into(),
            text: text.into(),
        },
    )
    .unwrap()
}

fn snapshot(s: &Store) -> Vec<u8> {
    postcard::to_allocvec(s.views()).unwrap()
}

const BIG: SyncCaps = SyncCaps {
    max_events: 10_000,
    max_bytes: 64 << 20,
};

#[test]
fn append_batch_reports_and_matches_single_appends() {
    let (k1, k2) = (key_from(1), key_from(2));
    let good: Vec<Event> = (1..=5)
        .map(|l| note(&k1, l, "x"))
        .chain((1..=3).map(|l| note(&k2, l, "y")))
        .collect();
    let mut tampered = note(&k1, 9, "z");
    tampered.lamport = Lamport(10);
    let wrong_room = Event::sign(
        &k1,
        RoomId::from_bytes([0; 32]),
        "ag",
        1,
        Lamport(1),
        1,
        EventBody::Leave,
    )
    .unwrap();

    let mut batch = good.clone();
    batch.push(tampered);
    batch.push(wrong_room);
    batch.push(good[0].clone()); // duplicate inside the batch

    let mut s = Store::open_in_memory(room()).unwrap();
    s.append(&good[2]).unwrap(); // already stored before the batch
    let r: BatchReport = s.append_batch_at(&batch, 1_000).unwrap();
    assert_eq!(r.inserted, 7);
    assert_eq!(r.duplicates, 2);
    assert_eq!(r.rejected, 2);
    assert_eq!(s.event_count().unwrap(), 8);

    let mut one_by_one = Store::open_in_memory(room()).unwrap();
    for e in good.iter().rev() {
        one_by_one.append(e).unwrap();
    }
    assert_eq!(snapshot(&s), snapshot(&one_by_one));
    s.rebuild_views().unwrap();
    assert_eq!(snapshot(&s), snapshot(&one_by_one));
}

#[test]
fn have_vector_is_max_lamport_per_node() {
    let (k1, k2) = (key_from(1), key_from(2));
    let mut s = Store::open_in_memory(room()).unwrap();
    assert!(s.have().unwrap().is_empty());
    for e in [note(&k1, 3, "a"), note(&k1, 7, "b"), note(&k2, 2, "c")] {
        s.append(&e).unwrap();
    }
    let have = s.have().unwrap();
    assert_eq!(have.len(), 2);
    assert_eq!(have[&node(&k1)], Lamport(7));
    assert_eq!(have[&node(&k2)], Lamport(2));
}

#[test]
fn missing_events_respect_have_vector() {
    let (k1, k2) = (key_from(1), key_from(2));
    let mut s = Store::open_in_memory(room()).unwrap();
    for l in 1..=4 {
        s.append(&note(&k1, l, "a")).unwrap();
        s.append(&note(&k2, l, "b")).unwrap();
    }
    let have = BTreeMap::from([(node(&k1), Lamport(2))]);
    let (wire, more) = s.missing_events(&have, BIG).unwrap();
    assert!(!more);
    let got: Vec<Event> = wire.iter().map(|b| Event::decode(b).unwrap()).collect();
    // k1: lamport 3,4; k2 (unknown to requester): all 4.
    assert_eq!(got.len(), 6);
    assert!(
        got.iter()
            .all(|e| e.author.node != node(&k1) || e.lamport.0 > 2)
    );
    // In (lamport, node, id) order.
    assert!(
        got.windows(2)
            .all(|w| (w[0].lamport, w[0].author.node, w[0].id)
                < (w[1].lamport, w[1].author.node, w[1].id))
    );
}

#[test]
fn missing_events_capped_with_more_flag() {
    let k = key_from(1);
    let mut s = Store::open_in_memory(room()).unwrap();
    for l in 1..=10 {
        s.append(&note(&k, l, "a")).unwrap();
    }
    let caps = SyncCaps {
        max_events: 4,
        max_bytes: 64 << 20,
    };
    let (wire, more) = s.missing_events(&BTreeMap::new(), caps).unwrap();
    assert_eq!(wire.len(), 4);
    assert!(more);

    let one = wire[0].len();
    let caps = SyncCaps {
        max_events: 100,
        max_bytes: one * 3,
    };
    let (wire, more) = s.missing_events(&BTreeMap::new(), caps).unwrap();
    assert_eq!(wire.len(), 3);
    assert!(more);
}

#[test]
fn cap_never_splits_same_node_and_lamport() {
    // Two events from one node with the same lamport: a cap between them would
    // make the requester's have-vector skip the second one forever.
    let k = key_from(1);
    let mut s = Store::open_in_memory(room()).unwrap();
    s.append(&note(&k, 1, "a")).unwrap();
    s.append(&note(&k, 2, "b")).unwrap();
    s.append(&note_seq(&k, 3, 2, "c")).unwrap();
    let caps = SyncCaps {
        max_events: 2,
        max_bytes: 64 << 20,
    };
    let (wire, more) = s.missing_events(&BTreeMap::new(), caps).unwrap();
    assert_eq!(wire.len(), 1);
    assert!(more);
}

#[test]
fn past_dated_claim_keeps_its_own_ts() {
    let k = key_from(1);
    let claim = Event::sign(
        &k,
        room(),
        "ag",
        1,
        Lamport(1),
        500,
        EventBody::LeaseClaim {
            paths: vec!["a".into()],
            ttl_secs: 1,
        },
    )
    .unwrap();
    let mut s = Store::open_in_memory(room()).unwrap();
    s.append_batch_at(std::slice::from_ref(&claim), 9_000)
        .unwrap();
    assert_eq!(
        s.views().active_leases(0).next().unwrap().1.expires_at_ms,
        1_500
    );
    assert_eq!(
        s.claim_state(&claim.id, 1_500).unwrap(),
        Some(ClaimState::Expired)
    );
}
