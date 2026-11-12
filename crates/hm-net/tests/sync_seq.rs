//! Per-author seq reconciliation (ADR-002 §3, sync wire v2).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_net::AuthorWant;
use hm_net::sync::SYNC_VERSION;
use hm_net::{
    ERR_REFUSED, MAX_AUTHORS_PER_REQUEST, MAX_HOLES_PER_AUTHOR, MAX_REQUEST_FRAME,
    MAX_RESPONSE_FRAME, Net, NetConfig, RelayChoice, RoomSecret, SYNC_ALPN, SharedStore, SyncError,
    SyncMsg, SyncSince, read_frame, request_page, sync_auth, sync_from, write_frame,
};
use hm_proto::{Event, EventBody, Lamport, NodeId, RoomId};
use hm_store::{AuthorSeqs, Store};
use iroh::endpoint::{Connection, ReadError, ReadExactError, VarInt, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode};

const T: Duration = Duration::from_secs(60);

fn secret() -> RoomSecret {
    RoomSecret::from_bytes([11; 32])
}

fn room() -> RoomId {
    secret().room_id()
}

fn key(n: u32) -> SigningKey {
    SigningKey::from_bytes(blake3::hash(&n.to_le_bytes()).as_bytes())
}

fn store() -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(room()).unwrap()))
}

fn ev(k: &SigningKey, seq: u64, lamport: u64, text: &str) -> Event {
    Event::sign(
        k,
        room(),
        "ag",
        seq,
        Lamport(lamport),
        1_000 + seq,
        EventBody::Note {
            scope: "s".into(),
            text: text.into(),
        },
    )
    .unwrap()
}

/// Seqs 1..=n of one author, lamport = seq.
fn chain(k: &SigningKey, n: u64) -> Vec<Event> {
    (1..=n).map(|i| ev(k, i, i, &format!("e{i}"))).collect()
}

fn fill(s: &SharedStore, events: &[Event]) {
    let r = s.lock().unwrap().append_batch_at(events, 10_000).unwrap();
    assert_eq!(r.rejected, 0, "{r:?}");
}

fn ids(s: &SharedStore) -> Vec<hm_proto::EventId> {
    let mut v: Vec<_> = s
        .lock()
        .unwrap()
        .events_since(Lamport(0))
        .unwrap()
        .into_iter()
        .map(|e| e.id)
        .collect();
    v.sort();
    v
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

async fn serve(dir: &tempfile::TempDir, events: &[Event]) -> (Net, SharedStore) {
    let a = bind(dir, "a").await;
    let sa = store();
    fill(&sa, events);
    a.sync_rooms().insert(secret(), sa.clone()).unwrap();
    (a, sa)
}

#[tokio::test(flavor = "multi_thread")]
async fn withheld_and_out_of_order_events_are_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(1);
    let all = chain(&k, 10);
    let (a, sa) = serve(&dir, &all).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    // Gossip delivered 7 before 3, and 3, 6 and 8..10 were withheld.
    let got: Vec<Event> = [7u64, 1, 5, 2, 4]
        .iter()
        .map(|s| all[*s as usize - 1].clone())
        .collect();
    fill(&sb, &got);
    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.inserted, 5, "{stats:?}");
    assert_eq!(stats.duplicates, 0, "only the missing seqs are sent");
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn many_holes_beyond_one_request_are_all_filled() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(2);
    let all = chain(&k, 100);
    let (a, sa) = serve(&dir, &all).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    // Only the first and last: 98 holes, far more than one request lists.
    fill(&sb, &[all[0].clone(), all[99].clone()]);
    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.inserted, 98, "{stats:?}");
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn holes_nobody_has_do_not_block_newer_events() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(3);
    let all = chain(&k, 200);
    // The server lacks seqs 2..=40 too (39 holes: more than two windows).
    let served: Vec<Event> = all
        .iter()
        .filter(|e| !(2..=40).contains(&e.seq))
        .cloned()
        .collect();
    let (a, _sa) = serve(&dir, &served).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    fill(&sb, &[all[0].clone(), all[49].clone()]);
    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    // 41..=49 and 51..=200.
    assert_eq!(stats.inserted, 9 + 150, "{stats:?}");
    assert_eq!(sb.lock().unwrap().event_count().unwrap(), 161);
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn same_lamport_siblings_are_not_skipped() {
    let dir = tempfile::tempdir().unwrap();
    let (k1, k2) = (key(4), key(5));
    // Same author, two events at lamport 5; another author also at 5.
    let s1 = ev(&k1, 1, 5, "first");
    let s2 = ev(&k1, 2, 5, "sibling");
    let other = ev(&k2, 1, 5, "other");
    let (a, sa) = serve(&dir, &[s1.clone(), s2, other]).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    // A lamport-vector protocol would say "have k1 up to lamport 5".
    fill(&sb, &[s1]);
    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.inserted, 2, "{stats:?}");
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn thousand_authors_sync_via_pagination() {
    let dir = tempfile::tempdir().unwrap();
    // Signing is slow in debug builds: sign on all cores.
    let all: Vec<Event> = std::thread::scope(|sc| {
        let parts: Vec<_> = (0..8u32)
            .map(|t| {
                sc.spawn(move || {
                    (100 + t * 125..100 + (t + 1) * 125)
                        .flat_map(|n| chain(&key(n), 2))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        parts.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    // B knows every author but only their first event; A serves the second
    // ones (its own hole at seq 1 doesn't matter).
    let (firsts, seconds): (Vec<Event>, Vec<Event>) = all.into_iter().partition(|e| e.seq == 1);
    let (a, sa) = serve(&dir, &seconds).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    fill(&sb, &firsts);
    let stats = tokio::time::timeout(T, sync_from(b.endpoint(), a.local_addr(), &secret(), &sb))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stats.inserted, 1000, "{stats:?}");
    assert_eq!(stats.duplicates, 0, "{stats:?}");
    assert!(
        stats.rounds >= 1000_usize.div_ceil(MAX_AUTHORS_PER_REQUEST),
        "not paginated: {stats:?}"
    );
    assert_eq!(sb.lock().unwrap().event_count().unwrap(), 2000);
    assert_eq!(sa.lock().unwrap().event_count().unwrap(), 1000);
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn worst_case_request_page_fits_the_request_frame() {
    // 1000 authors, each with the max holes at huge seqs.
    let summary: BTreeMap<NodeId, AuthorSeqs> = (0..1000u32)
        .map(|i| {
            let mut b = [0u8; 32];
            b[..4].copy_from_slice(&i.to_be_bytes());
            let holes = (0..200).map(|h| u64::MAX - 1000 + h).collect();
            (
                NodeId::from_bytes(b),
                AuthorSeqs {
                    hi: u64::MAX - 1001,
                    max: u64::MAX - 1,
                    holes,
                },
            )
        })
        .collect();
    let mut from = None;
    let mut pages = 0;
    loop {
        let page = request_page(&summary, from, 3);
        assert!(page.want.len() <= MAX_AUTHORS_PER_REQUEST);
        assert!(
            page.want
                .iter()
                .all(|w| w.holes.len() <= MAX_HOLES_PER_AUTHOR)
        );
        let req = SyncSince {
            room: room(),
            from,
            to: page.to,
            want: page.want,
            auth: [0; 32],
        };
        let len = 1 + postcard::to_allocvec(&req).unwrap().len();
        assert!(len <= MAX_REQUEST_FRAME, "page of {len} bytes");
        pages += 1;
        match page.to {
            Some(t) => from = Some(t),
            None => break,
        }
    }
    assert_eq!(pages, 1000_usize.div_ceil(MAX_AUTHORS_PER_REQUEST));
}

#[test]
fn hole_windows_cover_all_listed_holes() {
    let author = NodeId::from_bytes([1; 32]);
    let holes: Vec<u64> = (2..=40).collect();
    let summary = BTreeMap::from([(
        author,
        AuthorSeqs {
            hi: 1,
            max: 50,
            holes: holes.clone(),
        },
    )]);
    let first = request_page(&summary, None, 0);
    assert_eq!(first.windows, 3);
    let mut seen = Vec::new();
    for w in 0..first.windows {
        let p = request_page(&summary, None, w);
        assert_eq!(p.want[0].have_hi, 50, "tail always requested");
        seen.extend(p.want[0].holes.clone());
    }
    assert_eq!(seen, holes);
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

/// Server that answers every request with two different events signed for
/// the same (author, seq).
#[derive(Debug, Clone)]
struct Equivocator(Vec<Event>);

impl ProtocolHandler for Equivocator {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        let io = |e: SyncError| AcceptError::from_err(std::io::Error::other(e.to_string()));
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let _req: SyncSince = read_frame(&mut recv, MAX_REQUEST_FRAME).await.map_err(io)?;
            for e in &self.0 {
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

#[tokio::test(flavor = "multi_thread")]
async fn equivocating_twins_keep_lowest_id_and_are_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(6);
    let x = ev(&k, 1, 1, "x");
    let y = ev(&k, 1, 1, "y");
    let low = x.id.min(y.id);
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    let router = Router::builder(ep)
        .accept(SYNC_ALPN, Equivocator(vec![x, y]))
        .spawn();
    let b = bind(&dir, "b").await;
    let sb = store();
    let stats = tokio::time::timeout(
        T,
        sync_from(b.endpoint(), loopback(router.endpoint()), &secret(), &sb),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(stats.equivocations >= 1, "{stats:?}");
    assert_eq!(ids(&sb), vec![low]);
    let marks = sb.lock().unwrap().equivocations().unwrap();
    assert_eq!(marks.len(), 1);
    assert_eq!(marks[0].kept, low);
    router.shutdown().await.unwrap();
    b.shutdown().await;
}

fn is_reset_refused(r: &Result<SyncMsg, SyncError>) -> bool {
    matches!(
        r,
        Err(SyncError::Read(ReadExactError::ReadError(ReadError::Reset(c))))
            if *c == VarInt::from_u32(ERR_REFUSED)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_requests_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (a, _sa) = serve(&dir, &chain(&key(7), 3)).await;
    let b = bind(&dir, "b").await;
    let auth = sync_auth(&secret(), b.endpoint().id(), a.endpoint().id(), room());
    let n = |x: u8| NodeId::from_bytes([x; 32]);
    let want = |author, holes: Vec<u64>| AuthorWant {
        author,
        have_hi: 100,
        holes,
    };
    let bad = [
        // Too many holes.
        SyncSince {
            want: vec![want(n(1), (1..=17).collect())],
            ..SyncSince::everything(room(), auth)
        },
        // Hole above have_hi.
        SyncSince {
            want: vec![want(n(1), vec![101])],
            ..SyncSince::everything(room(), auth)
        },
        // Authors out of order.
        SyncSince {
            want: vec![want(n(2), vec![]), want(n(1), vec![])],
            ..SyncSince::everything(room(), auth)
        },
        // Author outside (from, to].
        SyncSince {
            from: Some(n(5)),
            want: vec![want(n(1), vec![])],
            ..SyncSince::everything(room(), auth)
        },
    ];
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    for req in &bad {
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        write_frame(&mut send, req).await.unwrap();
        send.finish().unwrap();
        let out = tokio::time::timeout(T, read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME))
            .await
            .unwrap();
        assert!(is_reset_refused(&out), "{req:?} -> {out:?}");
    }
    conn.close(0u32.into(), b"done");
    assert_eq!(SYNC_VERSION, 2);
    a.shutdown().await;
    b.shutdown().await;
}
