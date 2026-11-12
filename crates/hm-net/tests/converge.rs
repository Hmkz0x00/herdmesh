//! Integration test: 3 in-process nodes converge across gossip and anti-entropy sync.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_net::{
    Net, NetConfig, RelayChoice, RoomEvent, RoomReceiver, RoomSecret, RoomSender, SharedStore,
    SyncError, join_room, sync_from,
};
use hm_proto::{Event, EventBody, Lamport, RoomId, TaskId};
use hm_store::Store;
use tokio::time::{Instant, timeout};

const MESH_TIMEOUT: Duration = Duration::from_secs(10);
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(20);

fn secret() -> RoomSecret {
    RoomSecret::from_bytes([42; 32])
}

fn key_from(seed: u8) -> SigningKey {
    let mut h = blake3::Hasher::new();
    h.update(b"converge-author");
    h.update(&[seed]);
    SigningKey::from_bytes(h.finalize().as_bytes())
}

fn store(room: RoomId) -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(room).unwrap()))
}

async fn bind(dir: &tempfile::TempDir, name: &str) -> Net {
    Net::bind(NetConfig {
        key_path: dir.path().join(name).join("node.key"),
        relay: RelayChoice::Disabled,
    })
    .await
    .unwrap()
}

fn spawn_receiver(mut rx: RoomReceiver, store: SharedStore) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(event)) = rx.next().await {
            if let RoomEvent::Event(ev) = event {
                let _ = store.lock().unwrap().append(&ev);
            }
        }
    })
}

fn make_event(key: &SigningKey, room: RoomId, author: &str, lamport: u64, i: u64) -> Event {
    let tid = TaskId::new(format!("t-{author}-{i}")).unwrap();
    let body = match i % 4 {
        0 => EventBody::TaskAdd {
            task: tid,
            title: format!("Task {i} by {author}"),
            paths: vec![format!("src/{author}_{i}.rs")],
        },
        1 => EventBody::LeaseClaim {
            paths: vec![format!("src/{author}_{i}.rs")],
            ttl_secs: 120,
        },
        // Releases the claim made by the previous event (same author).
        2 => EventBody::LeaseRelease {
            claim: make_event(key, room, author, lamport - 1, i - 1).id,
        },
        _ => EventBody::Note {
            scope: "converge".into(),
            text: format!("Note {i} from {author}"),
        },
    };
    Event::sign(key, room, author, i + 1, Lamport(lamport), 1_000 + i, body).unwrap()
}

async fn publish_event(tx: &RoomSender, store: &SharedStore, ev: &Event) {
    store.lock().unwrap().append(ev).unwrap();
    tx.publish(ev).await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn three_nodes_converge_across_gossip_and_sync() {
    let dir = tempfile::tempdir().unwrap();
    let room_sec = secret();
    let room = room_sec.room_id();

    let key_a = key_from(1);
    let key_b = key_from(2);
    let key_c = key_from(3);

    // 1. Nodes A and B start up and register sync rooms.
    let net_a = bind(&dir, "a").await;
    let net_b = bind(&dir, "b").await;
    let store_a = store(room);
    let store_b = store(room);
    net_a
        .sync_rooms()
        .insert(room_sec.clone(), store_a.clone())
        .unwrap();
    net_b
        .sync_rooms()
        .insert(room_sec.clone(), store_b.clone())
        .unwrap();

    let (tx_a, mut rx_a) = join_room(&net_a, &room_sec, vec![]).await.unwrap();
    let (tx_b, mut rx_b) = join_room(&net_b, &room_sec, vec![net_a.local_addr()])
        .await
        .unwrap();

    // Ensure A and B see each other on gossip before publishing.
    timeout(MESH_TIMEOUT, rx_b.joined())
        .await
        .expect("B join timed out")
        .unwrap();
    timeout(MESH_TIMEOUT, rx_a.joined())
        .await
        .expect("A join timed out")
        .unwrap();

    let rx_handle_a = spawn_receiver(rx_a, store_a.clone());
    let rx_handle_b = spawn_receiver(rx_b, store_b.clone());

    // 2. Nodes A and B exchange 20 signed events each via gossip.
    for i in 1..=20 {
        let ev_a = make_event(&key_a, room, "agent-a", i, i);
        publish_event(&tx_a, &store_a, &ev_a).await;

        let ev_b = make_event(&key_b, room, "agent-b", i, i);
        publish_event(&tx_b, &store_b, &ev_b).await;

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Wait until both A and B hold all 40 events.
    let phase1_deadline = Instant::now() + Duration::from_secs(10);
    while store_a.lock().unwrap().event_count().unwrap() < 40
        || store_b.lock().unwrap().event_count().unwrap() < 40
    {
        assert!(
            Instant::now() < phase1_deadline,
            "timed out waiting for A and B to reach 40 events: A={}, B={}",
            store_a.lock().unwrap().event_count().unwrap(),
            store_b.lock().unwrap().event_count().unwrap()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(store_a.lock().unwrap().event_count().unwrap(), 40);
    assert_eq!(store_b.lock().unwrap().event_count().unwrap(), 40);

    // 3. Node C (late joiner) starts, syncs from A, then joins gossip.
    let net_c = bind(&dir, "c").await;
    let store_c = store(room);
    net_c
        .sync_rooms()
        .insert(room_sec.clone(), store_c.clone())
        .unwrap();

    let sync_stats = sync_from(net_c.endpoint(), net_a.local_addr(), &room_sec, &store_c)
        .await
        .unwrap();
    assert_eq!(sync_stats.inserted, 40);
    assert_eq!(store_c.lock().unwrap().event_count().unwrap(), 40);

    let (tx_c, mut rx_c) = join_room(
        &net_c,
        &room_sec,
        vec![net_a.local_addr(), net_b.local_addr()],
    )
    .await
    .unwrap();

    // Wait until C has joined and established connections with both peers.
    timeout(MESH_TIMEOUT, rx_c.joined())
        .await
        .expect("C join timed out")
        .unwrap();

    timeout(MESH_TIMEOUT, async {
        while rx_c.neighbor_count() < 2 {
            match rx_c.next().await {
                Some(Ok(RoomEvent::Event(ev))) => {
                    let _ = store_c.lock().unwrap().append(&ev);
                }
                Some(Ok(_)) => {}
                other => panic!("receiver ended unexpectedly: {other:?}"),
            }
        }
    })
    .await
    .expect("C mesh formation timed out");

    let rx_handle_c = spawn_receiver(rx_c, store_c.clone());
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 4. Each node publishes 5 more events (total: 40 + 15 = 55).
    for i in 1..=5 {
        let ev_a = make_event(&key_a, room, "agent-a", 20 + i, 20 + i);
        publish_event(&tx_a, &store_a, &ev_a).await;

        let ev_b = make_event(&key_b, room, "agent-b", 20 + i, 20 + i);
        publish_event(&tx_b, &store_b, &ev_b).await;

        let ev_c = make_event(&key_c, room, "agent-c", 20 + i, 20 + i);
        publish_event(&tx_c, &store_c, &ev_c).await;

        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // 5. Assert: all three stores end with identical event counts (55) within 20s.
    let converge_deadline = Instant::now() + CONVERGE_TIMEOUT;
    while Instant::now() < converge_deadline {
        let ca = store_a.lock().unwrap().event_count().unwrap();
        let cb = store_b.lock().unwrap().event_count().unwrap();
        let cc = store_c.lock().unwrap().event_count().unwrap();
        if ca == 55 && cb == 55 && cc == 55 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let count_a = store_a.lock().unwrap().event_count().unwrap();
    let count_b = store_b.lock().unwrap().event_count().unwrap();
    let count_c = store_c.lock().unwrap().event_count().unwrap();
    assert_eq!(count_a, 55, "node A event count mismatch");
    assert_eq!(count_b, 55, "node B event count mismatch");
    assert_eq!(count_c, 55, "node C event count mismatch");

    // Assert: identical serialized Views.
    let views_a = postcard::to_allocvec(store_a.lock().unwrap().views()).unwrap();
    let views_b = postcard::to_allocvec(store_b.lock().unwrap().views()).unwrap();
    let views_c = postcard::to_allocvec(store_c.lock().unwrap().views()).unwrap();
    assert_eq!(views_a, views_b, "Views A and B differ");
    assert_eq!(views_b, views_c, "Views B and C differ");

    // 6. Assert: a 4th node with the WRONG secret gets nothing from sync.
    let net_d = bind(&dir, "d").await;
    let wrong_sec = RoomSecret::from_bytes([99; 32]);
    let store_d = store(wrong_sec.room_id());

    let sync_err = sync_from(net_d.endpoint(), net_a.local_addr(), &wrong_sec, &store_d)
        .await
        .unwrap_err();
    assert!(
        matches!(sync_err, SyncError::Refused(_)),
        "expected SyncError::Refused, got {sync_err:?}"
    );
    assert_eq!(
        store_d.lock().unwrap().event_count().unwrap(),
        0,
        "unauthorized node must receive 0 events"
    );

    // Clean up background tasks and endpoints.
    rx_handle_a.abort();
    rx_handle_b.abort();
    rx_handle_c.abort();

    net_a.shutdown().await;
    net_b.shutdown().await;
    net_c.shutdown().await;
    net_d.shutdown().await;
}
