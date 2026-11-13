//! `Call::Feed`: the read-only room snapshot behind `herdmesh view` (ADR-003).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::SigningKey;
use hm_daemon::ipc::{Call, Request, Response};
use hm_daemon::{Daemon, NullNet};
use hm_proto::{AgentStatus, Event, EventBody, Lamport, RoomId};
use hm_store::Store;
use serde_json::{Value, json};

const T0: u64 = 1_000_000_000;
/// Synthetic GitHub-style token (not a real credential).
const TOKEN: &str = "ghp_a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8";

fn setup() -> (Daemon, RoomId, Arc<AtomicU64>) {
    let now = Arc::new(AtomicU64::new(T0));
    let c = Arc::clone(&now);
    let d = Daemon::with_clock(
        SigningKey::from_bytes(&[7; 32]),
        Arc::new(NullNet),
        Box::new(move || c.load(Ordering::Relaxed)),
    );
    let id = d
        .create_room(
            "alpha",
            Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
        )
        .unwrap();
    (d, id, now)
}

fn req(d: &Daemon, agent: &str, call: Call) -> Response {
    d.handle_request(Request {
        agent: agent.into(),
        room: None,
        call,
    })
}

fn ok(d: &Daemon, agent: &str, call: Call) {
    assert!(matches!(req(d, agent, call), Response::Ok(_)));
}

fn feed(d: &Daemon, since: u64, limit: u32) -> Value {
    match req(d, "viewer", Call::Feed { since, limit }) {
        Response::Ok(items) => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].0, "feed");
            serde_json::from_str(&items[0].1).unwrap()
        }
        other => panic!("{other:?}"),
    }
}

fn member<'a>(v: &'a Value, name: &str) -> &'a Value {
    v["members"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == name)
        .unwrap_or_else(|| panic!("no member {name} in {v}"))
}

#[test]
fn feed_is_read_only_and_matches_the_adr_shape() {
    let (d, room, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    let before = d.store_for_test(room).lock().unwrap().event_count().unwrap();
    let v = feed(&d, 0, 50);
    let after = d.store_for_test(room).lock().unwrap().event_count().unwrap();
    assert_eq!(before, after, "Feed must not append anything");
    // the viewer is not made a member
    assert!(
        v["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["name"] != "viewer")
    );
    assert_eq!(v["room"], "alpha");
    assert_eq!(v["now_ms"], T0);
    assert!(v["you"].as_str().unwrap().ends_with("/viewer"));
    assert!(v["cursor"].as_u64().unwrap() >= 1);
    let a = member(&v, "alice");
    assert_eq!(a["role"], "owner");
    assert_eq!(a["local"], true);
    assert_eq!(a["present"], true);
    assert_eq!(a["status"], "unknown");
    assert_eq!(a["node"].as_str().unwrap().len(), 6);
    assert!(a["id"].as_str().unwrap().ends_with("/alice"));
    for k in ["activity", "last_ms"] {
        assert!(a.get(k).is_some(), "{k}");
    }
    for k in ["leases", "tasks", "events"] {
        assert!(v[k].is_array(), "{k}");
    }
    let e = &v["events"][0];
    for k in ["id", "lamport", "ts_ms", "kind", "from", "to", "text", "full", "ref"] {
        assert!(e.get(k).is_some(), "event.{k} in {e}");
    }
    assert_eq!(e["kind"], "join");
    assert_eq!(e["to"], Value::Null);
    assert_eq!(e["id"].as_str().unwrap().len(), 64, "full hex id");
    assert_eq!(e["full"], true);
}

#[test]
fn status_comes_from_presence() {
    let (d, room, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    d.set_presence(room, "alice", AgentStatus::Working).unwrap();
    assert_eq!(member(&feed(&d, 0, 50), "alice")["status"], "working");
}

#[test]
fn leases_tasks_and_events_follow_the_room() {
    let (d, _, now) = setup();
    ok(
        &d,
        "alice",
        Call::Claim {
            paths: vec!["src/net/**".into()],
            ttl_min: 10,
        },
    );
    ok(
        &d,
        "alice",
        Call::TaskAdd {
            task: "t-3".into(),
            title: "fix sync".into(),
            paths: vec![],
        },
    );
    ok(
        &d,
        "bob",
        Call::TaskAdd {
            task: "t-4".into(),
            title: "done thing".into(),
            paths: vec![],
        },
    );
    ok(
        &d,
        "bob",
        Call::TaskUpdate {
            task: "t-4".into(),
            status: "done".into(),
            note: None,
        },
    );
    ok(
        &d,
        "alice",
        Call::TaskUpdate {
            task: "t-3".into(),
            status: "in_progress".into(),
            note: None,
        },
    );
    let v = feed(&d, 0, 50);
    let l = &v["leases"][0];
    assert_eq!(l["paths"], json!(["src/net/**"]));
    assert!(l["holder"].as_str().unwrap().ends_with("/alice"));
    assert_eq!(l["state"], "pending");
    assert_eq!(l["expires_ms"], T0 + 600_000);
    assert_eq!(l["id"].as_str().unwrap().len(), 6);
    now.store(T0 + 5_000, Ordering::Relaxed);
    assert_eq!(feed(&d, 0, 50)["leases"][0]["state"], "granted");

    let tasks = v["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "done tasks are not open: {tasks:?}");
    assert_eq!(tasks[0]["id"], "t-3");
    assert_eq!(tasks[0]["title"], "fix sync");
    assert_eq!(tasks[0]["status"], "doing");
    assert!(tasks[0]["owner"].as_str().unwrap().ends_with("/alice"));

    let kinds: Vec<&str> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert!(kinds.contains(&"claim") && kinds.contains(&"task_add"));
    assert!(kinds.contains(&"task_update") && kinds.contains(&"join"));
    let tu = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "task_update")
        .unwrap();
    assert_eq!(tu["ref"], "t-4");
}

#[test]
fn activity_line_is_derived_from_leases_tasks_and_last_action() {
    let (d, _, now) = setup();
    ok(&d, "bob", Call::RoomCard);
    ok(
        &d,
        "alice",
        Call::Claim {
            paths: vec!["src/net/**".into()],
            ttl_min: 30,
        },
    );
    ok(
        &d,
        "alice",
        Call::TaskAdd {
            task: "t-3".into(),
            title: "fix sync".into(),
            paths: vec![],
        },
    );
    ok(
        &d,
        "alice",
        Call::TaskUpdate {
            task: "t-3".into(),
            status: "in_progress".into(),
            note: None,
        },
    );
    ok(
        &d,
        "bob",
        Call::Ask {
            to: "alice".into(),
            question: "ready?".into(),
        },
    );
    let ask_id = {
        let v = feed(&d, 0, 50);
        let e = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "ask")
            .unwrap()
            .clone();
        e["id"].as_str().unwrap().to_owned()
    };
    // full id needed for Answer; read it back from the store
    let full = {
        let (_, room) = (0, d.room_ids()[0]);
        let s = d.store_for_test(room);
        let g = s.lock().unwrap();
        let evs = g.events_since(Lamport(0)).unwrap();
        evs.iter()
            .find(|e| e.id.to_string().starts_with(&ask_id))
            .unwrap()
            .id
            .to_string()
    };
    now.store(T0 + 1_000, Ordering::Relaxed);
    ok(
        &d,
        "alice",
        Call::Answer {
            ask_id: full,
            text: "yes".into(),
        },
    );
    now.store(T0 + 1_000 + 120_000, Ordering::Relaxed);
    let v = feed(&d, 0, 50);
    assert_eq!(
        member(&v, "alice")["activity"],
        "holds src/net/** · t-3 doing · answered bob 2m ago"
    );
    // bob: nothing held, no task; last action is the ask
    let b = member(&v, "bob")["activity"].as_str().unwrap().to_owned();
    assert!(b.starts_with("asked alice "), "{b}");
    assert_eq!(member(&v, "alice")["last_ms"], T0 + 1_000);
}

#[test]
fn activity_without_any_action_says_so() {
    let (d, _, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    assert_eq!(
        member(&feed(&d, 0, 50), "alice")["activity"],
        "no activity yet"
    );
}

#[test]
fn cursor_pages_events_oldest_first() {
    let (d, _, _) = setup();
    for i in 0..5 {
        ok(
            &d,
            "alice",
            Call::Decide {
                title: format!("d{i}"),
                rationale: "r".into(),
                refs: vec![],
            },
        );
    }
    let all = feed(&d, 0, 200);
    let n = all["events"].as_array().unwrap().len();
    assert!(n >= 6, "{n}");
    // First load (since=0) shows the newest `limit` events, oldest first.
    let tail = feed(&d, 0, 3);
    let lam: Vec<u64> = tail["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["lamport"].as_u64().unwrap())
        .collect();
    assert_eq!(lam.len(), 3);
    assert!(lam.windows(2).all(|w| w[0] < w[1]));
    assert_eq!(tail["cursor"], all["cursor"]);
    // Nothing new after the cursor.
    let cur = all["cursor"].as_u64().unwrap();
    let none = feed(&d, cur, 200);
    assert_eq!(none["events"], json!([]));
    assert_eq!(none["cursor"], all["cursor"]);
    // Incremental: only newer events.
    ok(
        &d,
        "alice",
        Call::Decide {
            title: "new".into(),
            rationale: "r".into(),
            refs: vec![],
        },
    );
    let inc = feed(&d, cur, 200);
    assert_eq!(inc["events"].as_array().unwrap().len(), 1);
    assert_eq!(inc["events"][0]["text"], "new");
    // since>0 pages forward from the cursor, oldest first.
    let first = feed(&d, 1, 2);
    let l: Vec<u64> = first["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["lamport"].as_u64().unwrap())
        .collect();
    assert_eq!(l.len(), 2);
    assert_eq!(first["cursor"].as_u64().unwrap(), l[1]);
}

#[test]
fn limit_is_capped_at_200() {
    let (d, _, _) = setup();
    for i in 0..230 {
        ok(
            &d,
            "alice",
            Call::Decide {
                title: format!("d{i}"),
                rationale: "r".into(),
                refs: vec![],
            },
        );
    }
    assert_eq!(feed(&d, 0, 100_000)["events"].as_array().unwrap().len(), 200);
    assert_eq!(feed(&d, 0, 0)["events"].as_array().unwrap().len(), 1);
}

#[test]
fn peer_text_is_truncated_redacted_and_flattened() {
    let (d, room, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    // A peer's event is not scrubbed at its author, so Feed must do it.
    let peer = SigningKey::from_bytes(&[9; 32]);
    let store = d.store_for_test(room);
    let (seq, lam) = {
        let s = store.lock().unwrap();
        let n = hm_proto::NodeId::from_verifying_key(&peer.verifying_key());
        (s.next_seq(&n).unwrap(), s.next_lamport().unwrap())
    };
    let evil = format!(
        "<script>alert(1)</script>\u{1b}[31m\nline2 {TOKEN} {}",
        "x".repeat(500)
    );
    let ev = Event::sign(
        &peer,
        room,
        "mallory",
        seq,
        lam,
        T0,
        EventBody::Note {
            scope: "s".into(),
            text: evil,
        },
    )
    .unwrap();
    store.lock().unwrap().append(&ev).unwrap();
    let v = feed(&d, 0, 50);
    let e = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "note")
        .unwrap();
    let text = e["text"].as_str().unwrap();
    assert!(text.chars().count() <= 200, "{}", text.chars().count());
    assert!(text.contains("<script>"), "stays a plain string: {text}");
    assert!(!text.contains(TOKEN), "redacted");
    assert!(!text.contains('\u{1b}') && !text.contains('\n'));
    assert!(!member(&v, "mallory")["local"].as_bool().unwrap());
    assert!(!v.to_string().contains(TOKEN));
}

fn event_req(d: &Daemon, id: &str) -> Response {
    req(d, "viewer", Call::FeedEvent { id: id.into() })
}

#[test]
fn long_text_is_cut_in_the_feed_but_complete_in_feed_event() {
    let (d, room, _) = setup();
    let long = format!("start {}end", "w ".repeat(300));
    ok(
        &d,
        "alice",
        Call::Ask {
            to: "bob".into(),
            question: long.clone(),
        },
    );
    ok(
        &d,
        "alice",
        Call::Ask {
            to: "bob".into(),
            question: "short one".into(),
        },
    );
    let before = d.store_for_test(room).lock().unwrap().event_count().unwrap();
    let v = feed(&d, 0, 50);
    let asks: Vec<&Value> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "ask")
        .collect();
    assert_eq!(asks[0]["full"], false);
    assert!(asks[0]["text"].as_str().unwrap().chars().count() <= 200);
    assert_eq!(asks[1]["full"], true);
    let id = asks[0]["id"].as_str().unwrap();
    let Response::Ok(items) = event_req(&d, id) else {
        panic!("no event")
    };
    let e: Value = serde_json::from_str(&items[0].1).unwrap();
    assert_eq!(e["id"], id);
    assert_eq!(e["kind"], "ask");
    assert!(e["from"].as_str().unwrap().ends_with("/alice"));
    assert!(e["to"].as_str().unwrap().ends_with("/bob"));
    assert!(e["ts_ms"].is_u64());
    assert_eq!(e["refs"], json!([]));
    assert_eq!(e["text"].as_str().unwrap(), long);
    // unique prefix works; read-only
    assert!(matches!(event_req(&d, &id[..8]), Response::Ok(_)));
    assert_eq!(
        before,
        d.store_for_test(room).lock().unwrap().event_count().unwrap()
    );
}

#[test]
fn feed_event_unknown_or_malformed_is_not_found() {
    let (d, _, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    for id in ["", "zz", "abcdef", &"0".repeat(64), "not hex at all"] {
        assert!(matches!(event_req(&d, id), Response::Err(_)), "{id}");
    }
}

#[test]
fn feed_event_caps_at_8_kib_and_redacts_peer_text() {
    let (d, room, _) = setup();
    ok(&d, "alice", Call::RoomCard);
    let peer = SigningKey::from_bytes(&[9; 32]);
    let store = d.store_for_test(room);
    let (seq, lam) = {
        let s = store.lock().unwrap();
        let n = hm_proto::NodeId::from_verifying_key(&peer.verifying_key());
        (s.next_seq(&n).unwrap(), s.next_lamport().unwrap())
    };
    let body = format!("{TOKEN}\nline two {}", "é".repeat(20_000));
    let ev = Event::sign(
        &peer,
        room,
        "mallory",
        seq,
        lam,
        T0,
        EventBody::Note {
            scope: "s".into(),
            text: body,
        },
    )
    .unwrap();
    store.lock().unwrap().append(&ev).unwrap();
    let Response::Ok(items) = event_req(&d, &ev.id.to_string()) else {
        panic!("no event")
    };
    let e: Value = serde_json::from_str(&items[0].1).unwrap();
    let t = e["text"].as_str().unwrap();
    assert!(t.len() <= 8 * 1024, "{}", t.len());
    assert!(!t.contains(TOKEN));
    assert!(t.contains("\nline two"), "keeps line breaks");
}
