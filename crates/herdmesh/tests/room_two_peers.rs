//! Two room cores in-process: A creates, B joins from A's ticket, A publishes
//! a Note, B's store ends up with it. Relay disabled, loopback only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use herdmesh::data::{DataDir, RoleName, RoomConfig};
use herdmesh::room;
use herdmesh::session::RoomSession;
use hm_net::{RelayChoice, RoomSecret};
use hm_proto::{EventBody, EventKind};
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(10);

#[tokio::test(flavor = "multi_thread")]
async fn create_join_publish_note_reaches_other_store() {
    let tmp = tempfile::tempdir().unwrap();
    let a_dir = DataDir::new(tmp.path().join("a"));
    let b_dir = DataDir::new(tmp.path().join("b"));

    // A: create the room locally and go online.
    let secret = RoomSecret::generate().unwrap();
    let id = a_dir
        .create_room(
            &secret,
            &RoomConfig::new("alpha", RoleName::Owner, "alice"),
            &[],
        )
        .unwrap();
    let mut a = RoomSession::open(&a_dir, id, RelayChoice::Disabled)
        .await
        .unwrap();

    // B: join via A's ticket text (the same path as `room join`).
    let ticket = a.ticket().await.to_string();
    let out = room::join(&b_dir, &ticket, "bob", Some("alpha-b")).unwrap();
    assert!(out.contains("Joined room 'alpha-b'"), "{out}");
    assert!(
        !out.contains(&ticket),
        "join output must not echo the ticket"
    );
    let (b_id, b_cfg) = b_dir.resolve_room("alpha-b").unwrap();
    assert_eq!(b_id, id);
    assert_eq!(b_cfg.agent_name, "bob");
    let mut b = RoomSession::open(&b_dir, id, RelayChoice::Disabled)
        .await
        .unwrap();

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
        loop {
            if b.store()
                .events_since(hm_proto::Lamport(0))
                .unwrap()
                .iter()
                .any(|e| e.id == note.id)
            {
                break;
            }
            let line = b.step().await.expect("B subscription ended").unwrap();
            assert!(
                !line.contains("hello from A"),
                "console line leaked body: {line}"
            );
        }
    })
    .await;
    assert!(got.is_ok(), "B never stored the note");

    let stored = b.store().events_since(hm_proto::Lamport(0)).unwrap();
    let n = stored.iter().find(|e| e.id == note.id).unwrap();
    assert_eq!(n.kind(), EventKind::Note);

    // B's status renders from its own store.
    let status = room::status(&b_dir, "alpha-b");
    // The store file is still open in session B; status opens a second handle.
    assert!(status.unwrap().contains("room alpha-b"));

    a.close().await;
    b.close().await;
}
