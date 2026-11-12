//! One room, live: bound endpoint + local store + gossip subscription.
//!
//! `herdmesh room run` drives a [`RoomSession`] in the foreground; tests drive
//! two of them in-process.
//!
//! Catch-up: the store is registered in the endpoint's [`hm_net::SyncRooms`]
//! so peers can pull from us, and we pull from every known peer after joining
//! gossip, then per [`crate::repair`]: on `NeighborUp`, on `Lagged`, and
//! periodically with backoff while connected. Syncs run as background tasks
//! (at most one per peer at a time); `sync_from` appends what it gets with
//! `Store::append_batch`. Gossip events still go through `Store::append`.

use std::fmt::Write as _;
use std::hash::BuildHasher as _;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use hm_net::{
    GossipError, Net, NetConfig, NetError, RelayChoice, RoomEvent, RoomReceiver, RoomSecret,
    RoomSender, RoomTicket, SharedStore, SyncError, SyncStats, endpoint_id_from_node, join_room,
    load_or_create_key, node_id_from_endpoint, sync_from,
};
use hm_proto::{AgentRef, Event, EventBody, EventId, NodeId, ProtoError, RoomId, TaskStatus};
use hm_store::{AppendOutcome, Store, StoreError, Views};
use iroh::EndpointAddr;
use tokio::sync::mpsc;

use crate::data::{DataDir, DataError, RoomConfig, SyncLog, SyncRecord};
use crate::display::{ERROR_MAX, NAME_MAX, TEXT_MAX, sanitize_display};
use crate::repair::Repair;

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error(transparent)]
    Data(#[from] DataError),
    #[error(transparent)]
    Net(#[from] NetError),
    #[error(transparent)]
    Gossip(#[from] GossipError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error("sync: {0}")]
    Sync(#[from] SyncError),
    #[error("store lock poisoned")]
    Poisoned,
    #[error("room has no genesis event; recreate it with this version")]
    NoGenesis,
}

/// Unix millis now (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `name@node10` for display; the peer-chosen name is sanitized.
pub fn short_agent(a: &AgentRef) -> String {
    format!(
        "{}@{}",
        sanitize_display(&a.agent_name, NAME_MAX),
        short_node(&a.node)
    )
}

/// Error text for the terminal: it may embed a peer's close reason.
fn show_err(e: &impl std::fmt::Display) -> String {
    sanitize_display(&e.to_string(), ERROR_MAX)
}

fn short_node(n: &NodeId) -> String {
    n.to_string().chars().take(10).collect()
}

/// A bound endpoint for one room, without the store or gossip. Used by
/// `create`/`invite` to learn our own address for a ticket.
pub async fn bind_net(data: &DataDir, relay: RelayChoice) -> Result<Net, SessionError> {
    Ok(Net::bind(NetConfig {
        key_path: data.node_key(),
        relay,
        allow_insecure_key: data.insecure_key_allowed(),
    })
    .await?)
}

/// Our dialable address for a ticket. With relay disabled (tests / same
/// host) that is the loopback form of our bound sockets.
pub async fn own_addr(net: &Net, relay: &RelayChoice, wait: Duration) -> EndpointAddr {
    if *relay == RelayChoice::Disabled {
        return net.local_addr();
    }
    // Give the endpoint a moment to learn its relay URL / public addrs.
    let _ = tokio::time::timeout(wait, net.endpoint().online()).await;
    net.addr()
}

/// Ticket: room secret + genesis id + our address + peers we already know.
pub fn make_ticket(
    secret: RoomSecret,
    genesis: EventId,
    own: EndpointAddr,
    known: Vec<EndpointAddr>,
) -> RoomTicket {
    let mut bootstrap = vec![own.clone()];
    bootstrap.extend(known.into_iter().filter(|p| p.id != own.id));
    RoomTicket::new(secret, genesis, bootstrap)
}

fn node_key(data: &DataDir) -> Result<SigningKey, SessionError> {
    // Same 32 bytes iroh uses: events are signed by our node identity.
    let key = load_or_create_key(&data.node_key(), data.insecure_key_allowed())?;
    Ok(SigningKey::from_bytes(&key.to_bytes()))
}

/// Room creation (ADR-002 §4): sign `Genesis { owner: us }` as the room's
/// first event and pin it. Idempotent: returns the pinned id if there is one.
pub fn init_genesis(data: &DataDir, id: RoomId, agent: &str) -> Result<EventId, SessionError> {
    let key = node_key(data)?;
    let mut store = Store::open(data.events_db(&id), id)?;
    if let Some(g) = store.genesis() {
        return Ok(g);
    }
    let me = NodeId::from_verifying_key(&key.verifying_key());
    let ev = Event::sign(
        &key,
        id,
        agent,
        store.next_seq(&me)?,
        store.next_lamport()?,
        now_ms(),
        EventBody::Genesis { owner: me },
    )?;
    store.append(&ev)?;
    store.pin_genesis(ev.id)?;
    Ok(ev.id)
}

/// Joining: pin the ticket's genesis id, so only that Genesis (when it
/// arrives by sync) anchors roles. A different, already pinned id is an
/// error.
pub fn pin_genesis(data: &DataDir, id: RoomId, genesis: EventId) -> Result<(), SessionError> {
    Ok(Store::open(data.events_db(&id), id)?.pin_genesis(genesis)?)
}

/// The room's pinned genesis id.
pub fn room_genesis(data: &DataDir, id: RoomId) -> Result<EventId, SessionError> {
    Store::open(data.events_db(&id), id)?
        .genesis()
        .ok_or(SessionError::NoGenesis)
}

/// Result of one background sync, sent back to the session.
struct SyncDone {
    peer: NodeId,
    result: Result<SyncStats, SyncError>,
}

/// Turn a finished sync into its persisted record and a console line.
fn sync_record(
    peer: &NodeId,
    result: &Result<SyncStats, SyncError>,
    at_ms: u64,
) -> (SyncRecord, String) {
    match result {
        Ok(st) => (
            SyncRecord {
                at_ms,
                ok: true,
                rounds: st.rounds,
                received: st.received,
                inserted: st.inserted,
                error: None,
            },
            format!(
                "sync      {} +{} new ({} received, {} round(s))",
                short_node(peer),
                st.inserted,
                st.received,
                st.rounds
            ),
        ),
        Err(e) => (
            SyncRecord {
                at_ms,
                ok: false,
                rounds: 0,
                received: 0,
                inserted: 0,
                error: Some(e.to_string()),
            },
            format!("sync      {} failed: {}", short_node(peer), show_err(e)),
        ),
    }
}

pub struct RoomSession {
    data: DataDir,
    id: RoomId,
    cfg: RoomConfig,
    secret: RoomSecret,
    relay: RelayChoice,
    known_peers: Vec<EndpointAddr>,
    key: SigningKey,
    net: Net,
    store: SharedStore,
    tx: RoomSender,
    rx: RoomReceiver,
    sync_tx: mpsc::UnboundedSender<SyncDone>,
    sync_rx: mpsc::UnboundedReceiver<SyncDone>,
    repair: Repair,
    sync_log: SyncLog,
}

/// Uniform-ish jitter in `[-1, 1]` from std's randomly keyed hasher.
fn jitter() -> f64 {
    let h = std::collections::hash_map::RandomState::new().hash_one(now_ms());
    #[allow(clippy::cast_precision_loss)]
    let unit = (h >> 11) as f64 / (1u64 << 53) as f64;
    unit.mul_add(2.0, -1.0)
}

impl RoomSession {
    /// Bind, open the store, serve it to peers, subscribe to the room topic,
    /// then start pulling from every known peer.
    pub async fn open(
        data: &DataDir,
        id: RoomId,
        relay: RelayChoice,
    ) -> Result<Self, SessionError> {
        let cfg = data.load_room(&id)?;
        let secret = data.load_secret(&id, &cfg)?;
        let known_peers = data.load_peers(&id)?;
        let key = node_key(data)?;
        let net = bind_net(data, relay.clone()).await?;
        let store: SharedStore = Arc::new(Mutex::new(Store::open(data.events_db(&id), id)?));
        net.sync_rooms()
            .insert(secret.clone(), Arc::clone(&store))?;
        let (tx, rx) = join_room(&net, &secret, known_peers.clone()).await?;
        let (sync_tx, sync_rx) = mpsc::unbounded_channel();
        let mut s = Self {
            data: data.clone(),
            id,
            cfg,
            secret,
            relay,
            known_peers,
            key,
            net,
            store,
            tx,
            rx,
            sync_tx,
            sync_rx,
            repair: Repair::default(),
            sync_log: data.load_sync(&id)?,
        };
        let peers: Vec<NodeId> = s
            .known_peers
            .iter()
            .map(|p| node_id_from_endpoint(p.id))
            .collect();
        for p in peers {
            if s.repair.start(p) {
                s.spawn_sync(p);
            }
        }
        Ok(s)
    }

    /// Pull from `peer` in the background. The caller has marked it started
    /// in `self.repair`; the result always comes back via `sync_rx`.
    fn spawn_sync(&mut self, peer: NodeId) {
        let id = match endpoint_id_from_node(peer) {
            Ok(id) if id != self.net.endpoint().id() => id,
            _ => {
                // Never a real peer: clear the in-flight mark, never schedule.
                self.repair.finish(peer, true, Instant::now(), 0.0);
                self.repair.down(peer);
                return;
            }
        };
        let endpoint = self.net.endpoint().clone();
        let secret = self.secret.clone();
        let store = Arc::clone(&self.store);
        let done = self.sync_tx.clone();
        tokio::spawn(async move {
            let result = sync_from(&endpoint, EndpointAddr::new(id), &secret, &store).await;
            // A closed receiver means the session is gone; nothing to report.
            let _ = done.send(SyncDone { peer, result });
        });
    }

    /// Record a finished sync (memory + sync.json) and describe it.
    fn finish_sync(&mut self, d: SyncDone) -> String {
        self.repair
            .finish(d.peer, d.result.is_ok(), Instant::now(), jitter());
        let (rec, line) = sync_record(&d.peer, &d.result, now_ms());
        self.sync_log.insert(d.peer.to_string(), rec);
        if let Err(e) = self.data.save_sync(&self.id, &self.sync_log) {
            tracing::warn!(error = %e, "saving sync.json");
        }
        line
    }

    fn lock(&self) -> Result<MutexGuard<'_, Store>, SessionError> {
        self.store.lock().map_err(|_| SessionError::Poisoned)
    }

    pub fn room_id(&self) -> RoomId {
        self.id
    }

    pub fn config(&self) -> &RoomConfig {
        &self.cfg
    }

    /// The shared store (also served to peers via sync).
    pub fn store(&self) -> &SharedStore {
        &self.store
    }

    /// Last sync result per peer (node id hex), as shown by `room status`.
    pub fn sync_log(&self) -> &SyncLog {
        &self.sync_log
    }

    pub fn net(&self) -> &Net {
        &self.net
    }

    /// Ticket inviting someone into this room via us.
    pub async fn ticket(&self) -> Result<RoomTicket, SessionError> {
        let genesis = self.lock()?.genesis().ok_or(SessionError::NoGenesis)?;
        let own = own_addr(&self.net, &self.relay, Duration::from_secs(5)).await;
        Ok(make_ticket(
            self.secret.clone(),
            genesis,
            own,
            self.known_peers.clone(),
        ))
    }

    /// Wait until at least one gossip neighbor is connected.
    pub async fn joined(&mut self) -> Result<(), SessionError> {
        Ok(self.rx.joined().await?)
    }

    /// Sign `body` as us, store it, broadcast it.
    pub async fn publish(&mut self, body: EventBody) -> Result<Event, SessionError> {
        let me = NodeId::from_verifying_key(&self.key.verifying_key());
        let (seq, lamport) = {
            let s = self.lock()?;
            (s.next_seq(&me)?, s.next_lamport()?)
        };
        let ev = Event::sign(
            &self.key,
            self.id,
            self.cfg.agent_name.clone(),
            seq,
            lamport,
            now_ms(),
            body,
        )?;
        self.lock()?.append(&ev)?;
        self.tx.publish(&ev).await?;
        Ok(ev)
    }

    /// Handle one incoming item (gossip or a finished sync). `None` when the
    /// gossip subscription ended; `Some(Ok(line))` is a one-line, body-free
    /// description for the console.
    pub async fn step(&mut self) -> Option<Result<String, SessionError>> {
        let item = loop {
            let due = self.repair.next_due();
            // Far future when nothing is scheduled; the branch is disabled then.
            let wake = due.map_or_else(
                || tokio::time::Instant::now() + Duration::from_secs(3600),
                tokio::time::Instant::from_std,
            );
            tokio::select! {
                // The session holds a sender, so recv() never yields None here.
                Some(done) = self.sync_rx.recv() => return Some(Ok(self.finish_sync(done))),
                item = self.rx.next() => break item?,
                () = tokio::time::sleep_until(wake), if due.is_some() => {
                    let peers = self.repair.take_due(Instant::now());
                    for p in &peers {
                        self.spawn_sync(*p);
                    }
                    if !peers.is_empty() {
                        return Some(Ok(format!("repair    periodic sync with {} peer(s)", peers.len())));
                    }
                }
            }
        };
        let item = match item {
            Ok(item) => item,
            Err(e) => return Some(Err(e.into())),
        };
        Some(match item {
            RoomEvent::Event(ev) => {
                let appended = self.lock().and_then(|mut s| Ok(s.append(&ev)?));
                match appended {
                    Ok(AppendOutcome::Inserted) => Ok(event_line(&ev)),
                    Ok(AppendOutcome::Duplicate) => Ok(format!("{} (duplicate)", event_line(&ev))),
                    Ok(AppendOutcome::Deferred) => Ok(format!("{} (deferred)", event_line(&ev))),
                    Err(e) => Err(e),
                }
            }
            RoomEvent::NeighborUp(n) => {
                if self.repair.up(n) {
                    self.spawn_sync(n);
                }
                Ok(format!("peer up   {}", short_node(&n)))
            }
            RoomEvent::NeighborDown(n) => {
                self.repair.down(n);
                Ok(format!("peer down {}", short_node(&n)))
            }
            RoomEvent::Lagged => {
                let peers = self.repair.lagged();
                for p in &peers {
                    self.spawn_sync(*p);
                }
                Ok(format!(
                    "lagged: some gossip messages were missed; re-syncing with {} peer(s)",
                    peers.len()
                ))
            }
        })
    }

    pub async fn close(self) {
        // Stop serving this room before the endpoint goes away.
        let _ = self.net.sync_rooms().remove(&self.id);
        self.net.shutdown().await;
    }
}

/// `lamport kind author`, never the body.
pub fn event_line(ev: &Event) -> String {
    format!(
        "#{:<5} {:<12} {}",
        ev.lamport.0,
        format!("{:?}", ev.kind()),
        short_agent(&ev.author)
    )
}

/// Human-readable `room status`.
pub fn render_status(
    id: &RoomId,
    cfg: &RoomConfig,
    views: &Views,
    event_count: u64,
    sync: &SyncLog,
    now_ms: u64,
) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "room {} ({})  you: {} [{:?}]",
        sanitize_display(&cfg.name, NAME_MAX),
        short_room(id),
        sanitize_display(&cfg.agent_name, NAME_MAX),
        cfg.role
    );
    let _ = writeln!(s, "events: {event_count}");
    let _ = match (views.owner, views.genesis) {
        (Some(o), _) => writeln!(s, "owner: {} (genesis verified)", short_node(&o)),
        (None, Some(g)) => writeln!(
            s,
            "owner: unverified (genesis {} not received yet)",
            g.to_string().chars().take(10).collect::<String>()
        ),
        (None, None) => writeln!(s, "owner: unknown (no genesis pinned)"),
    };

    let _ = writeln!(s, "members:");
    if views.members.is_empty() {
        let _ = writeln!(s, "  (none seen yet)");
    }
    for (who, m) in &views.members {
        let role = m
            .role
            .map_or("?".to_owned(), |r| format!("{r:?}").to_lowercase());
        let state = if m.present { "present" } else { "away" };
        let status = m
            .status
            .map_or(String::new(), |st| format!(" {st:?}").to_lowercase());
        let _ = writeln!(s, "  {} {role} {state}{status}", short_agent(who));
    }

    let leases: Vec<_> = views.active_leases(now_ms).collect();
    let _ = writeln!(s, "leases:");
    if leases.is_empty() {
        let _ = writeln!(s, "  (none)");
    }
    for (path, l) in leases {
        let left = l.expires_at_ms.saturating_sub(now_ms) / 1000;
        let _ = writeln!(
            s,
            "  {} by {} ({left}s left)",
            sanitize_display(&path, TEXT_MAX),
            short_agent(&l.holder)
        );
    }

    let open: Vec<_> = views
        .board
        .iter()
        .filter(|(_, t)| {
            matches!(
                t.status,
                TaskStatus::Open | TaskStatus::InProgress | TaskStatus::Blocked
            )
        })
        .collect();
    let _ = writeln!(s, "open tasks:");
    if open.is_empty() {
        let _ = writeln!(s, "  (none)");
    }
    for (id, t) in open {
        let st = format!("{:?}", t.status).to_lowercase();
        let _ = writeln!(
            s,
            "  {} [{st}] {}",
            sanitize_display(id.as_str(), TEXT_MAX),
            sanitize_display(&t.title, TEXT_MAX)
        );
    }

    let _ = writeln!(s, "last sync per peer:");
    if sync.is_empty() {
        let _ = writeln!(s, "  (none yet)");
    }
    for (peer, r) in sync {
        let ago = now_ms.saturating_sub(r.at_ms) / 1000;
        let peer = sanitize_display(&peer.chars().take(10).collect::<String>(), 10);
        if r.ok {
            let _ = writeln!(
                s,
                "  {peer} ok, +{} new / {} received, {ago}s ago",
                r.inserted, r.received
            );
        } else {
            let err = r.error.as_deref().unwrap_or("unknown error");
            let _ = writeln!(s, "  {peer} failed: {}, {ago}s ago", show_err(&err));
        }
    }
    s
}

fn short_room(id: &RoomId) -> String {
    id.to_string().chars().take(12).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::RoleName;
    use hm_proto::{Lamport, TaskId};
    use hm_store::{ClaimRec, Member, Task};

    fn agent(n: u8, name: &str) -> AgentRef {
        AgentRef::new(NodeId::from_bytes([n; 32]), name)
    }

    #[test]
    fn status_lists_members_leases_and_only_open_tasks() {
        let id = RoomId::from_bytes([0xab; 32]);
        let cfg = RoomConfig::new("alpha", RoleName::Owner, "lead");
        let mut v = Views::default();
        v.members.insert(
            agent(1, "lead"),
            Member {
                role: Some(hm_proto::Role::Owner),
                present: true,
                status: None,
                last_lamport: Lamport(3),
            },
        );
        let ev = hm_proto::EventId::from_bytes([0; 32]);
        for (n, path, lamport, ts, ttl_secs) in [
            (1u8, "src/net/**", 2, 10_000, 60), // expires at 70_000
            (2u8, "old/**", 1, 0, 5),           // expires at 5_000
        ] {
            v.leases.add_claim(ClaimRec {
                id: hm_proto::EventId::from_bytes([n; 32]),
                holder: agent(1, "lead"),
                lamport: Lamport(lamport),
                ts,
                paths: vec![path.into()],
                ttl_secs,
            });
        }
        for (tid, title, status) in [
            ("t-1", "open one", TaskStatus::Open),
            ("t-2", "done one", TaskStatus::Done),
        ] {
            v.board.insert(
                TaskId::new(tid).unwrap(),
                Task {
                    title: title.into(),
                    paths: vec![],
                    status,
                    note: None,
                    created_by: agent(1, "lead"),
                    created: ev,
                    updated: ev,
                },
            );
        }
        let out = render_status(&id, &cfg, &v, 7, &SyncLog::new(), 10_000);
        assert!(out.contains("room alpha (abababababab)"), "{out}");
        assert!(out.contains("events: 7"), "{out}");
        assert!(out.contains("lead@0101010101 owner present"), "{out}");
        assert!(
            out.contains("src/net/** by lead@0101010101 (60s left)"),
            "{out}"
        );
        assert!(!out.contains("old/**"), "expired lease shown: {out}");
        assert!(out.contains("t-1 [open] open one"), "{out}");
        assert!(!out.contains("t-2"), "done task shown: {out}");
    }

    #[test]
    fn empty_status_says_none() {
        let out = render_status(
            &RoomId::from_bytes([1; 32]),
            &RoomConfig::new("x", RoleName::Member, "a"),
            &Views::default(),
            0,
            &SyncLog::new(),
            0,
        );
        assert!(
            out.contains("(none seen yet)") && out.contains("(none)") && out.contains("(none yet)"),
            "{out}"
        );
    }

    #[test]
    fn status_shows_last_sync_per_peer() {
        let peer = NodeId::from_bytes([0xcd; 32]);
        let mut log = SyncLog::new();
        let ok = sync_record(
            &peer,
            &Ok(SyncStats {
                rounds: 2,
                received: 50,
                inserted: 48,
                duplicates: 2,
                rejected: 0,
                ..SyncStats::default()
            }),
            1_000,
        );
        assert!(
            ok.1.contains("+48 new (50 received, 2 round(s))"),
            "{}",
            ok.1
        );
        log.insert(peer.to_string(), ok.0);
        let failed = NodeId::from_bytes([0xef; 32]);
        let bad = sync_record(&failed, &Err(SyncError::RoomMismatch), 2_000);
        assert!(!bad.0.ok && bad.0.error.is_some());
        log.insert(failed.to_string(), bad.0);

        let out = render_status(
            &RoomId::from_bytes([1; 32]),
            &RoomConfig::new("x", RoleName::Member, "a"),
            &Views::default(),
            50,
            &log,
            11_000,
        );
        assert!(
            out.contains("cdcdcdcdcd ok, +48 new / 50 received, 10s ago"),
            "{out}"
        );
        assert!(out.contains("efefefefef failed: "), "{out}");
    }

    #[test]
    fn peer_authored_fields_are_sanitized_in_status_and_event_lines() {
        let evil = "\x1b[2J\x1b]0;pwned\x07x\r\nsync      fake ok";
        let mut v = Views::default();
        let who = agent(1, evil);
        v.members.insert(
            who.clone(),
            Member {
                role: None,
                present: true,
                status: None,
                last_lamport: Lamport(1),
            },
        );
        let ev = hm_proto::EventId::from_bytes([0; 32]);
        v.leases.add_claim(ClaimRec {
            id: hm_proto::EventId::from_bytes([9; 32]),
            holder: who.clone(),
            lamport: Lamport(1),
            ts: 0,
            paths: vec![format!("src/{evil}")],
            ttl_secs: 60,
        });
        v.board.insert(
            TaskId::new("t-1").unwrap(),
            Task {
                title: evil.into(),
                paths: vec![],
                status: TaskStatus::Open,
                note: None,
                created_by: who.clone(),
                created: ev,
                updated: ev,
            },
        );
        let mut log = SyncLog::new();
        let peer = NodeId::from_bytes([0xcd; 32]);
        let mut rec = sync_record(&peer, &Err(SyncError::RoomMismatch), 0).0;
        rec.error = Some(evil.into());
        log.insert(peer.to_string(), rec);
        let out = render_status(
            &RoomId::from_bytes([1; 32]),
            &RoomConfig::new("x", RoleName::Member, "a"),
            &v,
            3,
            &log,
            10_000,
        );
        assert!(
            !out.contains('\x1b') && !out.contains('\r') && !out.contains('\x07'),
            "{out:?}"
        );
        // Only the renderer's own newlines: one per line it writes.
        assert!(
            out.lines().all(|l| !l.starts_with("sync")),
            "a peer forged a line: {out}"
        );
        assert!(out.contains("\\x1b[2J"), "{out}");

        let note = Event::sign(
            &SigningKey::from_bytes(&[4; 32]),
            RoomId::from_bytes([1; 32]),
            evil,
            1,
            Lamport(1),
            1,
            EventBody::Note {
                scope: "s".into(),
                text: "t".into(),
            },
        )
        .unwrap();
        let line = event_line(&note);
        assert!(!line.chars().any(char::is_control), "{line:?}");
    }

    #[test]
    fn ticket_puts_own_addr_first_and_skips_self_in_known() {
        let me = EndpointAddr::new(iroh::SecretKey::from_bytes(&[1; 32]).public());
        let other = EndpointAddr::new(iroh::SecretKey::from_bytes(&[2; 32]).public());
        let t = make_ticket(
            RoomSecret::from_bytes([9; 32]),
            hm_proto::EventId::from_bytes([1; 32]),
            me.clone(),
            vec![me.clone(), other.clone()],
        );
        assert_eq!(t.bootstrap, vec![me, other]);
    }
}
