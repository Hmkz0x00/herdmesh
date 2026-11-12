//! Daemon queries the CLI loop builds on: local members, presence, pings.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use ed25519_dalek::SigningKey;
use hm_daemon::ipc::{Call, Request, Response};
use hm_daemon::{Daemon, NullNet};
use hm_proto::{AgentStatus, EventBody, EventKind, Lamport, RoomId};
use hm_store::Store;

fn daemon() -> (Daemon, RoomId) {
    let d = Daemon::new(SigningKey::from_bytes(&[7; 32]), Arc::new(NullNet));
    let id = d
        .create_room(
            "alpha",
            Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
        )
        .unwrap();
    (d, id)
}

fn req(d: &Daemon, agent: &str, call: Call) -> Response {
    d.handle_request(Request {
        agent: agent.into(),
        room: None,
        call,
    })
}

#[test]
fn local_members_lists_registered_agents_only() {
    let (d, room) = daemon();
    assert!(d.local_members(room).is_empty());
    req(&d, "a", Call::RoomCard);
    req(&d, "b", Call::RoomCard);
    let mut m = d.local_members(room);
    m.sort();
    assert_eq!(m, ["a", "b"]);
}

#[test]
fn presence_needs_membership_and_is_stored() {
    let (d, room) = daemon();
    assert!(d.set_presence(room, "a", AgentStatus::Idle).is_err());
    req(&d, "a", Call::RoomCard);
    d.set_presence(room, "a", AgentStatus::Working).unwrap();
    let (_, store) = (0, d.store_for_test(room));
    let n = store
        .lock()
        .unwrap()
        .events_since(Lamport(0))
        .unwrap()
        .iter()
        .filter(|e| {
            e.kind() == EventKind::Presence
                && matches!(
                    e.body,
                    EventBody::Presence {
                        status: AgentStatus::Working
                    }
                )
        })
        .count();
    assert_eq!(n, 1);
}

#[test]
fn pings_cover_unread_asks_until_inbox_ack() {
    let (d, _) = daemon();
    req(&d, "a", Call::RoomCard);
    req(&d, "b", Call::RoomCard);
    req(
        &d,
        "a",
        Call::Ask {
            to: "b".into(),
            question: "ready?".into(),
        },
    );
    let p = d.pending_pings();
    assert_eq!(p.len(), 1, "{p:?}");
    assert_eq!((p[0].agent.as_str(), p[0].kind), ("b", "Ask"));
    assert!(p[0].from.ends_with("/a"), "{}", p[0].from);
    // Not acked yet: still pending. After ack: gone.
    assert_eq!(d.pending_pings().len(), 1);
    req(&d, "b", Call::Inbox { ack: true });
    assert!(d.pending_pings().is_empty());
}
