//! Sync budgets (ADR-002 §3): deadlines, per-connection concurrency and
//! cumulative caps, against fake malicious peers on both sides.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ed25519_dalek::SigningKey;
use hm_net::{
    ERR_REFUSED, MAX_RESPONSE_FRAME, Net, NetConfig, RelayChoice, RoomSecret, SYNC_ALPN,
    SharedStore, SyncError, SyncLimits, SyncMsg, SyncSince, read_frame, sync_auth, sync_from_with,
    write_frame,
};
use hm_proto::{Event, EventBody, Lamport, RoomId};
use hm_store::Store;
use iroh::endpoint::{Connection, ReadError, ReadExactError, VarInt, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, RelayMode};

/// Outer bound for every test: far above the short limits used below.
const T: Duration = Duration::from_secs(20);

fn secret() -> RoomSecret {
    RoomSecret::from_bytes([9; 32])
}

fn room() -> RoomId {
    secret().room_id()
}

fn store() -> SharedStore {
    Arc::new(Mutex::new(Store::open_in_memory(room()).unwrap()))
}

fn notes(n: u64) -> Vec<Event> {
    let k = SigningKey::from_bytes(&[3; 32]);
    (1..=n)
        .map(|i| {
            Event::sign(
                &k,
                room(),
                "ag",
                i,
                Lamport(i),
                1_000 + i,
                EventBody::Note {
                    scope: "s".into(),
                    text: format!("note {i}"),
                },
            )
            .unwrap()
        })
        .collect()
}

/// Short limits so stall tests finish in about a second.
fn short() -> SyncLimits {
    SyncLimits {
        frame_deadline: Duration::from_millis(400),
        request_deadline: Duration::from_millis(1_500),
        total_deadline: Duration::from_secs(5),
        ..SyncLimits::DEFAULT
    }
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

#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Read the request, then hold the stream open and say nothing.
    Silent,
    /// Announce a 100-byte frame and send one byte every 100 ms.
    Drip,
    /// Valid but useless frames, each in time, never an `End`.
    Endless,
    /// One validly signed event above `MAX_EVENT_BYTES`, then `End`.
    Oversized,
}

#[derive(Debug, Clone)]
struct Fake(Mode);

impl ProtocolHandler for Fake {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        while let Ok((mut send, mut recv)) = conn.accept_bi().await {
            let mode = self.0;
            tokio::spawn(async move {
                let _ = read_frame::<SyncSince>(&mut recv, 1 << 16).await;
                match mode {
                    Mode::Silent => tokio::time::sleep(Duration::from_secs(60)).await,
                    Mode::Drip => {
                        send.write_all(&100u32.to_be_bytes()).await.ok();
                        for _ in 0..600 {
                            if send.write_all(&[0]).await.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    Mode::Oversized => {
                        let k = SigningKey::from_bytes(&[3; 32]);
                        let e = Event::sign(
                            &k,
                            room(),
                            "ag",
                            1,
                            Lamport(1),
                            1_000,
                            EventBody::Note {
                                scope: "s".into(),
                                text: "x".repeat(hm_proto::limits::MAX_EVENT_BYTES),
                            },
                        )
                        .unwrap();
                        let wire = e.encode().unwrap();
                        write_frame(&mut send, &SyncMsg::Event(wire)).await.ok();
                        write_frame(&mut send, &SyncMsg::End { more: false })
                            .await
                            .ok();
                        send.finish().ok();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Mode::Endless => {
                        for _ in 0..600 {
                            let msg = SyncMsg::Event(vec![1, 2, 3]);
                            if write_frame(&mut send, &msg).await.is_err() {
                                break;
                            }
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
            });
        }
        Ok(())
    }
}

async fn fake(mode: Mode) -> Router {
    let ep = Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .bind()
        .await
        .unwrap();
    Router::builder(ep).accept(SYNC_ALPN, Fake(mode)).spawn()
}

/// Run the client against a fake server; return (error, elapsed).
async fn client_vs(mode: Mode, limits: SyncLimits) -> (SyncError, Duration) {
    let dir = tempfile::tempdir().unwrap();
    let b = bind(&dir, "b").await;
    let srv = fake(mode).await;
    let start = Instant::now();
    let res = tokio::time::timeout(
        T,
        sync_from_with(
            b.endpoint(),
            loopback(srv.endpoint()),
            &secret(),
            &store(),
            &limits,
        ),
    )
    .await
    .expect("client was not bounded by its limits");
    let took = start.elapsed();
    srv.shutdown().await.unwrap();
    b.shutdown().await;
    (res.unwrap_err(), took)
}

#[tokio::test(flavor = "multi_thread")]
async fn client_frame_deadline_stops_silent_server() {
    let (err, took) = client_vs(Mode::Silent, short()).await;
    assert!(matches!(err, SyncError::Timeout(_)), "{err:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_frame_deadline_stops_dripping_server() {
    let (err, took) = client_vs(Mode::Drip, short()).await;
    assert!(matches!(err, SyncError::Timeout(_)), "{err:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_request_deadline_stops_endless_response() {
    let (err, took) = client_vs(Mode::Endless, short()).await;
    assert!(matches!(err, SyncError::Timeout(_)), "{err:?}");
    // Every frame is on time, so only the request deadline can stop it.
    assert!(took >= Duration::from_millis(1_400), "{took:?}");
    assert!(took < Duration::from_secs(4), "{took:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn client_total_budget_bounds_whole_sync() {
    let limits = SyncLimits {
        frame_deadline: Duration::from_secs(30),
        request_deadline: Duration::from_secs(30),
        total_deadline: Duration::from_millis(700),
        ..SyncLimits::DEFAULT
    };
    let (err, took) = client_vs(Mode::Silent, limits).await;
    assert!(matches!(err, SyncError::Timeout(_)), "{err:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
}

#[test]
fn default_limits_match_adr_002() {
    let d = SyncLimits::DEFAULT;
    assert_eq!(d.concurrent_requests, 2);
    assert_eq!(d.request_deadline, Duration::from_secs(10));
    assert_eq!(d.frame_deadline, Duration::from_secs(5));
    assert!(d.total_deadline >= d.request_deadline);
    assert!(d.max_rounds > 0 && d.max_bytes > 0);
}

// ---- server side: a real Net facing a malicious client ----

async fn serving(dir: &tempfile::TempDir, n: u64, limits: SyncLimits) -> Net {
    let a = bind(dir, "a").await;
    let sa = store();
    sa.lock().unwrap().append_batch(&notes(n)).unwrap();
    a.sync_rooms().insert(secret(), sa).unwrap();
    a.sync_rooms().set_limits(limits);
    a
}

fn request(from: &Net, to: &Net) -> SyncSince {
    SyncSince::everything(
        room(),
        sync_auth(&secret(), from.endpoint().id(), to.endpoint().id(), room()),
    )
}

/// One valid request on `conn`: (event frames, outcome).
async fn ask(conn: &Connection, req: &SyncSince) -> (usize, Result<(), SyncError>) {
    let (mut send, mut recv) = match conn.open_bi().await {
        Ok(s) => s,
        Err(e) => return (0, Err(e.into())),
    };
    if let Err(e) = write_frame(&mut send, req).await {
        return (0, Err(e));
    }
    send.finish().ok();
    let mut events = 0;
    loop {
        match read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME).await {
            Ok(SyncMsg::Event(_)) => events += 1,
            Ok(SyncMsg::End { .. }) => return (events, Ok(())),
            Ok(SyncMsg::Refused(r)) => return (events, Err(SyncError::Refused(r))),
            Err(e) => return (events, Err(e)),
        }
    }
}

fn is_reset_refused<T: std::fmt::Debug>(r: &Result<T, SyncError>) -> bool {
    matches!(
        r,
        Err(SyncError::Read(ReadExactError::ReadError(ReadError::Reset(c))))
            if *c == VarInt::from_u32(ERR_REFUSED)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn server_resets_stalled_request() {
    let dir = tempfile::tempdir().unwrap();
    let a = serving(&dir, 3, short()).await;
    let b = bind(&dir, "b").await;
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    // Half a length prefix, then nothing.
    send.write_all(&[0, 0]).await.unwrap();
    let start = Instant::now();
    let out = tokio::time::timeout(T, read_frame::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME))
        .await
        .expect("server never gave up on the stalled request");
    assert!(is_reset_refused(&out), "{out:?}");
    assert!(start.elapsed() < Duration::from_secs(3));
    conn.close(0u32.into(), b"done");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_serves_at_most_two_requests_per_connection_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let limits = SyncLimits {
        frame_deadline: Duration::from_millis(1_000),
        ..short()
    };
    let a = serving(&dir, 3, limits).await;
    let b = bind(&dir, "b").await;
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    let req = request(&b, &a);

    // Unloaded, a request is answered quickly.
    let start = Instant::now();
    let (events, out) = ask(&conn, &req).await;
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(events, 3);
    assert!(start.elapsed() < Duration::from_millis(700));

    // Two stalled requests take both slots...
    let mut stalled = Vec::new();
    for _ in 0..2 {
        let (mut send, recv) = conn.open_bi().await.unwrap();
        send.write_all(&[0, 0]).await.unwrap();
        stalled.push((send, recv));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    // ...so a third waits until the server drops them at the frame deadline.
    let start = Instant::now();
    let (events, out) = tokio::time::timeout(T, ask(&conn, &req)).await.unwrap();
    let took = start.elapsed();
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(events, 3);
    assert!(
        took >= Duration::from_millis(700),
        "served too early: {took:?}"
    );
    conn.close(0u32.into(), b"done");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_caps_rounds_per_connection() {
    let dir = tempfile::tempdir().unwrap();
    let limits = SyncLimits {
        max_rounds: 2,
        ..short()
    };
    let a = serving(&dir, 3, limits).await;
    let b = bind(&dir, "b").await;
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    let req = request(&b, &a);
    for _ in 0..2 {
        let (events, out) = ask(&conn, &req).await;
        assert!(out.is_ok(), "{out:?}");
        assert_eq!(events, 3);
    }
    let (events, out) = tokio::time::timeout(T, ask(&conn, &req)).await.unwrap();
    assert_eq!(events, 0);
    assert!(out.is_err(), "third round on one connection was served");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn server_caps_bytes_per_connection() {
    let dir = tempfile::tempdir().unwrap();
    let one = notes(1)[0].encode().unwrap().len();
    let limits = SyncLimits {
        // Room for two events, not three.
        max_bytes: 2 * one + one / 2,
        ..short()
    };
    let a = serving(&dir, 3, limits).await;
    let b = bind(&dir, "b").await;
    let conn = b
        .endpoint()
        .connect(a.local_addr(), SYNC_ALPN)
        .await
        .unwrap();
    let req = request(&b, &a);
    let (events, out) = ask(&conn, &req).await;
    assert!(out.is_ok(), "{out:?}");
    assert_eq!(events, 2, "response not clamped to the connection budget");
    // Budget spent: the next request on this connection is refused.
    let (events, out) = tokio::time::timeout(T, ask(&conn, &req)).await.unwrap();
    assert_eq!(events, 0);
    assert!(is_reset_refused(&out), "{out:?}");
    conn.close(0u32.into(), b"done");
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_caps_total_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let a = serving(&dir, 5, SyncLimits::DEFAULT).await;
    let b = bind(&dir, "b").await;
    let one = notes(1)[0].encode().unwrap().len();
    let limits = SyncLimits {
        max_bytes: 2 * one,
        ..short()
    };
    let res = tokio::time::timeout(
        T,
        sync_from_with(b.endpoint(), a.local_addr(), &secret(), &store(), &limits),
    )
    .await
    .unwrap();
    assert!(
        matches!(res, Err(SyncError::Protocol(_))),
        "client accepted more than its byte budget: {res:?}"
    );
    a.shutdown().await;
    b.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn client_rejects_event_above_gossip_cap() {
    // GATE-2 N5: sync must not be a side door for events gossip would drop.
    let (err, _) = client_vs(Mode::Oversized, short()).await;
    assert!(matches!(err, SyncError::Protocol(_)), "{err:?}");
}
