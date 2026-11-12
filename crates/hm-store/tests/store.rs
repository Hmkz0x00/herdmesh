//! Acceptance tests for hm-store.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::SigningKey;
use hm_proto::{
    AgentRef, AgentStatus, Event, EventBody, Lamport, NodeId, Ref, Role, RoomId, TaskId, TaskStatus,
};
use hm_store::{AppendOutcome, Store, StoreError, Views, next_lamport};
use proptest::prelude::*;

fn key_from(seed: u8) -> SigningKey {
    // Deterministic per-seed test keys derived at runtime (never real keys).
    SigningKey::from_bytes(blake3::hash(&[b'k', seed]).as_bytes())
}

fn room() -> RoomId {
    RoomId::from_bytes([9; 32])
}

fn ev(k: &SigningKey, name: &str, lamport: u64, ts: u64, body: EventBody) -> Event {
    Event::sign(k, room(), name, Lamport(lamport), ts, body).unwrap()
}

fn agent(k: &SigningKey, name: &str) -> AgentRef {
    AgentRef::new(NodeId::from_verifying_key(&k.verifying_key()), name)
}

fn task(id: &str) -> TaskId {
    TaskId::new(id).unwrap()
}

fn snapshot(v: &Views) -> Vec<u8> {
    postcard::to_allocvec(v).unwrap()
}

#[test]
fn duplicate_append_is_noop() {
    let k = key_from(1);
    let mut s = Store::open_in_memory(room()).unwrap();
    let e = ev(&k, "a", 1, 0, EventBody::Join { role: Role::Owner });
    assert_eq!(s.append(&e).unwrap(), AppendOutcome::Inserted);
    let before = snapshot(s.views());
    assert_eq!(s.append(&e).unwrap(), AppendOutcome::Duplicate);
    assert_eq!(snapshot(s.views()), before);
    assert_eq!(s.event_count().unwrap(), 1);
}

#[test]
fn tampered_event_is_rejected() {
    let k = key_from(1);
    let mut s = Store::open_in_memory(room()).unwrap();
    let mut e = ev(&k, "a", 1, 0, EventBody::Leave);
    e.lamport = Lamport(2);
    assert!(matches!(s.append(&e), Err(StoreError::Proto(_))));
    assert_eq!(s.event_count().unwrap(), 0);
}

#[test]
fn wrong_room_is_rejected() {
    let k = key_from(1);
    let mut s = Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap();
    let e = ev(&k, "a", 1, 0, EventBody::Leave);
    assert!(matches!(s.append(&e), Err(StoreError::WrongRoom)));
}

#[test]
fn bodies_are_deduplicated() {
    let (k1, k2) = (key_from(1), key_from(2));
    let mut s = Store::open_in_memory(room()).unwrap();
    s.append(&ev(&k1, "a", 1, 0, EventBody::Leave)).unwrap();
    s.append(&ev(&k2, "b", 1, 0, EventBody::Leave)).unwrap();
    s.append(&ev(&k1, "a", 2, 0, EventBody::Leave)).unwrap();
    assert_eq!(s.event_count().unwrap(), 3);
    assert_eq!(s.blob_count().unwrap(), 1);
}

#[test]
fn board_decisions_members() {
    let (k1, k2) = (key_from(1), key_from(2));
    let mut s = Store::open_in_memory(room()).unwrap();
    let r: Ref = "task:t1".parse().unwrap();
    for e in [
        ev(&k1, "a", 1, 0, EventBody::Join { role: Role::Owner }),
        ev(
            &k1,
            "a",
            2,
            0,
            EventBody::TaskAdd {
                task: task("t1"),
                title: "first".into(),
                paths: vec!["src/**".into()],
            },
        ),
        ev(
            &k2,
            "b",
            3,
            0,
            EventBody::Presence {
                status: AgentStatus::Working,
            },
        ),
        ev(
            &k2,
            "b",
            4,
            0,
            EventBody::TaskUpdate {
                task: task("t1"),
                status: TaskStatus::Done,
                note: Some("ok".into()),
            },
        ),
        // Update for an unknown task is ignored.
        ev(
            &k2,
            "b",
            5,
            0,
            EventBody::TaskUpdate {
                task: task("nope"),
                status: TaskStatus::Done,
                note: None,
            },
        ),
        ev(
            &k1,
            "a",
            6,
            0,
            EventBody::Decision {
                title: "use sqlite".into(),
                rationale: "bundled".into(),
                refs: vec![r.clone()],
            },
        ),
    ] {
        s.append(&e).unwrap();
    }
    let v = s.views();
    let t = &v.board[&task("t1")];
    assert_eq!(t.status, TaskStatus::Done);
    assert_eq!(t.note.as_deref(), Some("ok"));
    assert_eq!(t.created_by, agent(&k1, "a"));
    assert_eq!(v.board.len(), 1);
    assert_eq!(v.decisions.len(), 1);
    assert_eq!(v.decisions[0].title, "use sqlite");
    assert_eq!(v.decisions[0].refs, vec![r]);
    let a = &v.members[&agent(&k1, "a")];
    assert_eq!(a.role, Some(Role::Owner));
    assert!(a.present);
    assert_eq!(a.last_lamport, Lamport(6));
    let b = &v.members[&agent(&k2, "b")];
    assert_eq!(b.status, Some(AgentStatus::Working));
    assert_eq!(b.role, None);
}

#[test]
fn leases_first_in_order_wins_and_expire() {
    let (k1, k2) = (key_from(1), key_from(2));
    let mut s = Store::open_in_memory(room()).unwrap();
    let claim = |k: &SigningKey, n: &str, l: u64, ts: u64| {
        ev(
            k,
            n,
            l,
            ts,
            EventBody::LeaseClaim {
                paths: vec!["src/lib.rs".into()],
                ttl_secs: 10,
            },
        )
    };
    // b claims at lamport 2, a at lamport 1: a wins regardless of arrival order.
    s.append(&claim(&k2, "b", 2, 1_000)).unwrap();
    s.append(&claim(&k1, "a", 1, 1_000)).unwrap();
    let l = &s.views().leases["src/lib.rs"];
    assert_eq!(l.holder, agent(&k1, "a"));
    assert_eq!(l.expires_at_ms, 11_000);
    assert_eq!(s.views().active_leases(10_999).count(), 1);
    assert_eq!(s.views().active_leases(11_000).count(), 0);

    // After a's lease expired (by claim ts), b's new claim wins.
    s.append(&claim(&k2, "b", 3, 20_000)).unwrap();
    assert_eq!(s.views().leases["src/lib.rs"].holder, agent(&k2, "b"));

    // Release by a non-holder is ignored; by the holder removes it.
    let release = |k: &SigningKey, n: &str, l: u64| {
        ev(
            k,
            n,
            l,
            20_001,
            EventBody::LeaseRelease {
                paths: vec!["src/lib.rs".into()],
            },
        )
    };
    s.append(&release(&k1, "a", 4)).unwrap();
    assert!(s.views().leases.contains_key("src/lib.rs"));
    s.append(&release(&k2, "b", 5)).unwrap();
    assert!(s.views().leases.is_empty());
}

#[test]
fn lamport_helper() {
    assert_eq!(next_lamport([]), Lamport(1));
    assert_eq!(
        next_lamport([Lamport(3), Lamport(9), Lamport(1)]),
        Lamport(10)
    );
    let k = key_from(1);
    let mut s = Store::open_in_memory(room()).unwrap();
    assert_eq!(s.next_lamport().unwrap(), Lamport(1));
    s.append(&ev(&k, "a", 41, 0, EventBody::Leave)).unwrap();
    assert_eq!(s.next_lamport().unwrap(), Lamport(42));
}

#[test]
fn cursors_and_events_since() {
    let k = key_from(1);
    let me = agent(&k, "a");
    let mut s = Store::open_in_memory(room()).unwrap();
    for l in [3, 1, 2] {
        s.append(&ev(&k, "a", l, 0, EventBody::Leave)).unwrap();
    }
    assert_eq!(s.cursor(&me).unwrap(), None);
    let all: Vec<u64> = s
        .events_since(Lamport(0))
        .unwrap()
        .iter()
        .map(|e| e.lamport.0)
        .collect();
    assert_eq!(all, vec![1, 2, 3]);
    s.advance_cursor(&me, Lamport(2)).unwrap();
    s.advance_cursor(&me, Lamport(1)).unwrap(); // never moves back
    assert_eq!(s.cursor(&me).unwrap(), Some(Lamport(2)));
    assert_eq!(s.events_since(Lamport(2)).unwrap().len(), 1);
}

#[test]
fn file_backed_store_persists() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("room.sqlite");
    let k = key_from(1);
    let e = ev(&k, "a", 1, 0, EventBody::Join { role: Role::Owner });
    let before = {
        let mut s = Store::open(&path, room()).unwrap();
        s.append(&e).unwrap();
        snapshot(s.views())
    };
    let mut s = Store::open(&path, room()).unwrap();
    assert_eq!(snapshot(s.views()), before);
    assert_eq!(s.append(&e).unwrap(), AppendOutcome::Duplicate);
    assert!(matches!(
        Store::open(&path, RoomId::from_bytes([1; 32])),
        Err(StoreError::WrongRoom)
    ));
}

// ---- property tests ----

const PATHS: [&str; 3] = ["a.rs", "b.rs", "dir/**"];
const TASKS: [&str; 3] = ["t1", "t2", "t3"];

fn arb_body() -> impl Strategy<Value = EventBody> {
    let path = prop::sample::select(PATHS.to_vec()).prop_map(String::from);
    let paths = prop::collection::vec(path, 1..3);
    let tid = prop::sample::select(TASKS.to_vec()).prop_map(task);
    prop_oneof![
        prop::sample::select(vec![Role::Owner, Role::Member, Role::Observer])
            .prop_map(|role| EventBody::Join { role }),
        Just(EventBody::Leave),
        prop::sample::select(vec![
            AgentStatus::Idle,
            AgentStatus::Working,
            AgentStatus::Done
        ])
        .prop_map(|status| EventBody::Presence { status }),
        (tid.clone(), "[a-z]{1,6}").prop_map(|(task, title)| EventBody::TaskAdd {
            task,
            title,
            paths: vec![],
        }),
        (
            tid,
            prop::sample::select(vec![TaskStatus::InProgress, TaskStatus::Done])
        )
            .prop_map(|(task, status)| EventBody::TaskUpdate {
                task,
                status,
                note: None
            }),
        (paths.clone(), 1u32..30)
            .prop_map(|(paths, ttl_secs)| EventBody::LeaseClaim { paths, ttl_secs }),
        paths.prop_map(|paths| EventBody::LeaseRelease { paths }),
        "[a-z]{1,6}".prop_map(|title| EventBody::Decision {
            title,
            rationale: String::new(),
            refs: vec![],
        }),
    ]
}

/// (author seed, lamport, ts, body): small ranges force lamport ties and lease conflicts.
fn arb_events() -> impl Strategy<Value = Vec<Event>> {
    prop::collection::vec((0u8..3, 1u64..8, 0u64..40_000, arb_body()), 1..24).prop_map(|specs| {
        specs
            .into_iter()
            .map(|(seed, l, ts, body)| ev(&key_from(seed), "ag", l, ts, body))
            .collect()
    })
}

fn views_after(events: &[Event]) -> (Vec<u8>, Store) {
    let mut s = Store::open_in_memory(room()).unwrap();
    for e in events {
        s.append(e).unwrap();
    }
    (snapshot(s.views()), s)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn any_insertion_order_gives_identical_views(
        (events, shuffled) in arb_events().prop_flat_map(|evs| {
            let s = Just(evs.clone()).prop_shuffle();
            (Just(evs), s)
        })
    ) {
        let (a, _) = views_after(&events);
        let (b, _) = views_after(&shuffled);
        prop_assert_eq!(a, b);
    }

    #[test]
    fn incremental_equals_rebuild(events in arb_events()) {
        let (incremental, mut s) = views_after(&events);
        s.rebuild_views().unwrap();
        prop_assert_eq!(incremental, snapshot(s.views()));
    }
}
