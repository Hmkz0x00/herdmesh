//! GATE-7 N20 / ADR-005 addendum: removal (demotion to observer) is
//! retroactive. Roles are resolved over the whole stored set first; every
//! other event is judged by its author's final role.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::{Event, EventBody, Lamport, NodeId, Ref, Role, RoomId, TaskId, TaskStatus};
use hm_store::Store;
use proptest::prelude::*;

const T: u64 = 1_700_000_000_000;

/// Deterministic test keys; never real secrets.
fn key(n: u8) -> SigningKey {
    SigningKey::from_bytes(&[n; 32])
}

fn node(n: u8) -> NodeId {
    NodeId::from_bytes(key(n).verifying_key().to_bytes())
}

fn room() -> RoomId {
    RoomId::from_bytes([2; 32])
}

fn sign(k: u8, seq: u64, lamport: u64, body: EventBody) -> Event {
    Event::sign(&key(k), room(), "ag", seq, Lamport(lamport), T, body).unwrap()
}

fn genesis() -> Event {
    sign(1, 1, 1, EventBody::Genesis { owner: node(1) })
}

fn task(k: u8, seq: u64, l: u64, id: &str) -> Event {
    sign(
        k,
        seq,
        l,
        EventBody::TaskAdd {
            task: TaskId::new(id).unwrap(),
            title: id.into(),
            paths: vec![],
        },
    )
}

fn update(k: u8, seq: u64, l: u64, id: &str, status: TaskStatus) -> Event {
    sign(
        k,
        seq,
        l,
        EventBody::TaskUpdate {
            task: TaskId::new(id).unwrap(),
            status,
            note: Some(format!("by {k}")),
        },
    )
}

fn decision(k: u8, seq: u64, l: u64) -> Event {
    sign(
        k,
        seq,
        l,
        EventBody::Decision {
            title: format!("d{k}-{seq}"),
            rationale: String::new(),
            refs: Vec::<Ref>::new(),
        },
    )
}

fn remove(seq: u64, l: u64, who: u8) -> Event {
    sign(
        1,
        seq,
        l,
        EventBody::RoleGrant {
            node: node(who),
            role: Role::Observer,
        },
    )
}

fn store_with(events: &[Event]) -> Store {
    let mut s = Store::open_in_memory(room()).unwrap();
    s.pin_genesis(genesis().id).unwrap();
    for e in events {
        let _ = s.append_at(e, T);
    }
    s
}

/// Owner (1) adds task "own"; member M (2) adds task "m", a decision, and
/// updates "own"; then the owner removes M at L10.
fn scenario() -> Vec<Event> {
    vec![
        genesis(),
        task(1, 2, 2, "own"),
        task(2, 1, 3, "m"),
        decision(2, 2, 4),
        update(2, 3, 5, "own", TaskStatus::Blocked),
        remove(3, 10, 2),
    ]
}

#[test]
fn removal_retracts_prior_tasks_updates_and_decisions() {
    let s = store_with(&scenario());
    let v = s.views();
    assert_eq!(v.role_of(&node(2)), Role::Observer);
    assert!(v.decisions.is_empty());
    assert!(!v.board.contains_key(&TaskId::new("m").unwrap()));
    let own = &v.board[&TaskId::new("own").unwrap()];
    assert_eq!(own.status, TaskStatus::Open);
    assert_eq!(own.note, None);
}

#[test]
fn backdated_writes_after_removal_are_ignored() {
    let mut s = store_with(&scenario());
    // Signed after removal with unused seqs and a Lamport below the grant.
    for e in [
        task(2, 4, 9, "late"),
        decision(2, 5, 9),
        update(2, 6, 9, "own", TaskStatus::Done),
    ] {
        let _ = s.append_at(&e, T + 1);
    }
    let v = s.views();
    assert!(v.decisions.is_empty());
    assert!(!v.board.contains_key(&TaskId::new("late").unwrap()));
    assert_eq!(
        v.board[&TaskId::new("own").unwrap()].status,
        TaskStatus::Open
    );
}

#[test]
fn removal_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("r.db");
    {
        let mut s = Store::open(&path, room()).unwrap();
        s.pin_genesis(genesis().id).unwrap();
        for e in scenario() {
            let _ = s.append_at(&e, T);
        }
    }
    let mut s = Store::open(&path, room()).unwrap();
    s.rebuild_views().unwrap();
    assert!(s.views().decisions.is_empty());
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(32))]

    /// Any delivery order, one by one: same views.
    #[test]
    fn removal_is_order_independent(perm in Just((0..9usize).collect::<Vec<_>>()).prop_shuffle()) {
        let mut evs = scenario();
        evs.extend([task(2, 4, 9, "late"), decision(2, 5, 9), update(2, 6, 9, "own", TaskStatus::Done)]);
        let shuffled: Vec<Event> = perm.iter().map(|&i| evs[i].clone()).collect();
        let snap = |s: &Store| postcard::to_allocvec(s.views()).unwrap();
        prop_assert_eq!(snap(&store_with(&evs)), snap(&store_with(&shuffled)));
    }
}
