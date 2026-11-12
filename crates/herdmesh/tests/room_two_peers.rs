//! Two room cores in-process, relay disabled, loopback only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use herdmesh::data::{DataDir, RoleName, RoomConfig};
use herdmesh::room;
use herdmesh::session::{RoomSession, init_genesis};
use hm_net::{RelayChoice, RoomSecret, RoomTicket, node_id_from_endpoint};
use hm_proto::{Event, EventBody, EventId, EventKind, Lamport, NodeId};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

fn events(s: &RoomSession) -> Vec<Event> {
    s.store().lock().unwrap().events_since(Lamport(0)).unwrap()
}

/// A creates room "alpha" with its Genesis (as `room create` does) and
/// opens it.
async fn setup(tmp: &tempfile::TempDir) -> (DataDir, DataDir, RoomSession) {
    let a_dir = DataDir::new(tmp.path().join("a"));
    let b_dir = DataDir::new(tmp.path().join("b"));
    let secret = RoomSecret::generate().unwrap();
    let id = a_dir
        .create_room(
            &secret,
            &RoomConfig::new("alpha", RoleName::Owner, "alice"),
            &[],
        )
        .unwrap();
    init_genesis(&a_dir, id, "alice").unwrap();
    let a = RoomSession::open(&a_dir, id, RelayChoice::Disabled)
        .await
        .unwrap();
    (a_dir, b_dir, a)
}

/// B joins from A's ticket text (the same path as `room join`) and opens.
async fn join_b(b_dir: &DataDir, a: &RoomSession) -> RoomSession {
    join_b_with(b_dir, a, |_| {}).await
}

async fn join_b_with(
    b_dir: &DataDir,
    a: &RoomSession,
    edit: impl FnOnce(&mut RoomTicket),
) -> RoomSession {
    let mut t = a.ticket().await.unwrap();
    edit(&mut t);
    let ticket = t.to_string();
    let out = room::join(b_dir, &ticket, "bob", Some("alpha-b")).unwrap();
    assert!(out.contains("Joined room 'alpha-b'"), "{out}");
    assert!(
        !out.contains(&ticket),
        "join output must not echo the ticket"
    );
    let (b_id, b_cfg) = b_dir.resolve_room("alpha-b").unwrap();
    assert_eq!(b_id, a.room_id());
    assert_eq!(b_cfg.agent_name, "bob");
    RoomSession::open(b_dir, b_id, RelayChoice::Disabled)
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn create_join_publish_note_reaches_other_store() {
    let tmp = tempfile::tempdir().unwrap();
    let (_a_dir, b_dir, mut a) = setup(&tmp).await;
    let mut b = join_b(&b_dir, &a).await;

    timeout(DEADLINE, b.joined())
        .await
        .expect("B join timed out")
        .unwrap();
    timeout(DEADLINE, a.joined())
        .await
        .expect("A join timed out")
        .unwrap();

    let note = a
        .publish(EventBody::Note {
            scope: "test".into(),
            text: "hello from A".into(),
        })
        .await
        .unwrap();
    assert_eq!(note.author.agent_name, "alice");

    // B keeps stepping (as `room run` does) until the note is in its store.
    let got = timeout(DEADLINE, async {
        while !events(&b).iter().any(|e| e.id == note.id) {
            let line = b.step().await.expect("B subscription ended").unwrap();
            assert!(
                !line.contains("hello from A"),
                "console line leaked body: {line}"
            );
        }
    })
    .await;
    assert!(got.is_ok(), "B never stored the note");
    let n = events(&b).into_iter().find(|e| e.id == note.id).unwrap();
    assert_eq!(n.kind(), EventKind::Note);

    // `room status` opens its own store handle while the session holds one.
    let status = room::status(&b_dir, "alpha-b").unwrap();
    assert!(status.contains("room alpha-b"), "{status}");

    a.close().await;
    b.close().await;
}

/// Events published while B did not exist can only reach B via sync: gossip
/// has no history.
#[tokio::test(flavor = "multi_thread")]
async fn late_joiner_catches_up_via_sync() {
    let tmp = tempfile::tempdir().unwrap();
    let (_a_dir, b_dir, mut a) = setup(&tmp).await;

    let mut published = Vec::new();
    for i in 0..50 {
        let ev = a
            .publish(EventBody::Note {
                scope: "backlog".into(),
                text: format!("note {i}"),
            })
            .await
            .unwrap();
        published.push(ev.id);
    }
    // 50 notes after A's Genesis.
    assert_eq!(events(&a).len(), 51);

    let mut b = join_b(&b_dir, &a).await;
    let a_node = node_id_from_endpoint(a.net().endpoint().id()).to_string();

    // Step B like `room run` until it has all 50 and has recorded a sync
    // from A that inserted them.
    let mut max_inserted = 0;
    let caught_up = timeout(DEADLINE, async {
        loop {
            if let Some(r) = b.sync_log().get(&a_node) {
                assert!(r.ok, "sync failed: {:?}", r.error);
                max_inserted = max_inserted.max(r.inserted);
            }
            let have = events(&b);
            if published.iter().all(|id| have.iter().any(|e| e.id == *id)) && max_inserted >= 50 {
                break;
            }
            b.step().await.expect("B subscription ended").unwrap();
        }
    })
    .await;
    assert!(
        caught_up.is_ok(),
        "B has {} of 50 events, sync inserted {max_inserted}",
        events(&b).len()
    );
    assert_eq!(events(&b).len(), 51);
    // The ticket's genesis arrived by sync and anchors A as owner.
    let a_id = NodeId::from_bytes(*a.net().endpoint().id().as_bytes());
    assert_eq!(b.store().lock().unwrap().views().owner, Some(a_id));

    // `room status` reports the count and A's sync result from sync.json.
    let status = room::status(&b_dir, "alpha-b").unwrap();
    assert!(status.contains("events: 51"), "{status}");
    assert!(
        status.contains(&format!("owner: {} (genesis verified)", &a_node[..10])),
        "{status}"
    );
    assert!(
        status.contains(&format!("{} ok", &a_node[..10])),
        "{status}"
    );

    a.close().await;
    b.close().await;
}

/// A ticket whose genesis id is not the room's real Genesis never verifies:
/// B syncs every event but recognises no owner.
#[tokio::test(flavor = "multi_thread")]
async fn forged_genesis_in_ticket_is_never_verified() {
    let tmp = tempfile::tempdir().unwrap();
    let (_a_dir, b_dir, mut a) = setup(&tmp).await;
    a.publish(EventBody::Note {
        scope: "s".into(),
        text: "hi".into(),
    })
    .await
    .unwrap();
    let real = a.store().lock().unwrap().genesis().unwrap();
    let forged = EventId::from_bytes([0xee; 32]);
    let mut b = join_b_with(&b_dir, &a, |t| t.genesis = forged).await;
    let a_node = node_id_from_endpoint(a.net().endpoint().id()).to_string();
    let synced = timeout(DEADLINE, async {
        loop {
            if b.sync_log().get(&a_node).is_some_and(|r| r.ok) && events(&b).len() == 2 {
                break;
            }
            b.step().await.expect("B subscription ended").unwrap();
        }
    })
    .await;
    assert!(synced.is_ok(), "B has {} events", events(&b).len());
    assert!(events(&b).iter().any(|e| e.id == real));
    assert_eq!(b.store().lock().unwrap().views().owner, None);
    let status = room::status(&b_dir, "alpha-b").unwrap();
    assert!(status.contains("owner: unverified"), "{status}");
    // A pinned genesis can't be swapped by re-pinning another id.
    assert!(herdmesh::session::pin_genesis(&b_dir, a.room_id(), real).is_err());
    a.close().await;
    b.close().await;
}
