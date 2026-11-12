//! Anti-entropy sync on ALPN `herdmesh/sync/1` (ADR-001 note 1).
//!
//! Gossip is best-effort with no history, so late joiners and returning peers
//! pull what they lack directly from a member:
//!
//! - One bi-stream per request. Client sends [`SyncSince`] (room + per-author
//!   max-lamport vector); server streams [`SyncMsg::Event`] frames for events
//!   the client lacks, then [`SyncMsg::End`] `{ more }`. Responses are capped
//!   ([`SyncCaps`]); the client re-asks with its updated vector while `more`.
//! - Frame: `u32 BE length || SYNC_VERSION || postcard(msg)`.
//! - Server answers only for rooms registered in its [`SyncRooms`], only from
//!   that room's store. Client verifies every event and appends them in one
//!   batch; invalid events are counted and dropped, never fatal.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use hm_proto::{Event, Lamport, NodeId, RoomId};
use hm_store::{Store, StoreError, SyncCaps};
use iroh::endpoint::{
    ClosedStream, ConnectError, Connection, ConnectionError, ReadExactError, RecvStream,
    SendStream, WriteError,
};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::net::SYNC_ALPN;

/// Version byte at the start of every frame payload.
pub const SYNC_VERSION: u8 = 1;
/// Largest frame accepted (one event, or one request).
pub const MAX_FRAME: usize = 9 << 20;
/// Client-side limits on one response, regardless of server caps.
pub const MAX_RESPONSE_EVENTS: usize = SyncCaps::DEFAULT.max_events;
pub const MAX_RESPONSE_BYTES: usize = 2 * SyncCaps::DEFAULT.max_bytes;
/// Upper bound on request rounds per [`sync_with`] call.
pub const MAX_ROUNDS: usize = 1024;

/// A store shared between the sync server, the client and the daemon.
pub type SharedStore = Arc<Mutex<Store>>;

/// Client request: "send me what I lack in `room`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSince {
    pub room: RoomId,
    /// Highest lamport the client holds per author node.
    pub have: Vec<(NodeId, Lamport)>,
}

/// Server response frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMsg {
    /// One event in `Event::encode` wire form.
    Event(#[serde(with = "serde_bytes")] Vec<u8>),
    /// End of this response; `more` = ask again with an updated vector.
    End {
        more: bool,
    },
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    UnknownRoom,
    Internal,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("connect: {0}")]
    Connect(#[from] ConnectError),
    #[error("connection: {0}")]
    Connection(#[from] ConnectionError),
    #[error("write: {0}")]
    Write(#[from] WriteError),
    #[error("read: {0}")]
    Read(#[from] ReadExactError),
    #[error("stream closed: {0}")]
    Closed(#[from] ClosedStream),
    #[error("codec: {0}")]
    Codec(#[from] postcard::Error),
    #[error("store: {0}")]
    Store(#[from] StoreError),
    #[error("unsupported sync version {0}")]
    UnsupportedVersion(u8),
    #[error("frame of {0} bytes exceeds limit")]
    FrameTooLarge(usize),
    #[error("peer refused: {0:?}")]
    Refused(Refusal),
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("store lock poisoned")]
    Poisoned,
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
}

impl<T> From<PoisonError<T>> for SyncError {
    fn from(_: PoisonError<T>) -> Self {
        Self::Poisoned
    }
}

/// Write one length-prefixed, versioned postcard frame.
pub async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), SyncError> {
    let mut payload = vec![SYNC_VERSION];
    payload.extend_from_slice(&postcard::to_allocvec(msg)?);
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= MAX_FRAME)
        .ok_or(SyncError::FrameTooLarge(payload.len()))?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&payload).await?;
    Ok(())
}

/// Read one frame written by [`write_frame`].
pub async fn read_frame<T: DeserializeOwned>(recv: &mut RecvStream) -> Result<T, SyncError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > MAX_FRAME {
        return Err(SyncError::FrameTooLarge(len));
    }
    let mut payload = vec![0u8; len];
    recv.read_exact(&mut payload).await?;
    match payload.split_first() {
        Some((&SYNC_VERSION, rest)) => {
            let (msg, tail) = postcard::take_from_bytes(rest)?;
            if !tail.is_empty() {
                return Err(SyncError::Protocol("trailing bytes in frame"));
            }
            Ok(msg)
        }
        Some((&v, _)) => Err(SyncError::UnsupportedVersion(v)),
        None => Err(SyncError::Protocol("empty frame")),
    }
}

#[derive(Debug, Default)]
struct RoomsInner {
    rooms: HashMap<RoomId, SharedStore>,
    caps: Option<SyncCaps>,
}

/// Rooms this node serves over sync. Cheap to clone; shared with the handler.
#[derive(Debug, Clone, Default)]
pub struct SyncRooms(Arc<RwLock<RoomsInner>>);

impl SyncRooms {
    /// Serve `store`'s room from `store`. Replaces an earlier registration.
    pub fn insert(&self, store: SharedStore) -> Result<(), SyncError> {
        let room = store.lock()?.room();
        self.0.write()?.rooms.insert(room, store);
        Ok(())
    }

    pub fn remove(&self, room: &RoomId) -> Result<(), SyncError> {
        self.0.write()?.rooms.remove(room);
        Ok(())
    }

    /// Response caps (default [`SyncCaps::DEFAULT`]). Never above the default.
    pub fn set_caps(&self, caps: SyncCaps) {
        let caps = SyncCaps {
            max_events: caps.max_events.clamp(1, SyncCaps::DEFAULT.max_events),
            max_bytes: caps.max_bytes.clamp(1, SyncCaps::DEFAULT.max_bytes),
        };
        if let Ok(mut g) = self.0.write() {
            g.caps = Some(caps);
        }
    }

    fn is_empty(&self) -> bool {
        self.0.read().map_or(true, |g| g.rooms.is_empty())
    }

    fn lookup(&self, room: &RoomId) -> Result<(Option<SharedStore>, SyncCaps), SyncError> {
        let g = self.0.read()?;
        Ok((g.rooms.get(room).cloned(), g.caps.unwrap_or_default()))
    }
}

/// `ProtocolHandler` for [`SYNC_ALPN`].
#[derive(Debug, Clone)]
pub struct SyncHandler {
    rooms: SyncRooms,
}

impl SyncHandler {
    pub fn new(rooms: SyncRooms) -> Self {
        Self { rooms }
    }
}

async fn serve_stream(
    rooms: &SyncRooms,
    mut send: SendStream,
    mut recv: RecvStream,
) -> Result<(), SyncError> {
    let req: SyncSince = read_frame(&mut recv).await?;
    let (store, caps) = rooms.lookup(&req.room)?;
    let Some(store) = store else {
        write_frame(&mut send, &SyncMsg::Refused(Refusal::UnknownRoom)).await?;
        send.finish()?;
        return Ok(());
    };
    let have: BTreeMap<NodeId, Lamport> = req.have.into_iter().collect();
    let answer = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
        Ok(store.lock()?.missing_events(&have, caps)?)
    })
    .await?;
    let (events, more) = match answer {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "sync query failed");
            write_frame(&mut send, &SyncMsg::Refused(Refusal::Internal)).await?;
            send.finish()?;
            return Ok(());
        }
    };
    tracing::debug!(count = events.len(), more, "sync response");
    for wire in events {
        write_frame(&mut send, &SyncMsg::Event(wire)).await?;
    }
    write_frame(&mut send, &SyncMsg::End { more }).await?;
    send.finish()?;
    Ok(())
}

impl ProtocolHandler for SyncHandler {
    async fn accept(&self, conn: Connection) -> Result<(), AcceptError> {
        if self.rooms.is_empty() {
            conn.close(1u32.into(), b"no rooms served");
            return Err(AcceptError::from_err(std::io::Error::other(
                "herdmesh/sync/1: no rooms served",
            )));
        }
        // Serve requests until the client closes the connection.
        while let Ok((send, recv)) = conn.accept_bi().await {
            if let Err(e) = serve_stream(&self.rooms, send, recv).await {
                tracing::debug!(error = %e, "sync stream failed");
                break;
            }
        }
        Ok(())
    }
}

/// Counters from a sync run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub rounds: usize,
    /// Event frames received.
    pub received: usize,
    pub inserted: usize,
    pub duplicates: usize,
    /// Undecodable, unverifiable or wrong-room events dropped.
    pub rejected: usize,
}

/// One request/response round. Returns the server's `more` flag.
pub async fn sync_once(
    conn: &Connection,
    store: &SharedStore,
    stats: &mut SyncStats,
) -> Result<bool, SyncError> {
    let req = {
        let s = store.lock()?;
        SyncSince {
            room: s.room(),
            have: s.have()?.into_iter().collect(),
        }
    };
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(&mut send, &req).await?;
    send.finish()?;

    let mut events = Vec::new();
    let (mut frames, mut bytes) = (0usize, 0usize);
    let more = loop {
        match read_frame::<SyncMsg>(&mut recv).await? {
            SyncMsg::Event(wire) => {
                stats.received += 1;
                frames += 1;
                bytes = bytes.saturating_add(wire.len());
                if frames > MAX_RESPONSE_EVENTS || bytes > MAX_RESPONSE_BYTES {
                    return Err(SyncError::Protocol("response exceeds limits"));
                }
                match Event::decode(&wire) {
                    Ok(e) => events.push(e),
                    Err(_) => stats.rejected += 1,
                }
            }
            SyncMsg::End { more } => break more,
            SyncMsg::Refused(r) => return Err(SyncError::Refused(r)),
        }
    };
    stats.rounds += 1;

    let store = store.clone();
    let report = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
        Ok(store.lock()?.append_batch(&events)?)
    })
    .await??;
    stats.inserted += report.inserted;
    stats.duplicates += report.duplicates;
    stats.rejected += report.rejected;
    tracing::debug!(
        inserted = report.inserted,
        rejected = report.rejected,
        more,
        "sync round"
    );
    Ok(more && report.inserted > 0)
}

/// Pull everything `conn`'s peer has that `store` lacks.
///
/// Stops when the server says no `more`, when a round adds nothing (so a
/// server that keeps saying `more` can't loop us), or after [`MAX_ROUNDS`].
pub async fn sync_with(conn: &Connection, store: &SharedStore) -> Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    while stats.rounds < MAX_ROUNDS && sync_once(conn, store, &mut stats).await? {}
    Ok(stats)
}

/// Connect to `peer` on [`SYNC_ALPN`], run [`sync_with`], and close.
pub async fn sync_from(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    store: &SharedStore,
) -> Result<SyncStats, SyncError> {
    let conn = endpoint.connect(peer, SYNC_ALPN).await?;
    let result = sync_with(&conn, store).await;
    conn.close(0u32.into(), b"done");
    result
}
