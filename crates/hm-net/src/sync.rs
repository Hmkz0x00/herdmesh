//! Anti-entropy sync on ALPN `herdmesh/sync/1` (ADR-001 note 1).
//!
//! Gossip is best-effort with no history, so late joiners and returning peers
//! pull what they lack directly from a member:
//!
//! - One bi-stream per request. Client sends [`SyncSince`] (room, per-author
//!   max-lamport vector, auth tag); server streams [`SyncMsg::Event`] frames
//!   for events the client lacks, then [`SyncMsg::End`] `{ more }`. Responses
//!   are capped ([`SyncCaps`]); the client re-asks while `more`.
//! - Frame: `u32 BE length || SYNC_VERSION || postcard(msg)`. The server reads
//!   requests with [`MAX_REQUEST_FRAME`]; only clients accept the larger
//!   [`MAX_RESPONSE_FRAME`].
//! - **Auth.** `RoomId` is public (it is in every event), so the request
//!   carries [`sync_auth`]: a keyed blake3 over (requester id, server id,
//!   room) under a key derived from the room secret. The server takes the
//!   requester id from the QUIC connection, so a captured request can't be
//!   replayed from another endpoint or to another server. Any failure (bad
//!   tag, unknown room, oversized or malformed request) resets the stream
//!   with [`ERR_REFUSED`] and sends no data.
//! - **Budgets** ([`SyncLimits`], ADR-002 §3). Every frame read and every
//!   request has a deadline; each side caps the rounds and bytes of one
//!   connection (server) or sync run (client); the server serves at most
//!   `concurrent_requests` streams of one connection at a time. A peer that
//!   goes silent, drips bytes or never ends a response is cut off.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use hm_proto::{Event, Lamport, NodeId, RoomId};
use hm_store::{Store, StoreError, SyncCaps};
use iroh::endpoint::{
    ClosedStream, ConnectError, Connection, ConnectionError, ReadError, ReadExactError, RecvStream,
    SendStream, VarInt, WriteError,
};
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointAddr, EndpointId};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::net::SYNC_ALPN;
use crate::ticket::RoomSecret;

/// Version byte at the start of every frame payload.
pub const SYNC_VERSION: u8 = 1;
/// Largest request frame a server reads (before auth, so kept small).
pub const MAX_REQUEST_FRAME: usize = 64 << 10;
/// Largest response frame (one event) a client reads.
pub const MAX_RESPONSE_FRAME: usize = 9 << 20;
/// Client-side limits on one response, regardless of server caps.
pub const MAX_RESPONSE_EVENTS: usize = SyncCaps::DEFAULT.max_events;
pub const MAX_RESPONSE_BYTES: usize = 2 * SyncCaps::DEFAULT.max_bytes;
/// Default cap on request rounds per connection (server) / sync run (client).
pub const MAX_ROUNDS: usize = 1024;
/// Default cap on event bytes per connection (server) / sync run (client).
pub const MAX_SYNC_BYTES: usize = 256 << 20;
/// Stream reset code for every refused request.
pub const ERR_REFUSED: u32 = 1;

const AUTH_CONTEXT: &str = "herdmesh sync auth v1";

/// Time and volume budgets for sync (ADR-002 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncLimits {
    /// Server: streams of one connection served at the same time.
    pub concurrent_requests: usize,
    /// Both: one request, from opening the stream to the response's `End`.
    pub request_deadline: Duration,
    /// Both: reading one frame, first byte to last.
    pub frame_deadline: Duration,
    /// Client: one whole [`sync_from_with`] run, connect included.
    pub total_deadline: Duration,
    /// Server: requests per connection. Client: rounds per run.
    pub max_rounds: usize,
    /// Server: event bytes sent per connection. Client: received per run.
    pub max_bytes: usize,
}

impl SyncLimits {
    pub const DEFAULT: Self = Self {
        concurrent_requests: 2,
        request_deadline: Duration::from_secs(10),
        frame_deadline: Duration::from_secs(5),
        total_deadline: Duration::from_secs(120),
        max_rounds: MAX_ROUNDS,
        max_bytes: MAX_SYNC_BYTES,
    };
}

impl Default for SyncLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// A store shared between the sync server, the client and the daemon.
pub type SharedStore = Arc<Mutex<Store>>;

/// Client request: "send me what I lack in `room`".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSince {
    pub room: RoomId,
    /// Highest lamport the client holds per author node.
    pub have: Vec<(NodeId, Lamport)>,
    /// [`sync_auth`]`(secret, requester, server, room)`.
    pub auth: [u8; 32],
}

/// Server response frames.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncMsg {
    /// One event in `Event::encode` wire form.
    Event(#[serde(with = "serde_bytes")] Vec<u8>),
    /// End of this response; `more` = ask again with an updated vector.
    End { more: bool },
    /// Sent only after a request authenticated; refusals before that are a
    /// stream reset.
    Refused(Refusal),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Stream reset with [`ERR_REFUSED`]: bad auth, unknown room, or bad request.
    Unauthorized,
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
    #[error("secret does not match the store's room")]
    RoomMismatch,
    #[error("protocol violation: {0}")]
    Protocol(&'static str),
    #[error("store lock poisoned")]
    Poisoned,
    #[error("blocking task failed: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("timed out: {0}")]
    Timeout(&'static str),
}

impl<T> From<PoisonError<T>> for SyncError {
    fn from(_: PoisonError<T>) -> Self {
        Self::Poisoned
    }
}

/// Run `fut` within `limit`, else [`SyncError::Timeout`]`(what)`.
async fn within<T>(
    limit: Duration,
    what: &'static str,
    fut: impl Future<Output = Result<T, SyncError>>,
) -> Result<T, SyncError> {
    tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| SyncError::Timeout(what))?
}

/// Proof of the room secret bound to both QUIC identities:
/// `keyed_hash(derive_key(AUTH_CONTEXT, secret), requester || server || room)`.
pub fn sync_auth(
    secret: &RoomSecret,
    requester: EndpointId,
    server: EndpointId,
    room: RoomId,
) -> [u8; 32] {
    let key = blake3::derive_key(AUTH_CONTEXT, secret.as_bytes());
    let mut msg = [0u8; 96];
    msg[..32].copy_from_slice(requester.as_bytes());
    msg[32..64].copy_from_slice(server.as_bytes());
    msg[64..].copy_from_slice(room.as_bytes());
    *blake3::keyed_hash(&key, &msg).as_bytes()
}

/// Write one length-prefixed, versioned postcard frame.
pub async fn write_frame<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), SyncError> {
    let mut payload = vec![SYNC_VERSION];
    payload.extend_from_slice(&postcard::to_allocvec(msg)?);
    let len = u32::try_from(payload.len())
        .ok()
        .filter(|_| payload.len() <= MAX_RESPONSE_FRAME)
        .ok_or(SyncError::FrameTooLarge(payload.len()))?;
    send.write_all(&len.to_be_bytes()).await?;
    send.write_all(&payload).await?;
    Ok(())
}

/// Read one frame written by [`write_frame`] within the default
/// [`SyncLimits::frame_deadline`]. The length prefix is checked against
/// `max` before anything is allocated.
pub async fn read_frame<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<T, SyncError> {
    read_frame_within(recv, max, SyncLimits::DEFAULT.frame_deadline).await
}

/// [`read_frame`] with an explicit deadline for the whole frame, so a peer
/// dripping one byte at a time can't hold the stream.
pub async fn read_frame_within<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
    deadline: Duration,
) -> Result<T, SyncError> {
    within(deadline, "frame read", read_frame_untimed(recv, max)).await
}

async fn read_frame_untimed<T: DeserializeOwned>(
    recv: &mut RecvStream,
    max: usize,
) -> Result<T, SyncError> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await?;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > max {
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

#[derive(Debug)]
struct ServedRoom {
    /// Never logged (`RoomSecret`'s Debug is redacted).
    secret: RoomSecret,
    store: SharedStore,
}

#[derive(Debug, Default)]
struct RoomsInner {
    rooms: HashMap<RoomId, ServedRoom>,
    caps: Option<SyncCaps>,
    limits: Option<SyncLimits>,
}

/// Rooms this node serves over sync. Cheap to clone; shared with the handler.
#[derive(Debug, Clone, Default)]
pub struct SyncRooms(Arc<RwLock<RoomsInner>>);

impl SyncRooms {
    /// Serve `store` to peers proving `secret`. The store must be for
    /// `secret`'s room. Replaces an earlier registration.
    pub fn insert(&self, secret: RoomSecret, store: SharedStore) -> Result<(), SyncError> {
        let room = store.lock()?.room();
        if room != secret.room_id() {
            return Err(SyncError::RoomMismatch);
        }
        self.0
            .write()?
            .rooms
            .insert(room, ServedRoom { secret, store });
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

    /// Server budgets for new connections (default [`SyncLimits::DEFAULT`]).
    /// Zero counts are raised to 1.
    pub fn set_limits(&self, limits: SyncLimits) {
        let limits = SyncLimits {
            concurrent_requests: limits.concurrent_requests.max(1),
            max_rounds: limits.max_rounds.max(1),
            max_bytes: limits.max_bytes.max(1),
            ..limits
        };
        if let Ok(mut g) = self.0.write() {
            g.limits = Some(limits);
        }
    }

    fn limits(&self) -> SyncLimits {
        self.0
            .read()
            .ok()
            .and_then(|g| g.limits)
            .unwrap_or_default()
    }

    fn is_empty(&self) -> bool {
        self.0.read().map_or(true, |g| g.rooms.is_empty())
    }

    /// The room's store if `auth` is valid for (requester, server, room).
    fn authorize(
        &self,
        req: &SyncSince,
        requester: EndpointId,
        server: EndpointId,
    ) -> Result<Option<(SharedStore, SyncCaps)>, SyncError> {
        let g = self.0.read()?;
        let Some(served) = g.rooms.get(&req.room) else {
            return Ok(None);
        };
        let expected = sync_auth(&served.secret, requester, server, req.room);
        // blake3::Hash equality is constant-time.
        if blake3::Hash::from_bytes(expected) != blake3::Hash::from_bytes(req.auth) {
            return Ok(None);
        }
        Ok(Some((served.store.clone(), g.caps.unwrap_or_default())))
    }
}

/// `ProtocolHandler` for [`SYNC_ALPN`].
#[derive(Debug, Clone)]
pub struct SyncHandler {
    rooms: SyncRooms,
    /// This server's endpoint id, bound into every auth tag.
    local: EndpointId,
}

impl SyncHandler {
    pub fn new(rooms: SyncRooms, local: EndpointId) -> Self {
        Self { rooms, local }
    }
}

/// Close both directions of the stream with [`ERR_REFUSED`] and no data.
fn refuse(send: &mut SendStream, recv: &mut RecvStream) {
    let code = VarInt::from_u32(ERR_REFUSED);
    send.reset(code).ok();
    recv.stop(code).ok();
}

/// Per-connection server state shared by its concurrent request tasks.
#[derive(Debug)]
struct ConnBudget {
    rooms: SyncRooms,
    local: EndpointId,
    remote: EndpointId,
    limits: SyncLimits,
    /// Event bytes this connection may still be sent.
    bytes_left: AtomicUsize,
}

impl ConnBudget {
    /// Take exactly `want` bytes if that many are left.
    fn take(&self, want: usize) -> bool {
        let mut left = self.bytes_left.load(Ordering::SeqCst);
        loop {
            let Some(rest) = left.checked_sub(want) else {
                return false;
            };
            match self.bytes_left.compare_exchange_weak(
                left,
                rest,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(now) => left = now,
            }
        }
    }
}

/// Serve one stream within the request deadline; reset it on any failure so
/// a truncated response never looks complete.
async fn serve_request(budget: Arc<ConnBudget>, mut send: SendStream, mut recv: RecvStream) {
    let limit = budget.limits.request_deadline;
    let res = within(
        limit,
        "sync request",
        serve_stream(&budget, &mut send, &mut recv),
    )
    .await;
    if let Err(e) = res {
        tracing::debug!(error = %e, "sync stream failed");
        refuse(&mut send, &mut recv);
    }
}

async fn serve_stream(
    budget: &ConnBudget,
    send: &mut SendStream,
    recv: &mut RecvStream,
) -> Result<(), SyncError> {
    let req =
        read_frame_within::<SyncSince>(recv, MAX_REQUEST_FRAME, budget.limits.frame_deadline).await;
    let authorized = match req {
        Ok(req) => budget
            .rooms
            .authorize(&req, budget.remote, budget.local)?
            .map(|(store, caps)| (store, caps, req.have)),
        Err(e) => {
            tracing::debug!(error = %e, "bad sync request");
            None
        }
    };
    let Some((store, caps, have)) = authorized else {
        refuse(send, recv);
        return Ok(());
    };
    let left = budget.bytes_left.load(Ordering::SeqCst);
    if left == 0 {
        tracing::debug!("sync connection byte budget spent");
        refuse(send, recv);
        return Ok(());
    }
    let caps = SyncCaps {
        max_bytes: caps.max_bytes.min(left),
        ..caps
    };
    let have: BTreeMap<NodeId, Lamport> = have.into_iter().collect();
    let answer = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
        Ok(store.lock()?.missing_events(&have, caps)?)
    })
    .await?;
    let (mut events, mut more) = match answer {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "sync query failed");
            write_frame(send, &SyncMsg::Refused(Refusal::Internal)).await?;
            send.finish()?;
            return Ok(());
        }
    };
    // The store may send one oversized event alone; the connection budget is
    // strict, so keep only what fits.
    let (mut fit, mut total) = (0usize, 0usize);
    for wire in &events {
        let next = total.saturating_add(wire.len());
        if next > left {
            break;
        }
        total = next;
        fit += 1;
    }
    if fit < events.len() {
        events.truncate(fit);
        more = true;
    }
    if (events.is_empty() && more) || !budget.take(total) {
        tracing::debug!("sync connection byte budget too small");
        refuse(send, recv);
        return Ok(());
    }
    tracing::debug!(count = events.len(), more, "sync response");
    for wire in events {
        write_frame(send, &SyncMsg::Event(wire)).await?;
    }
    write_frame(send, &SyncMsg::End { more }).await?;
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
        let limits = self.rooms.limits();
        let budget = Arc::new(ConnBudget {
            rooms: self.rooms.clone(),
            local: self.local,
            remote: conn.remote_id(),
            limits,
            bytes_left: AtomicUsize::new(limits.max_bytes),
        });
        let slots = Arc::new(tokio::sync::Semaphore::new(limits.concurrent_requests));
        let mut tasks = tokio::task::JoinSet::new();
        let mut rounds = 0usize;
        // Serve requests until the client closes the connection. A slot is
        // taken before accepting, so extra streams wait unaccepted.
        loop {
            while tasks.try_join_next().is_some() {}
            let Ok(slot) = slots.clone().acquire_owned().await else {
                break;
            };
            let Ok((mut send, mut recv)) = conn.accept_bi().await else {
                break;
            };
            rounds += 1;
            if rounds > limits.max_rounds {
                tracing::debug!("sync connection round budget spent");
                refuse(&mut send, &mut recv);
                break;
            }
            let budget = budget.clone();
            tasks.spawn(async move {
                serve_request(budget, send, recv).await;
                drop(slot);
            });
        }
        // In-flight requests are each bounded by the request deadline.
        while tasks.join_next().await.is_some() {}
        conn.close(0u32.into(), b"done");
        Ok(())
    }
}

/// Counters from a sync run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub rounds: usize,
    /// Event frames received.
    pub received: usize,
    /// Event bytes received.
    pub bytes: usize,
    pub inserted: usize,
    pub duplicates: usize,
    /// Undecodable, unverifiable or wrong-room events dropped.
    pub rejected: usize,
}

/// Map a stream reset/stop with [`ERR_REFUSED`] to [`SyncError::Refused`].
fn refused_or(e: SyncError) -> SyncError {
    let refused = VarInt::from_u32(ERR_REFUSED);
    match e {
        SyncError::Read(ReadExactError::ReadError(ReadError::Reset(code)))
        | SyncError::Write(WriteError::Stopped(code))
            if code == refused =>
        {
            SyncError::Refused(Refusal::Unauthorized)
        }
        e => e,
    }
}

/// Send one request and collect its response, without deadlines.
async fn request_round(
    conn: &Connection,
    req: &SyncSince,
    stats: &mut SyncStats,
    limits: &SyncLimits,
) -> Result<(Vec<Event>, bool), SyncError> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(&mut send, req).await.map_err(refused_or)?;
    send.finish()?;

    let mut events = Vec::new();
    let (mut frames, mut bytes) = (0usize, 0usize);
    let more = loop {
        let msg =
            read_frame_within::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME, limits.frame_deadline)
                .await
                .map_err(refused_or)?;
        match msg {
            SyncMsg::Event(wire) => {
                stats.received += 1;
                stats.bytes = stats.bytes.saturating_add(wire.len());
                frames += 1;
                bytes = bytes.saturating_add(wire.len());
                if frames > MAX_RESPONSE_EVENTS || bytes > MAX_RESPONSE_BYTES {
                    return Err(SyncError::Protocol("response exceeds limits"));
                }
                if stats.bytes > limits.max_bytes {
                    return Err(SyncError::Protocol("sync byte budget exceeded"));
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
    Ok((events, more))
}

/// One request/response round within `limits.request_deadline`. `local` is
/// this node's endpoint id. Returns whether another round is worthwhile
/// (server said `more` and we progressed).
pub async fn sync_once(
    conn: &Connection,
    local: EndpointId,
    secret: &RoomSecret,
    store: &SharedStore,
    stats: &mut SyncStats,
    limits: &SyncLimits,
) -> Result<bool, SyncError> {
    let req = {
        let s = store.lock()?;
        let room = s.room();
        if room != secret.room_id() {
            return Err(SyncError::RoomMismatch);
        }
        SyncSince {
            room,
            have: s.have()?.into_iter().collect(),
            auth: sync_auth(secret, local, conn.remote_id(), room),
        }
    };
    let (events, more) = within(
        limits.request_deadline,
        "sync request",
        request_round(conn, &req, stats, limits),
    )
    .await?;
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
/// server that keeps saying `more` can't loop us), or after
/// `limits.max_rounds`.
pub async fn sync_with(
    conn: &Connection,
    local: EndpointId,
    secret: &RoomSecret,
    store: &SharedStore,
    limits: &SyncLimits,
) -> Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    while stats.rounds < limits.max_rounds
        && sync_once(conn, local, secret, store, &mut stats, limits).await?
    {}
    Ok(stats)
}

/// [`sync_from_with`] using [`SyncLimits::DEFAULT`].
pub async fn sync_from(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    secret: &RoomSecret,
    store: &SharedStore,
) -> Result<SyncStats, SyncError> {
    sync_from_with(endpoint, peer, secret, store, &SyncLimits::DEFAULT).await
}

/// Connect to `peer` on [`SYNC_ALPN`], run [`sync_with`], and close; all
/// within `limits.total_deadline`.
pub async fn sync_from_with(
    endpoint: &Endpoint,
    peer: impl Into<EndpointAddr>,
    secret: &RoomSecret,
    store: &SharedStore,
    limits: &SyncLimits,
) -> Result<SyncStats, SyncError> {
    let peer = peer.into();
    within(limits.total_deadline, "sync total budget", async {
        let conn = endpoint.connect(peer, SYNC_ALPN).await?;
        let result = sync_with(&conn, endpoint.id(), secret, store, limits).await;
        conn.close(0u32.into(), b"done");
        result
    })
    .await
}
