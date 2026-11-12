//! Every locally authored text field is redacted before it is signed,
//! stored or published.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use hm_daemon::ipc::{Call, Request, Response};
use hm_daemon::{Daemon, NetHandle};
use hm_proto::{Event, EventBody, Lamport, RoomId};
use hm_store::Store;

/// Synthetic GitHub-style token (not a real credential).
const TOKEN: &str = "ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";

#[derive(Default)]
struct Rec(Mutex<Vec<Event>>);
impl NetHandle for Rec {
    fn publish(&self, _: RoomId, e: &Event) {
        self.0.lock().unwrap().push(e.clone());
    }
}

fn setup() -> (Daemon, Arc<Rec>) {
    let net = Arc::new(Rec::default());
    let d = Daemon::new(SigningKey::from_bytes(&[5; 32]), net.clone());
    d.create_room(
        "alpha",
        Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
    )
    .unwrap();
    (d, net)
}

fn call(d: &Daemon, agent: &str, call: Call) -> Vec<(String, String)> {
    match d.handle_request(Request {
        agent: agent.into(),
        room: None,
        call,
    }) {
        Response::Ok(items) => items,
        Response::Err(e) => panic!("{e:?}"),
    }
}

fn leaked(net: &Rec, d: &Daemon) -> bool {
    let published = net.0.lock().unwrap();
    let dbg = format!("{published:?}");
    let room = d.room_ids()[0];
    let stored = d
        .store_for_test(room)
        .lock()
        .unwrap()
        .events_since(Lamport(0))
        .unwrap();
    dbg.contains(TOKEN) || format!("{stored:?}").contains(TOKEN)
}

#[test]
fn ask_with_a_token_is_published_and_stored_redacted() {
    let (d, net) = setup();
    call(&d, "b", Call::RoomCard);
    call(
        &d,
        "a",
        Call::Ask {
            to: "b".into(),
            question: format!("use {TOKEN} to log in?"),
        },
    );
    assert!(!leaked(&net, &d));
    let published = net.0.lock().unwrap();
    let q = published
        .iter()
        .find_map(|e| match &e.body {
            EventBody::Ask { q, .. } => Some(q.clone()),
            _ => None,
        })
        .unwrap();
    assert!(q.contains("[REDACTED:github_token]"), "{q}");
    assert!(q.starts_with("use ") && q.ends_with(" to log in?"), "{q}");
    assert_eq!(d.redaction_count(), 1);
}

#[test]
fn every_text_field_is_covered() {
    let (d, net) = setup();
    let t = |s: &str| format!("{s} {TOKEN}");
    call(&d, "b", Call::RoomCard);
    call(
        &d,
        "a",
        Call::Ask {
            to: "b".into(),
            question: "q".into(),
        },
    );
    let ask_id = {
        let room = d.room_ids()[0];
        let evs = d
            .store_for_test(room)
            .lock()
            .unwrap()
            .events_since(Lamport(0))
            .unwrap();
        evs.iter()
            .find(|e| matches!(e.body, EventBody::Ask { .. }))
            .unwrap()
            .id
            .to_string()
    };
    call(
        &d,
        "b",
        Call::Answer {
            ask_id,
            text: t("answer"),
        },
    );
    call(
        &d,
        "a",
        Call::Handoff {
            to: Some("b".into()),
            summary: t("summary"),
            refs: vec![],
            next_steps: vec![t("step")],
        },
    );
    call(
        &d,
        "a",
        Call::TaskAdd {
            task: "t1".into(),
            title: t("title"),
            paths: vec![],
        },
    );
    call(
        &d,
        "a",
        Call::TaskUpdate {
            task: "t1".into(),
            status: "blocked".into(),
            note: Some(t("note")),
        },
    );
    call(
        &d,
        "a",
        Call::Decide {
            title: t("title"),
            rationale: t("why"),
            refs: vec![],
        },
    );
    assert!(!leaked(&net, &d));
    assert_eq!(
        d.redaction_count(),
        7,
        "answer, summary, step, title, note, title, why"
    );
}

#[test]
fn clean_text_is_untouched_and_counts_nothing() {
    let (d, net) = setup();
    call(&d, "b", Call::RoomCard);
    call(
        &d,
        "a",
        Call::Ask {
            to: "b".into(),
            question: "which tokenizer?".into(),
        },
    );
    assert_eq!(d.redaction_count(), 0);
    let published = net.0.lock().unwrap();
    assert!(
        published
            .iter()
            .any(|e| matches!(&e.body, EventBody::Ask { q, .. } if q == "which tokenizer?"))
    );
}
