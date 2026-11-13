//! `herdmesh daemon`: one per user. Loads every room, runs gossip and sync
//! repair per room (via [`RoomSession`]), serves local agents over IPC, and
//! once per tick mirrors herdr agent status into Presence events and pings
//! idle agents about new Asks/Handoffs.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use hm_daemon::ipc::{self, IpcError, Probe, Server, Token};
use hm_daemon::{Daemon, NetHandle};
use hm_herdr::{HerdrCli, SystemHerdrCli};
use hm_net::{RelayChoice, SharedStore};
use hm_proto::{Event, RoomId};
use hm_store::Store;
use tokio::sync::{mpsc, watch};

use crate::data::{DataDir, DataError};
use crate::notify::{NotifyQueue, PresenceTracker, status_by_name};
use crate::session::{RoomSession, SessionError, bind_net, node_key};

/// How often herdr is polled for presence and pending pings.
pub const TICK: Duration = Duration::from_secs(5);
/// How long `mcp` waits for an auto-started daemon to serve its socket.
const START_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    #[error("a herdmesh daemon is already running (socket {0})")]
    AlreadyRunning(String),
    #[error("daemon did not start serving {0} in time")]
    StartTimeout(String),
    #[error(
        "socket {0} is served by another user or by a daemon with a different token; refusing to use it"
    )]
    Foreign(String),
    #[error(transparent)]
    Data(#[from] DataError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Net(#[from] hm_net::NetError),
    #[error("ipc: {0}")]
    Ipc(#[from] IpcError),
    #[error("store: {0}")]
    Store(#[from] hm_store::StoreError),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

pub struct DaemonOptions {
    /// Socket name (see [`ipc::socket_name`]); tests use their own.
    pub socket: String,
    pub relay: RelayChoice,
    pub herdr: Arc<dyn HerdrCli>,
    pub tick: Duration,
}

impl DaemonOptions {
    /// Per-user socket, real herdr, relay from the data dir config.
    pub fn from_data(data: &DataDir) -> Result<Self, DaemonError> {
        Ok(Self {
            socket: default_socket(),
            relay: data.load_config()?.relay()?,
            herdr: Arc::new(SystemHerdrCli::from_env()),
            tick: TICK,
        })
    }
}

/// The per-user daemon socket name.
pub fn default_socket() -> String {
    ipc::socket_name(&ipc::current_user())
}

/// Where the IPC token lives (owner-only file).
pub fn token_path(data: &DataDir) -> PathBuf {
    data.root().join("ipc.token")
}

/// True if a process of the current user serves `socket`. Sends nothing.
pub async fn is_serving(socket: &str) -> bool {
    ipc::probe_owner(socket).await == Probe::Ours
}

/// Who serves `socket`: nobody, our daemon (current user, proves it holds
/// `token`), or something we must not talk to (another user, or a daemon
/// with a different token). Neither the token nor a request is sent.
pub async fn serving(socket: &str, token: &Token) -> Probe {
    match ipc::probe_owner(socket).await {
        Probe::Ours => match ipc::Client::new(socket, token.clone()).probe().await {
            Ok(()) => Probe::Ours,
            Err(_) => Probe::Foreign,
        },
        p => p,
    }
}

/// Locally authored events go to the room's task, which broadcasts them.
struct ChannelNet {
    routes: HashMap<RoomId, mpsc::UnboundedSender<Event>>,
}

impl NetHandle for ChannelNet {
    fn publish(&self, room: RoomId, event: &Event) {
        if let Some(tx) = self.routes.get(&room) {
            // A closed receiver means the daemon is shutting down.
            let _ = tx.send(event.clone());
        }
    }
}

/// One tick of herdr-facing work, separate from the loop so tests drive it.
pub struct Ticker {
    daemon: Arc<Daemon>,
    cli: Arc<dyn HerdrCli>,
    presence: PresenceTracker,
    queue: NotifyQueue,
}

/// What one tick did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub presence: usize,
    pub pings: usize,
}

impl Ticker {
    pub fn new(daemon: Arc<Daemon>, cli: Arc<dyn HerdrCli>) -> Self {
        Self {
            daemon,
            cli,
            presence: PresenceTracker::default(),
            queue: NotifyQueue::default(),
        }
    }

    pub fn tick(&mut self) -> TickReport {
        let mut report = TickReport::default();
        let current = status_by_name(&*self.cli);
        for room in self.daemon.room_ids() {
            let members = self.daemon.local_members(room);
            for (agent, status) in self.presence.diff(room, &members, &current) {
                match self.daemon.set_presence(room, &agent, status) {
                    Ok(()) => report.presence += 1,
                    Err(_) => self.presence.forget(room, &agent),
                }
            }
        }
        self.queue.offer(self.daemon.pending_pings());
        report.pings = self.queue.flush(&*self.cli);
        report
    }
}

/// Run the daemon until `shutdown` resolves. `out` gets one-line,
/// body-free status lines.
pub async fn run(
    data: &DataDir,
    opts: DaemonOptions,
    shutdown: impl Future<Output = ()>,
    mut out: impl FnMut(&str),
) -> Result<(), DaemonError> {
    let token = ipc::load_or_create_token(&token_path(data))?;
    match serving(&opts.socket, &token).await {
        Probe::Free => {}
        Probe::Ours => return Err(DaemonError::AlreadyRunning(opts.socket)),
        Probe::Foreign => return Err(DaemonError::Foreign(opts.socket)),
    }
    let key = node_key(data)?;
    let rooms = data.list_rooms()?;

    let mut routes = HashMap::new();
    let mut outbound = HashMap::new();
    for (id, _) in &rooms {
        let (tx, rx) = mpsc::unbounded_channel();
        routes.insert(*id, tx);
        outbound.insert(*id, rx);
    }
    let daemon = Arc::new(Daemon::new(key, Arc::new(ChannelNet { routes })));

    let net = bind_net(data, opts.relay.clone()).await?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let (log_tx, mut log_rx) = mpsc::unbounded_channel::<String>();
    let mut tasks = Vec::new();
    for (id, cfg) in &rooms {
        let store: SharedStore = Arc::new(Mutex::new(Store::open(data.events_db(id), *id)?));
        let session = RoomSession::open_with(
            data,
            *id,
            opts.relay.clone(),
            net.clone(),
            Arc::clone(&store),
        )
        .await?;
        daemon.add_room_shared(&cfg.name, store);
        let rx = outbound
            .remove(id)
            .unwrap_or_else(|| mpsc::unbounded_channel().1);
        tasks.push(tokio::spawn(room_loop(
            session,
            rx,
            stop_rx.clone(),
            log_tx.clone(),
            cfg.name.clone(),
        )));
        out(&format!(
            "room      {} loaded",
            crate::display::sanitize_display(&cfg.name, crate::display::NAME_MAX)
        ));
    }
    drop(log_tx);

    let server = Server::bind(&opts.socket, token)?.spawn(daemon.clone());
    out(&format!(
        "daemon    serving {} ({} room(s), node {})",
        opts.socket,
        rooms.len(),
        net.endpoint().id()
    ));

    let ticker = Arc::new(Mutex::new(Ticker::new(daemon, opts.herdr)));
    let mut interval = tokio::time::interval(opts.tick);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            Some(line) = log_rx.recv() => out(&line),
            _ = interval.tick() => {
                let t = Arc::clone(&ticker);
                // herdr is a child process: keep it off the async threads.
                let report = tokio::task::spawn_blocking(move || {
                    t.lock().unwrap_or_else(PoisonError::into_inner).tick()
                })
                .await;
                if let Ok(r) = report && (r.presence > 0 || r.pings > 0) {
                    out(&format!("herdr     {} presence, {} ping(s)", r.presence, r.pings));
                }
            }
        }
    }
    drop(server);
    let _ = stop_tx.send(true);
    for t in tasks {
        let _ = t.await;
    }
    net.shutdown().await;
    out("daemon    stopped");
    Ok(())
}

async fn room_loop(
    mut s: RoomSession,
    mut outbound: mpsc::UnboundedReceiver<Event>,
    mut stop: watch::Receiver<bool>,
    log: mpsc::UnboundedSender<String>,
    name: String,
) {
    let name = crate::display::sanitize_display(&name, crate::display::NAME_MAX);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            Some(ev) = outbound.recv() => {
                if let Err(e) = s.broadcast(&ev).await {
                    let _ = log.send(format!("room      {name} broadcast failed: {e}"));
                }
            }
            step = s.step() => match step {
                Some(Ok(line)) => {
                    let _ = log.send(format!("{name}: {line}"));
                }
                Some(Err(e)) => {
                    let _ = log.send(format!("room      {name} stopped: {e}"));
                    break;
                }
                None => break,
            },
        }
    }
    s.detach();
}

/// Make sure a daemon of the current user serves `socket`, starting one
/// with `start` if nothing does. A listener run by someone else is an
/// error, never "running".
pub async fn ensure_running(
    socket: &str,
    start: impl FnOnce() -> io::Result<()>,
) -> Result<(), DaemonError> {
    match ipc::probe_owner(socket).await {
        Probe::Ours => return Ok(()),
        Probe::Foreign => return Err(DaemonError::Foreign(socket.to_owned())),
        Probe::Free => {}
    }
    start()?;
    let deadline = tokio::time::Instant::now() + START_WAIT;
    while tokio::time::Instant::now() < deadline {
        match ipc::probe_owner(socket).await {
            Probe::Ours => return Ok(()),
            Probe::Foreign => return Err(DaemonError::Foreign(socket.to_owned())),
            Probe::Free => {}
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(DaemonError::StartTimeout(socket.to_owned()))
}

/// Spawn `herdmesh daemon` detached, with the same data dir flags.
pub fn spawn_detached(data: &DataDir) -> io::Result<()> {
    use std::process::{Command, Stdio};
    let mut cmd = Command::new(std::env::current_exe()?);
    cmd.arg("--data-dir").arg(data.root());
    if data.insecure_key_allowed() {
        cmd.arg("--allow-insecure-key");
    }
    cmd.arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (not DETACHED_PROCESS): the daemon gets a hidden
        // console that its children (herdr polls) inherit. A console-less
        // parent makes Windows flash a new console window per child.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
    }
    cmd.spawn().map(|_| ())
}

/// A [`hm_mcp::DaemonBackend`] for `agent` (optionally pinned to a room by
/// local name), starting the daemon first if needed.
pub async fn mcp_backend(
    data: &DataDir,
    socket: &str,
    agent: &str,
    room: Option<&str>,
    start: impl FnOnce() -> io::Result<()>,
) -> Result<hm_mcp::DaemonBackend, DaemonError> {
    let room = room.map(|r| data.resolve_room(r)).transpose()?;
    // The token must exist before a daemon is started, so both agree on it.
    let token: Token = ipc::load_or_create_token(&token_path(data))?;
    ensure_running(socket, start).await?;
    // Fail loudly now (wrong token, e.g. another data dir) rather than
    // answering every tool call with "unavailable".
    if serving(socket, &token).await != Probe::Ours {
        return Err(DaemonError::Foreign(socket.to_owned()));
    }
    let b = hm_mcp::DaemonBackend::new(socket, token, agent);
    Ok(match room {
        Some((id, _)) => b.in_room(id),
        None => b,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use ed25519_dalek::SigningKey;
    use hm_daemon::NullNet;

    use super::*;

    static N: AtomicUsize = AtomicUsize::new(0);

    fn name() -> String {
        format!(
            "herdmesh-dm-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        )
    }

    fn serve(n: &str, token: Token) -> ipc::ServerHandle {
        let d = Daemon::new(SigningKey::from_bytes(&[2; 32]), Arc::new(NullNet));
        Server::bind(n, token).unwrap().spawn(Arc::new(d))
    }

    /// N1: "something listens" is not "our daemon serves".
    #[tokio::test]
    async fn serving_needs_our_user_and_our_token() {
        let n = name();
        let t = Token::from_bytes([4; 32]);
        assert_eq!(serving(&n, &t).await, Probe::Free);
        assert!(!is_serving(&n).await);
        let _srv = serve(&n, t.clone());
        assert_eq!(serving(&n, &t).await, Probe::Ours);
        assert!(is_serving(&n).await);
        // Same user, other token (another data dir): not ours to talk to.
        assert_eq!(
            serving(&n, &Token::from_bytes([5; 32])).await,
            Probe::Foreign
        );
    }

    #[tokio::test]
    async fn ensure_running_accepts_our_daemon_without_starting_another() {
        let n = name();
        let _srv = serve(&n, Token::from_bytes([4; 32]));
        ensure_running(&n, || Err(io::Error::other("must not start")))
            .await
            .unwrap();
    }
}
