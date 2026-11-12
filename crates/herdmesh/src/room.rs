//! `herdmesh room ...` commands. Each returns the text to print on stdout.
//!
//! Tickets contain the room secret: they are only ever returned as command
//! output (stdout), never logged.

use std::future::Future;
use std::time::Duration;

use hm_net::{RoomSecret, RoomTicket};
use hm_proto::EventBody;

use crate::data::{DataDir, RoleName, RoomConfig};
use crate::display::{NAME_MAX, sanitize_display};
use crate::session::{
    RoomSession, SessionError, bind_net, make_ticket, now_ms, own_addr, render_status,
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
}

const ONLINE_WAIT: Duration = Duration::from_secs(5);

/// Default agent label for events authored from the CLI.
pub const DEFAULT_AGENT: &str = "cli";

fn ticket_block(t: &RoomTicket) -> String {
    format!("Ticket (secret, share only with room members):\n{t}\n")
}

/// `room create <name>`: new secret, local state, prints a ticket.
pub async fn create(data: &DataDir, name: &str, agent: &str) -> Result<String, RoomError> {
    let relay = data.load_config()?.relay()?;
    let secret = RoomSecret::generate()?;
    let id = data.create_room(&secret, &RoomConfig::new(name, RoleName::Owner, agent), &[])?;
    let net = bind_net(data, relay.clone()).await?;
    let own = own_addr(&net, &relay, ONLINE_WAIT).await;
    let node = net.endpoint().id();
    net.shutdown().await;
    let t = make_ticket(secret, own, vec![]);
    Ok(format!(
        "Created room '{name}' (id: {id})\nNode: {node}\n{}Run `herdmesh room run {name}` to go online.\n",
        ticket_block(&t)
    ))
}

/// `room invite <room>`: ticket with our current address + known peers.
pub async fn invite(data: &DataDir, room: &str) -> Result<String, RoomError> {
    let relay = data.load_config()?.relay()?;
    let (id, cfg) = data.resolve_room(room)?;
    let secret = data.load_secret(&id, &cfg)?;
    let peers = data.load_peers(&id)?;
    let net = bind_net(data, relay.clone()).await?;
    let own = own_addr(&net, &relay, ONLINE_WAIT).await;
    net.shutdown().await;
    Ok(ticket_block(&make_ticket(secret, own, peers)))
}

/// `room join <ticket> [--as <agent>] [--name <name>]`: local state only.
pub fn join(
    data: &DataDir,
    ticket: &str,
    agent: &str,
    name: Option<&str>,
) -> Result<String, RoomError> {
    let t: RoomTicket = ticket.trim().parse()?;
    let id = t.room_id;
    let name = name.map_or_else(|| format!("room-{}", &id.to_string()[..8]), str::to_owned);
    data.create_room(
        &t.secret,
        &RoomConfig::new(&name, RoleName::Member, agent),
        &t.bootstrap,
    )?;
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
