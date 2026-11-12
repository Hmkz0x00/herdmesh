//! One room, live: bound endpoint + local store + gossip subscription.
//!
//! `herdmesh room run` drives a [`RoomSession`] in the foreground; tests drive
//! two of them in-process.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::SigningKey;
use hm_net::{
    GossipError, Net, NetConfig, NetError, RelayChoice, RoomEvent, RoomReceiver, RoomSecret,
    RoomSender, RoomTicket, join_room, load_or_create_key,
};
use hm_proto::{AgentRef, Event, EventBody, NodeId, ProtoError, RoomId, TaskStatus};
use hm_store::{AppendOutcome, Store, StoreError, Views};
use iroh::EndpointAddr;

use crate::data::{DataDir, DataError, RoomConfig};

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
}

/// Unix millis now (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// `name@node10` for display.
pub fn short_agent(a: &AgentRef) -> String {
    format!("{}@{}", a.agent_name, short_node(&a.node))
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

/// Ticket: room secret + our address + peers we already know.
pub fn make_ticket(secret: RoomSecret, own: EndpointAddr, known: Vec<EndpointAddr>) -> RoomTicket {
    let mut bootstrap = vec![own.clone()];
    bootstrap.extend(known.into_iter().filter(|p| p.id != own.id));
    RoomTicket::new(secret, bootstrap)
}

pub struct RoomSession {
    id: RoomId,
    cfg: RoomConfig,
    secret: RoomSecret,
    relay: RelayChoice,
    known_peers: Vec<EndpointAddr>,
    key: SigningKey,
    net: Net,
    store: Store,
    tx: RoomSender,
    rx: RoomReceiver,
}

impl RoomSession {
    /// Bind, open the store and subscribe to the room topic.
    pub async fn open(
        data: &DataDir,
        id: RoomId,
        relay: RelayChoice,
    ) -> Result<Self, SessionError> {
        let cfg = data.load_room(&id)?;
        let secret = data.load_secret(&id, &cfg)?;
        let known_peers = data.load_peers(&id)?;
        // Same 32 bytes iroh uses: events are signed by our node identity.
        let key = SigningKey::from_bytes(&load_or_create_key(&data.node_key())?.to_bytes());
        let net = bind_net(data, relay.clone()).await?;
        let store = Store::open(data.events_db(&id), id)?;
        let (tx, rx) = join_room(&net, &secret, known_peers.clone()).await?;
        Ok(Self {
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
        })
    }

    pub fn room_id(&self) -> RoomId {
        self.id
    }

    pub fn config(&self) -> &RoomConfig {
        &self.cfg
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    pub fn net(&self) -> &Net {
        &self.net
    }

    /// Ticket inviting someone into this room via us.
    pub async fn ticket(&self) -> RoomTicket {
        let own = own_addr(&self.net, &self.relay, Duration::from_secs(5)).await;
        make_ticket(self.secret.clone(), own, self.known_peers.clone())
    }

    /// Wait until at least one gossip neighbor is connected.
    pub async fn joined(&mut self) -> Result<(), SessionError> {
        Ok(self.rx.joined().await?)
    }

    /// Sign `body` as us, store it, broadcast it.
    pub async fn publish(&mut self, body: EventBody) -> Result<Event, SessionError> {
        let ev = Event::sign(
            &self.key,
            self.id,
            self.cfg.agent_name.clone(),
            self.store.next_lamport()?,
            now_ms(),
            body,
        )?;
        self.store.append(&ev)?;
        self.tx.publish(&ev).await?;
        Ok(ev)
    }

    /// Handle one incoming item. `None` when the subscription ended;
    /// `Some(Ok(line))` is a one-line, body-free description for the console.
    pub async fn step(&mut self) -> Option<Result<String, SessionError>> {
        let item = match self.rx.next().await? {
            Ok(item) => item,
            Err(e) => return Some(Err(e.into())),
        };
        Some(match item {
            RoomEvent::Event(ev) => match self.store.append(&ev) {
                Ok(AppendOutcome::Inserted) => Ok(event_line(&ev)),
                Ok(AppendOutcome::Duplicate) => Ok(format!("{} (duplicate)", event_line(&ev))),
                Err(e) => Err(e.into()),
            },
            RoomEvent::NeighborUp(n) => Ok(format!("peer up   {}", short_node(&n))),
            RoomEvent::NeighborDown(n) => Ok(format!("peer down {}", short_node(&n))),
            RoomEvent::Lagged => Ok("lagged: some gossip messages were missed".to_owned()),
        })
    }

    pub async fn close(self) {
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
pub fn render_status(id: &RoomId, cfg: &RoomConfig, views: &Views, now_ms: u64) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "room {} ({})  you: {} [{:?}]",
        cfg.name,
        short_room(id),
        cfg.agent_name,
        cfg.role
    );

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
        let _ = writeln!(s, "  {path} by {} ({left}s left)", short_agent(&l.holder));
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
        let _ = writeln!(s, "  {} [{st}] {}", id.as_str(), t.title);
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
    use hm_store::{Lease, Member, Task};

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
        v.leases.insert(
            "src/net/**".into(),
            Lease {
                holder: agent(1, "lead"),
                claim: ev,
                lamport: Lamport(2),
                expires_at_ms: 70_000,
            },
        );
        v.leases.insert(
            "old/**".into(),
            Lease {
                holder: agent(1, "lead"),
                claim: ev,
                lamport: Lamport(1),
                expires_at_ms: 5_000,
            },
        );
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
        let out = render_status(&id, &cfg, &v, 10_000);
        assert!(out.contains("room alpha (abababababab)"), "{out}");
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
        );
        assert!(
            out.contains("(none seen yet)") && out.contains("(none)"),
            "{out}"
        );
    }

    #[test]
    fn ticket_puts_own_addr_first_and_skips_self_in_known() {
        let me = EndpointAddr::new(iroh::SecretKey::from_bytes(&[1; 32]).public());
        let other = EndpointAddr::new(iroh::SecretKey::from_bytes(&[2; 32]).public());
        let t = make_ticket(
            RoomSecret::from_bytes([9; 32]),
            me.clone(),
            vec![me.clone(), other.clone()],
        );
        assert_eq!(t.bootstrap, vec![me, other]);
    }
}
