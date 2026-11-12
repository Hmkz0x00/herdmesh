//! GATE-2 N1: the client authenticates the server before sending anything,
//! and the token never crosses the socket (challenge-response).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use hm_daemon::ipc::{
    self, Call, ClientHello, ClientProof, IpcError, OwnerId, Probe, Request, Response, Server,
    ServerHello, Token,
};
use hm_daemon::{Daemon, NullNet};
use hm_proto::RoomId;
use hm_store::Store;
use interprocess::local_socket::tokio::prelude::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

static N: AtomicUsize = AtomicUsize::new(0);

fn name() -> String {
    format!(
        "herdmesh-auth-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

const TOKEN: [u8; 32] = [0x5a; 32];

fn req() -> Request {
    Request {
        agent: "probe-agent".into(),
        room: None,
        call: Call::RoomCard,
    }
}

fn serve(n: &str, token: Token) -> ipc::ServerHandle {
    let d = Daemon::new(SigningKey::from_bytes(&[3; 32]), Arc::new(NullNet));
    d.create_room(
        "alpha",
        Store::open_in_memory(RoomId::from_bytes([1; 32])).unwrap(),
    )
    .unwrap();
    Server::bind(n, token).unwrap().spawn(Arc::new(d))
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A squatter on `n`: records everything a client sends; `reply` (if any)
/// is sent after the first frame.
fn squat(n: &str, reply: Option<ServerHello>) -> Arc<Mutex<Vec<u8>>> {
    let listener = ipc::bind_raw(n).unwrap();
    let log = Arc::new(Mutex::new(Vec::new()));
    let l2 = log.clone();
    tokio::spawn(async move {
        while let Ok(mut s) = listener.accept().await {
            let log = l2.clone();
            let reply = reply.as_ref().map(|r| (r.nonce, r.proof));
            tokio::spawn(async move {
                let mut sent = reply.is_none();
                let mut buf = [0u8; 4096];
                loop {
                    let r = tokio::time::timeout(Duration::from_secs(2), s.read(&mut buf)).await;
                    let Ok(Ok(k)) = r else { break };
                    if k == 0 {
                        break;
                    }
                    log.lock().unwrap().extend_from_slice(&buf[..k]);
                    if !sent && let Some((nonce, proof)) = reply {
                        sent = true;
                        let _ = ipc::write_frame(&mut s, &ServerHello { nonce, proof }).await;
                    }
                }
            });
        }
    });
    log
}

#[tokio::test]
async fn client_refuses_a_server_run_by_another_identity() {
    let n = name();
    let log = squat(&n, None);
    let other = if cfg!(windows) {
        "S-1-5-21-1-2-3-4242"
    } else {
        "4242"
    };
    let c = ipc::Client::new(n, Token::from_bytes(TOKEN)).expect_owner(OwnerId::from_raw(other));
    let r = c.request(&req()).await;
    assert!(matches!(r, Err(IpcError::NotOwner)), "{r:?}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        log.lock().unwrap().is_empty(),
        "client sent bytes to a foreign server"
    );
}

#[tokio::test]
async fn client_refuses_a_server_that_cannot_prove_the_token() {
    let n = name();
    let log = squat(
        &n,
        Some(ServerHello {
            nonce: [1; 32],
            proof: [2; 32],
        }),
    );
    let c = ipc::Client::new(n.clone(), Token::from_bytes(TOKEN));
    let r = c.request(&req()).await;
    assert!(matches!(r, Err(IpcError::Refused)), "{r:?}");
    assert!(matches!(c.probe().await, Err(IpcError::Refused)));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let got = log.lock().unwrap().clone();
    assert!(!got.is_empty());
    assert!(!contains(&got, &TOKEN), "token sent to a squatter");
    assert!(!contains(&got, b"probe-agent"), "request sent before auth");
}

#[tokio::test]
async fn wrong_client_proof_gets_no_answer() {
    let n = name();
    let _srv = serve(&n, Token::from_bytes(TOKEN));
    let mut s = ipc::connect_raw(&n).await.unwrap();
    ipc::write_frame(&mut s, &ClientHello { nonce: [4; 32] })
        .await
        .unwrap();
    let _: ServerHello = ipc::read_frame(&mut s).await.unwrap();
    ipc::write_frame(&mut s, &ClientProof { proof: [0; 32] })
        .await
        .unwrap();
    let _ = ipc::write_frame(&mut s, &req()).await;
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    assert!(buf.is_empty(), "answered a client with a wrong proof");
    // The legacy v1 hello (raw token) is refused too.
    let mut s = ipc::connect_raw(&n).await.unwrap();
    s.write_all(&[1, 0, 0, 0, 32]).await.unwrap();
    s.write_all(&TOKEN).await.unwrap();
    let mut buf = Vec::new();
    let _ = s.read_to_end(&mut buf).await;
    assert!(buf.is_empty());
}

#[tokio::test]
async fn real_client_and_server_agree() {
    let n = name();
    let _srv = serve(&n, Token::from_bytes(TOKEN));
    let c = ipc::Client::new(n.clone(), Token::from_bytes(TOKEN));
    c.probe().await.unwrap();
    assert!(matches!(c.request(&req()).await, Ok(Response::Ok(_))));
    // Same user, different token (e.g. another data dir): refused.
    let other = ipc::Client::new(n, Token::from_bytes([1; 32]));
    assert!(matches!(other.probe().await, Err(IpcError::Refused)));
    assert!(other.request(&req()).await.is_err());
}

/// Records every byte written through it.
struct Tap<S> {
    inner: S,
    log: Arc<Mutex<Vec<u8>>>,
}

impl<S: AsyncRead + Unpin> AsyncRead for Tap<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for Tap<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let r = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(k)) = r {
            self.log.lock().unwrap().extend_from_slice(&buf[..k]);
        }
        r
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

#[tokio::test]
async fn token_never_appears_in_handshake_frames() {
    let (a, b) = tokio::io::duplex(4096);
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut client = Tap {
        inner: a,
        log: log.clone(),
    };
    let mut server = Tap {
        inner: b,
        log: log.clone(),
    };
    let t = Token::from_bytes(TOKEN);
    let t2 = t.clone();
    let srv = tokio::spawn(async move { ipc::server_handshake(&mut server, &t2).await });
    ipc::client_handshake(&mut client, &t).await.unwrap();
    srv.await.unwrap().unwrap();
    let wire = log.lock().unwrap().clone();
    assert!(wire.len() > 64, "handshake exchanged frames");
    assert!(!contains(&wire, &TOKEN), "token crossed the wire");
    assert!(
        !contains(&wire, &TOKEN[..8]),
        "token prefix crossed the wire"
    );
}

#[tokio::test]
async fn proofs_are_bound_to_both_nonces_and_roles() {
    let t = Token::from_bytes(TOKEN);
    let (c, s) = ([1; 32], [2; 32]);
    assert_ne!(t.server_proof(&c, &s), t.client_proof(&c, &s));
    assert_ne!(t.server_proof(&c, &s), t.server_proof(&s, &c));
    assert_ne!(
        t.client_proof(&c, &s),
        Token::from_bytes([1; 32]).client_proof(&c, &s)
    );
}

#[tokio::test]
async fn second_bind_on_a_live_name_fails() {
    let n = name();
    let _srv = serve(&n, Token::from_bytes(TOKEN));
    assert!(Server::bind(&n, Token::from_bytes(TOKEN)).is_err());
    // The first server still serves.
    let c = ipc::Client::new(n, Token::from_bytes(TOKEN));
    c.probe().await.unwrap();
}

#[tokio::test]
async fn probe_owner_reports_free_and_ours() {
    let n = name();
    assert_eq!(ipc::probe_owner(&n).await, Probe::Free);
    let _srv = serve(&n, Token::from_bytes(TOKEN));
    assert_eq!(ipc::probe_owner(&n).await, Probe::Ours);
}
