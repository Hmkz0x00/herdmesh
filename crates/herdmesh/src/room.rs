//! `herdmesh room ...` commands. Each returns the text to print on stdout.
//!
//! Tickets contain the room secret: they are only ever returned as command
//! output (stdout), never logged.

use std::future::Future;
use std::time::Duration;

use hm_net::{RoomSecret, RoomTicket};
use hm_proto::{EventBody, NodeId, Role, RoomId};
use hm_store::Views;

use crate::data::{DataDir, RoleName, RoomConfig};
use crate::display::{NAME_MAX, sanitize_display};
use crate::session::{
    RoomSession, SessionError, bind_net, init_genesis, make_ticket, now_ms, own_addr, pin_genesis,
    render_status, room_genesis,
};

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error(transparent)]
    Data(#[from] crate::data::DataError),
    #[error("invalid ticket: {0}")]
    Ticket(#[from] hm_net::TicketError),
    #[error("could not generate room secret: {0}")]
    Random(#[from] hm_net::RandomError),
    #[error("store: {0}")]
    Store(#[from] hm_store::StoreError),
    #[error("role must be owner, member or observer (got '{0}')")]
    BadRole(String),
    #[error("only the room owner can grant roles; this node is not the owner ({0})")]
    NotOwner(String),
    #[error("no known node matches '{0}' (use at least 4 hex chars from `room status`)")]
    UnknownNode(String),
    #[error("'{0}' matches several nodes; use a longer prefix")]
    AmbiguousNode(String),
    #[error("the room owner role cannot be changed")]
    GrantToOwner,
}

const ONLINE_WAIT: Duration = Duration::from_secs(5);

/// Default agent label for events authored from the CLI.
pub const DEFAULT_AGENT: &str = "cli";

fn ticket_block(t: &RoomTicket) -> String {
    format!("Ticket (secret, share only with room members):\n{t}\n")
}

/// A room we just created.
pub struct Created {
    pub id: RoomId,
    pub node: String,
    pub ticket: RoomTicket,
}

/// Create a room: new secret, local state; returns the ticket.
pub async fn create_with_ticket(
    data: &DataDir,
    name: &str,
    agent: &str,
) -> Result<Created, RoomError> {
    hm_proto::names::check_name(agent).map_err(SessionError::from)?;
    let relay = data.load_config()?.relay()?;
    let secret = RoomSecret::generate()?;
    let id = data.create_room(&secret, &RoomConfig::new(name, RoleName::Owner, agent), &[])?;
    let genesis = init_genesis(data, id, agent)?;
    let net = bind_net(data, relay.clone()).await?;
    let own = own_addr(&net, &relay, ONLINE_WAIT).await;
    let node = net.endpoint().id();
    net.shutdown().await;
    let ticket = make_ticket(secret, genesis, own, vec![]);
    Ok(Created {
        id,
        node: node.to_string(),
        ticket,
    })
}

/// `room create <name>`: new secret, local state, prints a ticket.
pub async fn create(data: &DataDir, name: &str, agent: &str) -> Result<String, RoomError> {
    let made = create_with_ticket(data, name, agent).await?;
    Ok(format!(
        "Created room '{name}' (id: {})\nNode: {}\n{}Run `herdmesh room run {name}` to go online.\n",
        made.id,
        made.node,
        ticket_block(&made.ticket)
    ))
}

/// `room invite <room>`: ticket with our current address + known peers.
pub async fn invite(data: &DataDir, room: &str, copy: bool) -> Result<String, RoomError> {
    let relay = data.load_config()?.relay()?;
    let (id, cfg) = data.resolve_room(room)?;
    let secret = data.load_secret(&id, &cfg)?;
    let peers = data.load_peers(&id)?;
    let genesis = room_genesis(data, id)?;
    let net = bind_net(data, relay.clone()).await?;
    let own = own_addr(&net, &relay, ONLINE_WAIT).await;
    net.shutdown().await;
    let ticket = make_ticket(secret, genesis, own, peers);
    let mut out = ticket_block(&ticket);
    if copy {
        out.push_str(&copy_note(crate::clipboard::copy(&ticket.to_string())));
    }
    Ok(out)
}

/// One line telling the user whether the clipboard copy worked.
pub fn copy_note(used: Option<&str>) -> String {
    match used {
        Some(_) => "Copied the ticket to the clipboard.\n".to_owned(),
        None => "No clipboard tool found; copy the ticket above by hand.\n".to_owned(),
    }
}

/// `room join <ticket> [--as <agent>] [--name <name>]`: local state only.
pub fn join(
    data: &DataDir,
    ticket: &str,
    agent: &str,
    name: Option<&str>,
) -> Result<String, RoomError> {
    hm_proto::names::check_name(agent).map_err(SessionError::from)?;
    let t: RoomTicket = ticket.trim().parse()?;
    let id = t.room_id;
    let name = name.map_or_else(|| format!("room-{}", &id.to_string()[..8]), str::to_owned);
    data.create_room(
        &t.secret,
        &RoomConfig::new(&name, RoleName::Member, agent),
        &t.bootstrap,
    )?;
    // Roles are anchored to the creator's Genesis; it arrives by sync.
    pin_genesis(data, id, t.genesis)?;
    Ok(format!(
        "Joined room '{name}' (id: {id}) as {agent}, {} bootstrap peer(s).\nRun `herdmesh room run {name}` to connect.\n",
        t.bootstrap.len()
    ))
}

/// `room leave <room> [--yes]`: publish Leave; delete local state only with `yes`.
pub async fn leave(data: &DataDir, room: &str, yes: bool) -> Result<String, RoomError> {
    let relay = data.load_config()?.relay()?;
    let (id, cfg) = data.resolve_room(room)?;
    let mut s = RoomSession::open(data, id, relay).await?;
    // Best effort: give peers a chance to be connected before announcing.
    let reached = tokio::time::timeout(ONLINE_WAIT, s.joined()).await.is_ok();
    s.publish(EventBody::Leave).await?;
    if reached {
        // Let the broadcast flush before the endpoint closes.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    s.close().await;
    let mut out = format!(
        "Published Leave for '{}'{}.\n",
        cfg.name,
        if reached {
            ""
        } else {
            " (no peers online; they will not see it until sync)"
        }
    );
    if yes {
        data.remove_room(&id)?;
        out.push_str("Removed local room state.\n");
    } else {
        out.push_str("Local state kept. Re-run with --yes to delete it.\n");
    }
    Ok(out)
}

/// `room status <room>`: members, leases, open tasks from the local store.
pub fn status(data: &DataDir, room: &str) -> Result<String, RoomError> {
    let (id, cfg) = data.resolve_room(room)?;
    let store = hm_store::Store::open(data.events_db(&id), id)?;
    Ok(render_status(
        &id,
        &cfg,
        store.views(),
        store.event_count()?,
        &data.load_sync(&id)?,
        now_ms(),
    ))
}

/// Parse `owner|member|observer`.
pub fn parse_role(s: &str) -> Result<Role, RoomError> {
    match s {
        "owner" => Ok(Role::Owner),
        "member" => Ok(Role::Member),
        "observer" => Ok(Role::Observer),
        other => Err(RoomError::BadRole(sanitize_display(other, NAME_MAX))),
    }
}

/// Check a grant before going online: we must be the verified owner, and
/// `prefix` must name exactly one node the room has seen. Returns that node.
pub fn plan_grant(views: &Views, me: &NodeId, prefix: &str) -> Result<NodeId, RoomError> {
    if views.owner.as_ref() != Some(me) {
        let owner = views.owner.map_or_else(
            || "unverified".to_owned(),
            |o| o.to_string()[..6].to_owned(),
        );
        return Err(RoomError::NotOwner(owner));
    }
    let p = prefix.trim().to_ascii_lowercase();
    let shown = sanitize_display(prefix, NAME_MAX);
    if p.len() < 4 || !p.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(RoomError::UnknownNode(shown));
    }
    let mut nodes: Vec<NodeId> = views
        .members
        .keys()
        .map(|a| a.node)
        .chain(views.roles.keys().copied())
        .chain(views.owner)
        .filter(|n| n.to_string().starts_with(&p))
        .collect();
    nodes.sort();
    nodes.dedup();
    match nodes.as_slice() {
        [] => Err(RoomError::UnknownNode(shown)),
        [n] if views.owner.as_ref() == Some(n) => Err(RoomError::GrantToOwner),
        [n] => Ok(*n),
        _ => Err(RoomError::AmbiguousNode(shown)),
    }
}

/// `room grant <room> <node-prefix> <role>`: publish a signed `RoleGrant`.
/// Fails with a clear error unless this node is the room owner.
pub async fn grant(
    data: &DataDir,
    room: &str,
    node_prefix: &str,
    role: &str,
) -> Result<String, RoomError> {
    let role = parse_role(role)?;
    let relay = data.load_config()?.relay()?;
    let (id, cfg) = data.resolve_room(room)?;
    let me = NodeId::from_verifying_key(&crate::session::node_key(data)?.verifying_key());
    let target = {
        let store = hm_store::Store::open(data.events_db(&id), id)?;
        plan_grant(store.views(), &me, node_prefix)?
    };
    let mut s = RoomSession::open(data, id, relay).await?;
    let reached = tokio::time::timeout(ONLINE_WAIT, s.joined()).await.is_ok();
    s.publish(EventBody::RoleGrant { node: target, role })
        .await?;
    if reached {
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    s.close().await;
    Ok(format!(
        "Granted {role:?} to node {} in '{}'{}.\n",
        &target.to_string()[..6],
        sanitize_display(&cfg.name, NAME_MAX),
        if reached {
            ""
        } else {
            " (no peers online; they will see it after sync)"
        }
    ))
}

/// `room run <room>`: foreground session until `shutdown` resolves.
/// Each line is passed to `out` (stdout in the CLI).
pub async fn run(
    data: &DataDir,
    room: &str,
    shutdown: impl Future<Output = ()>,
    mut out: impl FnMut(&str),
) -> Result<(), RoomError> {
    let relay = data.load_config()?.relay()?;
    let (id, cfg) = data.resolve_room(room)?;
    let mut s = RoomSession::open(data, id, relay).await?;
    out(&format!(
        "Online in '{}' as {} (node {}). Ctrl+C to exit.",
        sanitize_display(&cfg.name, NAME_MAX),
        sanitize_display(&cfg.agent_name, NAME_MAX),
        s.net().endpoint().id()
    ));
    s.publish(EventBody::Join {
        role: cfg.role.into(),
    })
    .await?;
    tokio::pin!(shutdown);
    let res = loop {
        tokio::select! {
            () = &mut shutdown => break Ok(()),
            step = s.step() => match step {
                Some(Ok(line)) => out(&line),
                Some(Err(e)) => break Err(e.into()),
                None => break Ok(()),
            },
        }
    };
    s.close().await;
    out("Offline.");
    res
}

/// Print the newest `limit` messages, then optionally follow local store updates.
/// Local insertion cursors also catch late events with older Lamport clocks.
pub async fn log(
    data: &DataDir,
    room: &str,
    follow: bool,
    limit: usize,
    shutdown: impl Future<Output = ()>,
    mut out: impl FnMut(&str),
) -> Result<(), RoomError> {
    let (id, _) = data.resolve_room(room)?;
    let store = hm_store::Store::open(data.events_db(&id), id)?;
    let initial = store.events_after(0, usize::MAX)?;
    let mut cursor = initial.last().map_or(0, |(position, _)| *position);
    let lines: Vec<_> = initial
        .iter()
        .filter_map(|(_, e)| hm_daemon::feed::log_line(e))
        .collect();
    for line in &lines[lines.len().saturating_sub(limit)..] {
        out(line);
    }
    if !follow {
        return Ok(());
    }
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            () = tokio::time::sleep(Duration::from_millis(500)) => {
                for (position, e) in store.events_after(cursor, 200)? {
                    cursor = position;
                    if let Some(line) = hm_daemon::feed::log_line(&e) { out(&line); }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hm_proto::{AgentRef, Lamport};
    use hm_store::Member;

    fn node(b: u8) -> NodeId {
        NodeId::from_bytes([b; 32])
    }

    fn views(owner: u8, others: &[u8]) -> Views {
        let mut v = Views {
            owner: Some(node(owner)),
            ..Views::default()
        };
        for &o in others {
            v.members.insert(
                AgentRef::new(node(o), "a"),
                Member {
                    role: Some(Role::Member),
                    present: true,
                    status: None,
                    last_lamport: Lamport(1),
                },
            );
        }
        v
    }

    #[tokio::test]
    async fn log_limits_history_and_follows_late_lower_lamport_once() {
        let temp = tempfile::tempdir().unwrap();
        let data = DataDir::new(temp.path().join("data"));
        let id = data
            .create_room(
                &RoomSecret::generate().unwrap(),
                &RoomConfig::new("alpha", RoleName::Owner, "alice"),
                &[],
            )
            .unwrap();
        let mut store = hm_store::Store::open(data.events_db(&id), id).unwrap();
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let event = |seq, text: &str| {
            hm_proto::Event::sign(
                &key,
                id,
                "alice",
                seq,
                Lamport(if seq == 3 { 1 } else { seq * 10 }),
                seq * 1000,
                EventBody::Note {
                    scope: "test".into(),
                    text: text.into(),
                },
            )
            .unwrap()
        };
        store.append(&event(1, "first")).unwrap();
        store.append(&event(2, "second")).unwrap();
        let mut initial = Vec::new();
        log(&data, "alpha", false, 1, std::future::pending(), |l| {
            initial.push(l.to_owned())
        })
        .await
        .unwrap();
        assert_eq!(initial.len(), 1);
        assert!(initial[0].ends_with("[note] second"));
        let mut lines = Vec::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut stop = Some(tx);
        let run = log(
            &data,
            "alpha",
            true,
            1,
            async {
                let _ = rx.await;
            },
            |l| {
                lines.push(l.to_owned());
                if lines.len() == 1 {
                    store.append(&event(3, "third")).unwrap();
                }
                if lines.len() == 2
                    && let Some(tx) = stop.take()
                {
                    let _ = tx.send(());
                }
            },
        );
        tokio::time::timeout(Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(lines.len(), 2);
        assert!(lines[1].ends_with("[note] third"));
    }

    #[test]
    fn parses_roles() {
        assert_eq!(parse_role("observer").unwrap(), Role::Observer);
        assert_eq!(parse_role("owner").unwrap(), Role::Owner);
        assert!(matches!(parse_role("admin"), Err(RoomError::BadRole(_))));
    }

    #[test]
    fn grant_needs_the_owner() {
        let v = views(1, &[2]);
        let err = plan_grant(&v, &node(2), "020202").unwrap_err();
        assert!(matches!(err, RoomError::NotOwner(_)), "{err}");
        assert!(err.to_string().contains("only the room owner"));
        let mut unverified = views(1, &[2]);
        unverified.owner = None;
        assert!(matches!(
            plan_grant(&unverified, &node(1), "0202"),
            Err(RoomError::NotOwner(_))
        ));
    }

    #[test]
    fn grant_resolves_unique_prefixes_only() {
        let v = views(1, &[2, 3]);
        assert_eq!(plan_grant(&v, &node(1), "0202").unwrap(), node(2));
        assert_eq!(plan_grant(&v, &node(1), "0303").unwrap(), node(3));
        for bad in ["0909", "02", "zzzz"] {
            assert!(
                matches!(
                    plan_grant(&v, &node(1), bad),
                    Err(RoomError::UnknownNode(_))
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            plan_grant(&v, &node(1), "0101"),
            Err(RoomError::GrantToOwner)
        ));
        let mut amb = views(1, &[]);
        let (mut a, mut b) = ([0xaa; 32], [0xaa; 32]);
        a[31] = 1;
        b[31] = 2;
        for n in [a, b] {
            amb.roles.insert(NodeId::from_bytes(n), Role::Member);
        }
        assert!(matches!(
            plan_grant(&amb, &node(1), "aaaa"),
            Err(RoomError::AmbiguousNode(_))
        ));
    }

    #[test]
    fn status_shows_each_members_role_after_a_grant() {
        use crate::data::RoleName;
        let key = |b: u8| ed25519_dalek::SigningKey::from_bytes(&[b; 32]);
        let room = hm_proto::RoomId::from_bytes([5; 32]);
        let sign = |k: u8, seq, lamport, body| {
            hm_proto::Event::sign(&key(k), room, "x", seq, Lamport(lamport), 1, body).unwrap()
        };
        let owner = NodeId::from_verifying_key(&key(1).verifying_key());
        let obs = NodeId::from_verifying_key(&key(2).verifying_key());
        let g = sign(1, 1, 1, EventBody::Genesis { owner });
        let mut v = Views::new(Some(g.id));
        for e in [
            g,
            sign(2, 1, 2, EventBody::Join { role: Role::Member }),
            sign(
                1,
                2,
                3,
                EventBody::RoleGrant {
                    node: obs,
                    role: Role::Observer,
                },
            ),
        ] {
            v.apply(&e);
        }
        let cfg = RoomConfig::new("alpha", RoleName::Owner, "x");
        let sync = crate::data::SyncLog::default();
        let out = crate::session::render_status(&room, &cfg, &v, 3, &sync, 10);
        assert!(out.contains("observer"), "{out}");
    }
}
