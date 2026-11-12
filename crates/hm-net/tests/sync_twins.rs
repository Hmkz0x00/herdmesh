//! Twin serving (ADR-002 addendum): a node that kept the higher-id twin of
//! an equivocation converges by sync to the lowest id and records the mark.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_net::{Net, NetConfig, RelayChoice, RoomSecret, SharedStore, sync_from};
use hm_proto::{Event, EventBody, Lamport, RoomId};
use hm_store::Store;

const T: Duration = Duration::from_secs(30);

fn secret() -> RoomSecret {
    RoomSecret::from_bytes([13; 32])
}

fn room() -> RoomId {
    secret().room_id()
}

fn store() -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(room()).unwrap()))
}

fn note(k: &SigningKey, seq: u64, text: &str) -> Event {
    Event::sign(
        k,
        room(),
        "ag",
        seq,
        Lamport(seq),
        1_000 + seq,
        EventBody::Note {
            scope: "s".into(),
            text: text.into(),
        },
    )
    .unwrap()
}

fn fill(s: &SharedStore, events: &[Event]) {
    s.lock().unwrap().append_batch_at(events, 10_000).unwrap();
}

async fn bind(dir: &tempfile::TempDir, name: &str) -> Net {
    Net::bind(NetConfig {
        key_path: dir.path().join(name).join("node.key"),
        relay: RelayChoice::Disabled,
        allow_insecure_key: false,
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn holder_of_higher_twin_converges_to_lowest_and_marks_author() {
    let dir = tempfile::tempdir().unwrap();
    let k = SigningKey::from_bytes(&[21; 32]);
    let honest = SigningKey::from_bytes(&[22; 32]);
    let (t1, t2) = (note(&k, 2, "twin one"), note(&k, 2, "twin two"));
    let (low, high) = if t1.id < t2.id { (t1, t2) } else { (t2, t1) };
    let common = [
        note(&k, 1, "before"),
        note(&k, 3, "after"),
        note(&honest, 1, "other"),
    ];

    // B saw both twins: keeps the lowest, marks the author.
    let sb = store();
    fill(&sb, &common);
    fill(&sb, &[high.clone(), low.clone()]);
    assert_eq!(sb.lock().unwrap().equivocations().unwrap().len(), 1);

    // A only ever saw the higher twin: its seq summary is complete.
    let sa = store();
    fill(&sa, &common);
    fill(&sa, std::slice::from_ref(&high));
    assert!(sa.lock().unwrap().equivocations().unwrap().is_empty());

    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    b.sync_rooms().insert(secret(), sb.clone()).unwrap();
    let stats = tokio::time::timeout(T, sync_from(a.endpoint(), b.local_addr(), &secret(), &sa))
        .await
        .unwrap()
        .unwrap();
    assert!(stats.equivocations >= 1, "{stats:?}");

    let (va, vb) = (
        sa.lock().unwrap().views().clone(),
        sb.lock().unwrap().views().clone(),
    );
    assert_eq!(va, vb);
    let marks = sa.lock().unwrap().equivocations().unwrap();
    assert_eq!(marks.len(), 1, "{marks:?}");
    assert_eq!(marks[0].node, low.author.node);
    assert_eq!(marks[0].seq, 2);
    assert_eq!(marks[0].kept, low.id);
    assert_eq!(marks[0].dropped, high.id);

    // Converged: a second sync changes nothing.
    let again = sync_from(a.endpoint(), b.local_addr(), &secret(), &sa)
        .await
        .unwrap();
    assert_eq!(again.inserted, 0, "{again:?}");
    a.shutdown().await;
    b.shutdown().await;
}
