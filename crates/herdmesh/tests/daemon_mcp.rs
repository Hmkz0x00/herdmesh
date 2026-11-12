//! Daemon in-process (relay disabled) + an `mcp` backend client over IPC.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use herdmesh::daemon::{self, DaemonError, DaemonOptions, Ticker};
use herdmesh::data::{DataDir, RoleName, RoomConfig};
use herdmesh::session::init_genesis;
use hm_daemon::{Daemon, NullNet};
use hm_herdr::FakeHerdrCli;
use hm_mcp::McpServer;
use hm_net::{RelayChoice, RoomSecret};
use hm_proto::{AgentStatus, EventBody, Lamport, RoomId};
use hm_store::Store;
use serde_json::json;
use tokio::sync::oneshot;

static N: AtomicUsize = AtomicUsize::new(0);

fn socket() -> String {
    format!(
        "herdmesh-dtest-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn herdr_json(name: &str, status: &str) -> String {
    format!(
        r#"{{"result":{{"agents":[{{"name":"{name}","agent":"claude","agent_status":"{status}","pane_id":"w1:p1","cwd":"/x"}}]}}}}"#
    )
}

fn opts(socket: &str, herdr: Arc<FakeHerdrCli>) -> DaemonOptions {
    DaemonOptions {
        socket: socket.to_owned(),
        relay: RelayChoice::Disabled,
        herdr,
        tick: Duration::from_millis(50),
    }
}

/// A data dir with room "alpha" owned by us, as `room create` leaves it.
fn setup(tmp: &tempfile::TempDir) -> (DataDir, RoomId) {
    let data = DataDir::new(tmp.path().join("d"));
    let id = data
        .create_room(
            &RoomSecret::generate().unwrap(),
            &RoomConfig::new("alpha", RoleName::Owner, "alice"),
            &[],
        )
        .unwrap();
    init_genesis(&data, id, "alice").unwrap();
    (data, id)
}

async fn wait_serving(socket: &str) {
    for _ in 0..100 {
        if daemon::is_serving(socket).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("daemon never served {socket}");
}

async fn mcp(data: &DataDir, socket: &str, agent: &str) -> McpServer {
    let b = daemon::mcp_backend(data, socket, agent, Some("alpha"), || {
        panic!("daemon is already running: must not auto-start")
    })
    .await
    .unwrap();
    McpServer::new(Arc::new(b))
}

#[tokio::test]
async fn mcp_backend_round_trips_through_a_running_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, id) = setup(&tmp);
    let sock = socket();
    let herdr = Arc::new(FakeHerdrCli::new(herdr_json("bob", "working")));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let d2 = data.clone();
    let o = opts(&sock, herdr);
    let task = tokio::spawn(async move {
        daemon::run(&d2, o, async { stop_rx.await.ok().unwrap_or(()) }, |_| {}).await
    });
    wait_serving(&sock).await;

    let (a, b) = (
        mcp(&data, &sock, "alice").await,
        mcp(&data, &sock, "bob").await,
    );
    a.invoke("ask", json!({"to": "bob", "question": "ready?"}))
        .await
        .unwrap();
    let inbox = b.invoke("inbox", json!({})).await.unwrap();
    assert!(inbox.contains("ready?"), "{inbox}");

    // A second daemon for the same socket is refused.
    let again = daemon::run(
        &data,
        opts(&sock, Arc::new(FakeHerdrCli::new("[]"))),
        std::future::pending::<()>(),
        |_| {},
    )
    .await;
    assert!(
        matches!(again, Err(DaemonError::AlreadyRunning(_))),
        "{again:?}"
    );

    stop_tx.send(()).unwrap();
    task.await.unwrap().unwrap();

    // Locally authored events were stored in the shared per-room db.
    let store = Store::open(data.events_db(&id), id).unwrap();
    let asks = store
        .events_since(Lamport(0))
        .unwrap()
        .into_iter()
        .filter(|e| matches!(e.body, EventBody::Ask { .. }))
        .count();
    assert_eq!(asks, 1);
}

#[tokio::test]
async fn daemon_loop_announces_presence_and_pings_idle_agent() {
    let tmp = tempfile::tempdir().unwrap();
    let (data, id) = setup(&tmp);
    let sock = socket();
    let herdr = Arc::new(FakeHerdrCli::new(herdr_json("bob", "working")));
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let d2 = data.clone();
    let o = opts(&sock, herdr.clone());
    let task = tokio::spawn(async move {
        daemon::run(&d2, o, async { stop_rx.await.ok().unwrap_or(()) }, |_| {}).await
    });
    wait_serving(&sock).await;
    let (a, b) = (
        mcp(&data, &sock, "alice").await,
        mcp(&data, &sock, "bob").await,
    );
    b.invoke("inbox", json!({})).await.unwrap(); // registers bob
    a.invoke("ask", json!({"to": "bob", "question": "ready?"}))
        .await
        .unwrap();

    let prompts = || herdr.calls().iter().filter(|c| c[1] == "prompt").count();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(prompts(), 0, "bob is working: no ping");

    herdr.set_list_json(herdr_json("bob", "idle"));
    for _ in 0..100 {
        if prompts() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(prompts(), 1);
    let call = herdr
        .calls()
        .into_iter()
        .find(|c| c[1] == "prompt")
        .unwrap();
    assert!(call.last().unwrap().starts_with("[herdmesh] Ask from "));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(prompts(), 1, "no repeat pings for the same ask");

    stop_tx.send(()).unwrap();
    task.await.unwrap().unwrap();

    // Presence: working, then idle: two events, no more.
    let store = Store::open(data.events_db(&id), id).unwrap();
    let statuses: Vec<AgentStatus> = store
        .events_since(Lamport(0))
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::Presence { status } if e.author.agent_name == "bob" => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses, [AgentStatus::Working, AgentStatus::Idle]);
}

#[test]
fn ticker_emits_presence_once_per_change() {
    let d = Arc::new(Daemon::new(
        ed25519_dalek::SigningKey::from_bytes(&[3; 32]),
        Arc::new(NullNet),
    ));
    let room = d
        .create_room(
            "alpha",
            Store::open_in_memory(RoomId::from_bytes([2; 32])).unwrap(),
        )
        .unwrap();
    let cli = Arc::new(FakeHerdrCli::new(herdr_json("bob", "idle")));
    let mut t = Ticker::new(d.clone(), cli.clone());
    assert_eq!(t.tick().presence, 0, "bob is not a member yet");
    d.handle_request(hm_daemon::ipc::Request {
        agent: "bob".into(),
        room: Some(room),
        call: hm_daemon::ipc::Call::RoomCard,
    });
    assert_eq!(t.tick().presence, 1);
    assert_eq!(t.tick().presence, 0, "unchanged");
    cli.set_list_json(herdr_json("bob", "working"));
    assert_eq!(t.tick().presence, 1);
}

#[tokio::test]
async fn ensure_running_reports_a_failed_start() {
    let sock = socket();
    let r = daemon::ensure_running(&sock, || Err(std::io::Error::other("no exe"))).await;
    assert!(matches!(r, Err(DaemonError::Io(_))), "{r:?}");
}
