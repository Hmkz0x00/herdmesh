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

#[tokio::test(flavor = "multi_thread")]
async fn sparse_gap_after_128_unfillable_holes_is_fetched() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(42);
    let all: Vec<_> = (129..=131).map(|i| ev(&k, i, i, "gap")).collect();
    let (a, sa) = serve(&dir, &all).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    fill(&sb, &[all[0].clone(), all[2].clone()]);
    let stats = sync_from(b.endpoint(), a.local_addr(), &secret(), &sb)
        .await
        .unwrap();
    assert_eq!(stats.inserted, 1, "{stats:?}");
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_pages_resume_across_budgeted_runs_and_store_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let k = key(43);
    let all: Vec<_> = (0..40)
        .map(|i| ev(&k, 129 + i * 2, 129 + i * 2, "sparse"))
        .collect();
    let (a, sa) = serve(&dir, &all).await;
    let b = bind(&dir, "b").await;
    let db = dir.path().join("client.db");
    let sb = Arc::new(Mutex::new(Store::open(&db, room()).unwrap()));
    let held: Vec<_> = all
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 35)
        .map(|(_, e)| e.clone())
        .collect();
    fill(&sb, &held);
    let limits = hm_net::SyncLimits {
        max_rounds: 1,
        ..hm_net::SyncLimits::DEFAULT
    };
    for _ in 0..8 {
        let stats = hm_net::sync_from_with(b.endpoint(), a.local_addr(), &secret(), &sb, &limits)
            .await
            .unwrap();
        assert_eq!(stats.rounds, 1);
        *sb.lock().unwrap() = Store::open(&db, room()).unwrap();
    }
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn sparse_ack_does_not_skip_events_cut_by_response_cap() {
    let s = store();
    let k = key(44);
    let all = [ev(&k, 129, 129, "one"), ev(&k, 131, 131, "two")];
    fill(&s, &all);
    let req = SyncSince {
        want: vec![AuthorWant {
            author: all[0].author.node,
            have_hi: 131,
            holes: vec![],
            check: None,
            sparse: Some(hm_net::sync::SparseCheck {
                after: 0,
                through: u64::MAX,
                ids: vec![],
            }),
        }],
        ..SyncSince::everything(room(), [0; 32])
    };
    let answer = hm_net::answer_request(
        &s.lock().unwrap(),
        &req,
        hm_store::SyncCaps {
            max_events: 1,
            max_bytes: 1 << 20,
        },
    )
    .unwrap();
    assert_eq!(answer.events.len(), 1);
    assert!(answer.more);
    assert_eq!(answer.sparse[0].through, 129);
}

#[tokio::test(flavor = "multi_thread")]
async fn sparse_author_pages_resume_across_short_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut keys: Vec<_> = (100..100 + MAX_AUTHORS_PER_REQUEST as u32 + 1)
        .map(key)
        .collect();
    keys.sort_by_key(|k| NodeId::from_verifying_key(&k.verifying_key()));
    let held: Vec<_> = keys.iter().map(|k| ev(k, 129, 129, "held")).collect();
    let missing = ev(keys.last().unwrap(), 130, 130, "last author");
    let mut all = held.clone();
    all.push(missing);
    let (a, sa) = serve(&dir, &all).await;
    let b = bind(&dir, "b").await;
    let sb = store();
    fill(&sb, &held);
    let limits = hm_net::SyncLimits {
        max_rounds: 1,
        ..hm_net::SyncLimits::DEFAULT
    };
    for _ in 0..3 {
        hm_net::sync_from_with(b.endpoint(), a.local_addr(), &secret(), &sb, &limits)
            .await
            .unwrap();
    }
    assert_eq!(ids(&sa), ids(&sb));
    a.shutdown().await;
    b.shutdown().await;
}

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
            want: page
                .want
                .into_iter()
                .map(|mut w| {
                    w.check = Some(hm_net::PrefixCheck {
                        lo: 0,
                        at: w.have_hi,
                        fp: [255; 32],
                    });
                    w.sparse = Some(hm_net::sync::SparseCheck {
                        after: u64::MAX - 1000,
                        through: u64::MAX,
                        ids: (0..hm_net::sync::SPARSE_IDS)
                            .map(|i| {
                                (
                                    u64::MAX - 999 + i as u64,
                                    hm_proto::EventId::from_bytes([255; 32]),
                                )
                            })
                            .collect(),
                    });
                    w
                })
                .collect(),
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
        check: None,
        sparse: None,
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
        // Prefix check with an empty range.
        SyncSince {
            want: vec![AuthorWant {
                check: Some(hm_net::PrefixCheck {
                    lo: 7,
                    at: 7,
                    fp: [0; 32],
                }),
                ..want(n(1), vec![])
            }],
            ..SyncSince::everything(room(), auth)
        },
        // Empty sparse interval.
        SyncSince {
            want: vec![AuthorWant {
                sparse: Some(hm_net::sync::SparseCheck {
                    after: 9,
                    through: 9,
                    ids: vec![],
                }),
                ..want(n(1), vec![])
            }],
            ..SyncSince::everything(room(), auth)
        },
        // Too many held IDs, even though the wire frame itself is small.
        SyncSince {
            want: vec![AuthorWant {
                sparse: Some(hm_net::sync::SparseCheck {
                    after: 0,
                    through: u64::MAX,
                    ids: (1..=hm_net::sync::SPARSE_IDS as u64 + 1)
                        .map(|seq| (seq, hm_proto::EventId::from_bytes([0; 32])))
                        .collect(),
                }),
                ..want(n(1), vec![])
            }],
            ..SyncSince::everything(room(), auth)
        },
        // An ID outside the claimed sparse interval.
        SyncSince {
            want: vec![AuthorWant {
                sparse: Some(hm_net::sync::SparseCheck {
                    after: 10,
                    through: 20,
                    ids: vec![(21, hm_proto::EventId::from_bytes([0; 32]))],
                }),
                ..want(n(1), vec![])
            }],
            ..SyncSince::everything(room(), auth)
        },
        // Prefix check above have_hi.
        SyncSince {
            want: vec![AuthorWant {
                check: Some(hm_net::PrefixCheck {
                    lo: 0,
                    at: 101,
                    fp: [0; 32],
                }),
                ..want(n(1), vec![])
            }],
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
    assert_eq!(SYNC_VERSION, 4);
    a.shutdown().await;
    b.shutdown().await;
}

#[test]
fn answer_pages_through_many_authors_within_the_requested_range() {
    // GATE-2 N4: the server reads authors a page at a time instead of a
    // whole-room summary; results must not change across page boundaries.
    let s = store();
    let evs: Vec<Event> = (0..150u32).map(|n| ev(&key(n), 1, 1, "x")).collect();
    fill(&s, &evs);
    let st = s.lock().unwrap();
    let caps = hm_store::SyncCaps::DEFAULT;
    let all = SyncSince::everything(room(), [0; 32]);
    let hm_net::Answer {
        events: out, more, ..
    } = hm_net::answer_request(&st, &all, caps).unwrap();
    assert_eq!((out.len(), more), (150, false));

    let mut nodes: Vec<NodeId> = evs.iter().map(|e| e.author.node).collect();
    nodes.sort();
    let mid = SyncSince {
        from: Some(nodes[9]),
        to: Some(nodes[109]),
        ..all
    };
    let hm_net::Answer {
        events: out, more, ..
    } = hm_net::answer_request(&st, &mid, caps).unwrap();
    assert!(!more);
    let got: Vec<NodeId> = out
        .iter()
        .map(|w| Event::decode(w).unwrap().author.node)
        .collect();
    assert_eq!(got, nodes[10..=109]);
}
