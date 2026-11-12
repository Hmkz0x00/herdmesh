//! Anti-entropy sync over `herdmesh/sync/1` between real iroh endpoints
//! (relay disabled, loopback).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_net::{
    ERR_REFUSED, MAX_REQUEST_FRAME, MAX_RESPONSE_FRAME, Net, NetConfig, Refusal, RelayChoice,
    RoomSecret, SYNC_ALPN, SharedStore, SyncError, SyncMsg, SyncSince, read_frame, sync_auth,
    sync_from, write_frame,
};
use hm_proto::{Event, EventBody, EventId, Lamport, RoomId, TaskId, TaskStatus};
use hm_store::{Store, SyncCaps, now_ms};
use iroh::endpoint::{Connection, ReadError, ReadExactError, VarInt, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode};

const T: Duration = Duration::from_secs(30);

fn secret() -> RoomSecret {
    RoomSecret::from_bytes([5; 32])
}

fn room() -> RoomId {
    secret().room_id()
}

fn key_from(seed: u8) -> SigningKey {
    SigningKey::from_bytes(blake3::hash(&[b'n', seed]).as_bytes())
}

fn store() -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(room()).unwrap()))
}

async fn bind(dir: &tempfile::TempDir, name: &str) -> Net {
    Net::bind(NetConfig {
        key_path: dir.path().join(name).join("node.key"),
        relay: RelayChoice::Disabled,
    })
    .await
    .unwrap()
}

/// A mixed workload: notes, tasks and lease churn from three authors.
fn workload(n: u64) -> Vec<Event> {
    let keys: Vec<SigningKey> = (0..3).map(key_from).collect();
    (1..=n)
        .map(|i| {
            let k = &keys[(i % 3) as usize];
            let tid = TaskId::new(format!("t{}", i % 7)).unwrap();
            let body = match i % 5 {
                0 => EventBody::TaskAdd {
                    task: tid,
                    title: format!("task {i}"),
                    paths: vec![],
                },
                1 => EventBody::TaskUpdate {
                    task: tid,
                    status: TaskStatus::InProgress,
                    note: None,
                },
                2 => EventBody::LeaseClaim {
                    paths: vec![format!("src/{}.rs", i % 4)],
                    ttl_secs: 60,
                },
                3 => EventBody::LeaseRelease {
                    claim: EventId::from_bytes([(i % 4) as u8; 32]),
                },
                _ => EventBody::Note {
                    scope: "s".into(),
                    text: format!("note {i}"),
                },
            };
            // Lamport roughly i/2 so authors interleave and tie.
            Event::sign(k, room(), "ag", i, Lamport(i / 2 + 1), 1_000 + i, body).unwrap()
        })
        .collect()
}

fn fill(s: &SharedStore, events: &[Event]) {
    let r = s.lock().unwrap().append_batch_at(events, 10_000).unwrap();
    assert_eq!(r.rejected, 0);
}

fn snapshot(s: &SharedStore) -> Vec<u8> {
    postcard::to_allocvec(s.lock().unwrap().views()).unwrap()
}

fn count(s: &SharedStore) -> u64 {
    s.lock().unwrap().event_count().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_node_catches_up_to_500_events() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    let (sa, sb) = (store(), store());
    fill(&sa, &workload(500));
    a.sync_rooms().insert(secret(), sa.clone()).unwrap();
    // Small caps force several request rounds over one connection.
    a.sync_rooms().set_caps(SyncCaps {
        max_events: 64,
        max_bytes: 8 << 20,
    });

    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.inserted, 500);
    assert_eq!(stats.rejected, 0);
    assert!(stats.rounds >= 8, "{stats:?}");
    assert_eq!(count(&sb), 500);
    // Views are pure over the signed event set (ADR-002), so they match.
    sb.lock().unwrap().rebuild_views().unwrap();
    let a_views = sa.lock().unwrap().views().clone();
    let b_views = sb.lock().unwrap().views().clone();
    assert_eq!(a_views, b_views);
    assert_eq!(snapshot(&sa), snapshot(&sb));

    // Second sync transfers nothing.
    let again = sync_from(b.endpoint(), a.local_addr(), &secret(), &sb)
        .await
        .unwrap();
    assert_eq!(again.received, 0);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_overlap_transfers_only_missing() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    let (sa, sb) = (store(), store());
    let mut all = workload(300);
    all.sort_by_key(|e| (e.lamport, e.author.node, e.id));
    fill(&sa, &all);
    fill(&sb, &all[..200]);
    // B also has events A never saw; they must not be sent back or lost.
    let own: Vec<Event> = (1..=10)
        .map(|l| {
            Event::sign(
                &key_from(9),
                room(),
                "b",
                l,
                Lamport(l),
                1,
                EventBody::Leave,
            )
            .unwrap()
        })
        .collect();
    fill(&sb, &own);
    a.sync_rooms().insert(secret(), sa.clone()).unwrap();

    let stats = sync_from(b.endpoint(), a.local_addr(), &secret(), &sb)
        .await
        .unwrap();
    assert_eq!(stats.received, 100);
    assert_eq!(stats.inserted, 100);
    assert_eq!(stats.duplicates, 0);
    assert_eq!(count(&sb), 310);
    assert_eq!(count(&sa), 300);
    a.shutdown().await;
    b.shutdown().await;
}

/// Send one hand-built request; return (event frames received, outcome).
async fn raw_request(
    from: &Net,
    to: EndpointAddr,
    req: &SyncSince,
) -> (usize, Result<(), SyncError>) {
    let conn = from.endpoint().connect(to, SYNC_ALPN).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    write_frame(&mut send, req).await.unwrap();
    send.finish().unwrap();
    let mut events = 0;
    let out = loop {
        match read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME).await {
            Ok(SyncMsg::Event(_)) => events += 1,
            Ok(SyncMsg::End { .. }) => break Ok(()),
            Ok(SyncMsg::Refused(r)) => break Err(SyncError::Refused(r)),
            Err(e) => break Err(e),
        }
    };
    conn.close(0u32.into(), b"done");
    (events, out)
}

fn is_reset_refused(r: &Result<(), SyncError>) -> bool {
    matches!(
        r,
        Err(SyncError::Read(ReadExactError::ReadError(ReadError::Reset(c))))
            if *c == VarInt::from_u32(ERR_REFUSED)
    )
}

/// Server `a` serving `n` events of `secret()`'s room.
async fn serving(dir: &tempfile::TempDir, n: u64) -> Net {
    let a = bind(dir, "a").await;
    let sa = store();
    fill(&sa, &workload(n));
    a.sync_rooms().insert(secret(), sa).unwrap();
    a
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_room_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    let other_secret = RoomSecret::from_bytes([6; 32]);
    let other = Arc::new(Mutex::new(
        Store::open_in_memory(other_secret.room_id()).unwrap(),
    ));
    a.sync_rooms().insert(other_secret, other).unwrap();
    let err = sync_from(b.endpoint(), a.local_addr(), &secret(), &store())
        .await
        .unwrap_err();
    assert!(
        matches!(err, SyncError::Refused(Refusal::Unauthorized)),
        "{err:?}"
    );
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_secret_gets_no_events() {
    let dir = tempfile::tempdir().unwrap();
    let a = serving(&dir, 20).await;
    let b = bind(&dir, "b").await;
    // Right room id (public), but the tag is made with a guessed secret.
    let wrong = RoomSecret::from_bytes([7; 32]);
    let req = SyncSince {
        room: room(),
        have: vec![],
        auth: sync_auth(&wrong, b.endpoint().id(), a.endpoint().id(), room()),
    };
    let (events, out) = tokio::time::timeout(T, raw_request(&b, a.local_addr(), &req))
        .await
        .unwrap();
    assert_eq!(events, 0);
    assert!(is_reset_refused(&out), "{out:?}");

    // Through the client API it surfaces as Refused.
    let sb = Arc::new(Mutex::new(Store::open_in_memory(wrong.room_id()).unwrap()));
    let err = sync_from(b.endpoint(), a.local_addr(), &wrong, &sb)
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::Refused(_)), "{err:?}");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn replayed_request_from_other_endpoint_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let a = serving(&dir, 20).await;
    let (b, c) = (bind(&dir, "b").await, bind(&dir, "c").await);
    // A valid request captured from b...
    let req = SyncSince {
        room: room(),
        have: vec![],
        auth: sync_auth(&secret(), b.endpoint().id(), a.endpoint().id(), room()),
    };
    // ...works for b,
    let (events, out) = raw_request(&b, a.local_addr(), &req).await;
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(events, 20);
    // ...but not when c replays it byte for byte.
    let (events, out) = tokio::time::timeout(T, raw_request(&c, a.local_addr(), &req))
        .await
        .unwrap();
    assert_eq!(events, 0);
    assert!(is_reset_refused(&out), "{out:?}");
    a.shutdown().await;
    b.shutdown().await;
    c.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn oversized_request_is_rejected_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let a = serving(&dir, 5).await;
    let b = bind(&dir, "b").await;
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    // Announce a ~4 GiB request but send no payload: a server that tried to
    // allocate or read it would hang here instead of refusing at once.
    send.write_all(&u32::MAX.to_be_bytes()).await.unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME),
    )
    .await
    .expect("server did not refuse promptly")
    .map(|_| ());
    assert!(is_reset_refused(&out), "{out:?}");

    // Just over the request limit is refused too.
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let len = u32::try_from(MAX_REQUEST_FRAME + 1).unwrap();
    send.write_all(&len.to_be_bytes()).await.unwrap();
    let out = tokio::time::timeout(
        Duration::from_secs(10),
        read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME),
    )
    .await
    .expect("server did not refuse promptly")
    .map(|_| ());
    assert!(is_reset_refused(&out), "{out:?}");
    conn.close(0u32.into(), b"done");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn secret_must_match_store_room() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    let other = RoomSecret::from_bytes([8; 32]);
    assert!(matches!(
        a.sync_rooms().insert(other.clone(), store()),
        Err(SyncError::RoomMismatch)
    ));
    a.sync_rooms().insert(secret(), store()).unwrap();
    let err = sync_from(b.endpoint(), a.local_addr(), &other, &store())
        .await
        .unwrap_err();
    assert!(matches!(err, SyncError::RoomMismatch), "{err:?}");
    a.shutdown().await;
    b.shutdown().await;
}

/// ADR-002 skew guard: a far-future event is not stored by the receiver
/// (it waits in the bounded pending queue instead).
#[tokio::test(flavor = "multi_thread")]
async fn future_dated_claim_is_not_stored_by_receiver() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (bind(&dir, "a").await, bind(&dir, "b").await);
    let (sa, sb) = (store(), store());
    let far_future = now_ms() + 30 * 24 * 3_600_000;
    let claim = Event::sign(
        &key_from(1),
        room(),
        "evil",
        1,
        Lamport(1),
        far_future,
        EventBody::LeaseClaim {
            paths: vec!["src/**".into()],
            ttl_secs: 5,
        },
    )
    .unwrap();
    // A's clock agrees with the claim, so A stores and serves it.
    let r = sa
        .lock()
        .unwrap()
        .append_batch_at(std::slice::from_ref(&claim), far_future)
        .unwrap();
    assert_eq!(r.inserted, 1);
    a.sync_rooms().insert(secret(), sa.clone()).unwrap();

    sync_from(b.endpoint(), a.local_addr(), &secret(), &sb)
        .await
        .unwrap();
    {
        let s = sb.lock().unwrap();
        assert_eq!(s.event_count().unwrap(), 0);
        assert_eq!(s.pending_len(), 1);
        assert_eq!(s.views().active_leases(far_future).count(), 0);
    }
    a.shutdown().await;
    b.shutdown().await;
}

/// Server that answers with good events plus one tampered event.
#[derive(Debug, Clone)]
struct Evil {
    good: Vec<Event>,
    bad: Event,
}

impl ProtocolHandler for Evil {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let io = |e: SyncError| AcceptError::from_err(std::io::Error::other(e.to_string()));
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let _req: SyncSince = read_frame(&mut recv, MAX_REQUEST_FRAME).await.map_err(io)?;
            for e in &self.good[..2] {
                write_frame(&mut send, &SyncMsg::Event(e.encode().unwrap()))
                    .await
                    .map_err(io)?;
            }
            write_frame(&mut send, &SyncMsg::Event(self.bad.encode().unwrap()))
                .await
                .map_err(io)?;
            // Undecodable garbage is rejected too.
            write_frame(&mut send, &SyncMsg::Event(vec![1, 2, 3]))
                .await
                .map_err(io)?;
            for e in &self.good[2..] {
                write_frame(&mut send, &SyncMsg::Event(e.encode().unwrap()))
                    .await
                    .map_err(io)?;
            }
            write_frame(&mut send, &SyncMsg::End { more: false })
                .await
                .map_err(io)?;
            send.finish().ok();
        }
        Ok(())
    }
}

fn loopback(ep: &Endpoint) -> EndpointAddr {
    ep.bound_sockets()
        .into_iter()
        .map(|s| {
            let ip = match s.ip() {
                IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::LOCALHOST),
            };
            SocketAddr::new(ip, s.port())
        })
        .fold(EndpointAddr::new(ep.id()), |a, s| a.with_ip_addr(s))
}

#[tokio::test(flavor = "multi_thread")]
async fn tampered_event_from_malicious_server_is_rejected_others_kept() {
    let dir = tempfile::tempdir().unwrap();
    let b = bind(&dir, "b").await;
    let sb = store();
    let good = workload(5);
    let mut bad = workload(6).pop().unwrap();
    bad.lamport = Lamport(bad.lamport.0 + 1); // signature no longer matches

    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    let router = Router::builder(ep)
        .accept(SYNC_ALPN, Evil { good, bad })
        .spawn();

    let stats = tokio::time::timeout(
        T,
        sync_from(b.endpoint(), loopback(router.endpoint()), &secret(), &sb),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(stats.received, 7);
    assert_eq!(stats.inserted, 5);
    assert_eq!(stats.rejected, 2);
    assert_eq!(count(&sb), 5);
    router.shutdown().await.unwrap();
    b.shutdown().await;
}
