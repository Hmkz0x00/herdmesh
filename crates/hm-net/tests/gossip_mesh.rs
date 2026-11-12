//! Three in-process endpoints (relay disabled, direct loopback addrs) on one
//! room topic.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_net::{Net, NetConfig, RelayChoice, RoomEvent, RoomReceiver, RoomSecret, join_room};
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId};
use tokio::time::{Instant, timeout};

const DEADLINE: Duration = Duration::from_secs(10);

async fn bind(dir: &tempfile::TempDir, name: &str) -> Net {
    Net::bind(NetConfig {
        key_path: dir.path().join(name).join("node.key"),
        relay: RelayChoice::Disabled,
        allow_insecure_key: false,
    })
    .await
    .unwrap()
}

fn note(seed: u8, room: RoomId, text: &str) -> Event {
    Event::sign(
        &SigningKey::from_bytes(&[seed; 32]),
        room,
        format!("agent{seed}"),
        1,
        Lamport(1),
        1,
        EventBody::Note {
            scope: "test".into(),
            text: text.into(),
        },
    )
    .unwrap()
}

/// Read until `want` event ids are all seen (or the deadline passes).
async fn collect(
    rx: &mut RoomReceiver,
    want: &BTreeSet<EventId>,
    until: Instant,
) -> BTreeSet<EventId> {
    let mut got = BTreeSet::new();
    while !want.is_subset(&got) {
        let left = until.saturating_duration_since(Instant::now());
        match timeout(left, rx.next()).await {
            Ok(Some(Ok(RoomEvent::Event(ev)))) => {
                assert!(got.insert(ev.id), "duplicate yield of {:?}", ev.id);
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => panic!("receive error: {e}"),
            Ok(None) | Err(_) => break,
        }
    }
    got
}

#[tokio::test(flavor = "multi_thread")]
async fn three_nodes_exchange_events_and_drop_bad_ones() {
    let dir = tempfile::tempdir().unwrap();
    let secret = RoomSecret::from_bytes([5; 32]);
    let room = secret.room_id();
    let nets = [
        bind(&dir, "a").await,
        bind(&dir, "b").await,
        bind(&dir, "c").await,
    ];

    let (tx_a, mut rx_a) = join_room(&nets[0], &secret, vec![]).await.unwrap();
    let (tx_b, mut rx_b) = join_room(&nets[1], &secret, vec![nets[0].local_addr()])
        .await
        .unwrap();
    let (tx_c, mut rx_c) = join_room(
        &nets[2],
        &secret,
        vec![nets[0].local_addr(), nets[1].local_addr()],
    )
    .await
    .unwrap();
    // Wait for a full mesh (every node has both others as direct neighbors)
    // so the broadcasts below are eager-pushed to everyone.
    for rx in [&mut rx_a, &mut rx_b, &mut rx_c] {
        timeout(DEADLINE, rx.joined())
            .await
            .expect("join timed out")
            .unwrap();
        timeout(DEADLINE, async {
            while rx.neighbor_count() < 2 {
                match rx.next().await {
                    Some(Ok(RoomEvent::Event(ev))) => panic!("unexpected event {:?}", ev.id),
                    Some(Ok(_)) => {}
                    other => panic!("receiver ended: {other:?}"),
                }
            }
        })
        .await
        .expect("full mesh timed out");
    }

    // Bad traffic first, via a raw gossip handle on node a: B and C must drop it.
    let (raw, _raw_rx) = nets[0]
        .gossip()
        .subscribe(secret.topic_id(), vec![])
        .await
        .unwrap()
        .split();
    let mut tampered = note(9, room, "tampered");
    tampered.ts += 1;
    let foreign = note(9, RoomId::from_bytes([0xee; 32]), "foreign");
    raw.broadcast(tampered.encode().unwrap().into())
        .await
        .unwrap();
    raw.broadcast(foreign.encode().unwrap().into())
        .await
        .unwrap();

    let evs = [
        note(1, room, "from a"),
        note(2, room, "from b"),
        note(3, room, "from c"),
    ];
    tx_a.publish(&evs[0]).await.unwrap();
    tx_b.publish(&evs[1]).await.unwrap();
    tx_c.publish(&evs[2]).await.unwrap();

    let until = Instant::now() + DEADLINE;
    let all: BTreeSet<EventId> = evs.iter().map(|e| e.id).collect();
    for (i, rx) in [&mut rx_a, &mut rx_b, &mut rx_c].into_iter().enumerate() {
        // A node does not hear its own broadcast; its own event is local.
        let want: BTreeSet<EventId> = all.iter().copied().filter(|id| *id != evs[i].id).collect();
        let got = collect(rx, &want, until).await;
        assert_eq!(got, want, "node {i} missing events");
        assert!(!got.contains(&tampered.id) && !got.contains(&foreign.id));
    }

    // Drops are counted as the receiver is polled; keep draining until both
    // bad events have been seen and rejected.
    for (name, rx) in [("b", &mut rx_b), ("c", &mut rx_c)] {
        loop {
            let d = rx.drops();
            if d.bad_signature >= 1 && d.wrong_room >= 1 {
                break;
            }
            let left = until.saturating_duration_since(Instant::now());
            match timeout(left, rx.next()).await {
                Ok(Some(Ok(RoomEvent::Event(ev)))) => panic!("{name} yielded {:?}", ev.id),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => panic!("{name} never dropped bad events: {d:?}"),
            }
        }
    }

    for n in &nets {
        n.shutdown().await;
    }
}
