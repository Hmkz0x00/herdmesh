//! Two room cores in-process, relay disabled, loopback only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use herdmesh::data::{DataDir, RoleName, RoomConfig};
use herdmesh::room;
use herdmesh::session::RoomSession;
use hm_net::{RelayChoice, RoomSecret, node_id_from_endpoint};
use hm_proto::{Event, EventBody, EventKind, Lamport};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

fn events(s: &RoomSession) -> Vec<Event> {
    s.store().lock().unwrap().events_since(Lamport(0)).unwrap()
}

/// A creates and opens room "alpha"; B joins from A's ticket text (the same
/// path as `room join`) but is not opened yet.
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
    let a = RoomSession::open(&a_dir, id, RelayChoice::Disabled)
        .await
        .unwrap();
    (a_dir, b_dir, a)
}

async fn join_b(b_dir: &DataDir, a: &RoomSession) -> RoomSession {
    let ticket = a.ticket().await.to_string();
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
    assert_eq!(events(&a).len(), 50);

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
    assert_eq!(events(&b).len(), 50);

    // `room status` reports the count and A's sync result from sync.json.
    let status = room::status(&b_dir, "alpha-b").unwrap();
    assert!(status.contains("events: 50"), "{status}");
    assert!(
        status.contains(&format!("{} ok", &a_node[..10])),
        "{status}"
    );

    a.close().await;
    b.close().await;
}
