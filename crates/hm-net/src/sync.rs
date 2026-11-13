//! Anti-entropy sync on ALPN `herdmesh/sync/1` (ADR-001 note 1).
//!
//! Gossip is best-effort with no history, so late joiners and returning peers
//! pull what they lack directly from a member:
//!
//! - **Sparse inventories** (GATE-3 N2/N10, wire v4). The client pages exact
//!   `(seq, id)` pairs in [`SparseCheck`] ranges. The server compares bounded
//!   indexed pages and sends only missing/different events, acknowledging
//!   completed ranges with [`SparseAck`]. Empty sequence spans cost no work.
//!   This detects twins without a contiguous prefix and traverses every gap.
//!   Authors the page does not list but whose id falls in the
//!   page's `(from, to]` range are sent from seq 1, so unknown authors are
//!   discovered. Many authors are split into pages of at most
//!   [`MAX_AUTHORS_PER_REQUEST`], so a request always fits
//!   [`MAX_REQUEST_FRAME`].
//! - One bi-stream per request. Server streams [`SyncMsg::Event`] frames,
//!   then [`SyncMsg::End`] `{ more }`. Responses are capped ([`SyncCaps`]);
//!   the client re-asks while events or acknowledged ranges make progress,
//!   then moves to the next author page. Both cursors persist per peer, so
//!   bounded runs resume instead of revisiting the same unfillable holes.
//!   Completed sweeps wrap to catch later arrivals and replacements.
//! - **Twins.** For each listed author the response also carries the kept
//!   twin of every equivocated seq (at most [`MAX_TWINS_PER_AUTHOR`]); the
//!   client's store keeps the lowest id and records the mark.
//! - **Prefix checks** remain supported for callers with contiguous logs:
//!   an author can carry a [`PrefixCheck`] (the
//!   client's XOR fingerprint over its contiguous prefix). A server that
//!   disagrees replies with a [`SyncMsg::Probe`] of [`PROBE_POINTS`]
//!   fingerprints; the client narrows the range over rounds of the same
//!   page until it is at most [`PROBE_DIRECT`] seqs, which the server then
//!   sends outright, putting both twins in front of one store.
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

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::ops::Bound;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use hm_proto::limits::MAX_EVENT_BYTES;
use hm_proto::{Event, EventId, NodeId, RoomId};
use hm_store::{AuthorSeqs, Store, StoreError, SyncCaps};
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
pub const SYNC_VERSION: u8 = 4;
/// Largest request frame a server reads (before auth, so kept small).
pub const MAX_REQUEST_FRAME: usize = 64 << 10;
/// Largest response frame (one event plus framing) a client reads. Events
/// above [`MAX_EVENT_BYTES`] are refused on sync exactly as on gossip.
pub const MAX_RESPONSE_FRAME: usize = MAX_EVENT_BYTES + 1024;
/// Client-side limits on one response, regardless of server caps.
pub const MAX_RESPONSE_EVENTS: usize = SyncCaps::DEFAULT.max_events;
pub const MAX_RESPONSE_BYTES: usize = 2 * SyncCaps::DEFAULT.max_bytes;
/// Default cap on request rounds per connection (server) / sync run (client).
pub const MAX_ROUNDS: usize = 1024;
/// Default cap on event bytes per connection (server) / sync run (client).
pub const MAX_SYNC_BYTES: usize = 256 << 20;
/// Authors listed in one request page.
pub const MAX_AUTHORS_PER_REQUEST: usize = 64;
/// Held IDs per sparse page; even worst-case requests fit 64 KiB.
pub const SPARSE_IDS: usize = 8;
/// Holes listed per author in one request.
pub const MAX_HOLES_PER_AUTHOR: usize = 16;
/// Twins (kept events at equivocated seqs) served per requested author.
pub const MAX_TWINS_PER_AUTHOR: usize = 64;
/// Legacy explicit-hole windows. Normal sync uses sparse range inventories
/// instead, so this bound cannot hide later gaps.
pub const HOLE_WINDOWS: usize = 8;
/// Authors the server reads per store query while answering.
const SERVER_AUTHOR_PAGE: usize = 64;
/// Points per [`SyncMsg::Probe`] when bisecting a prefix mismatch.
pub const PROBE_POINTS: u64 = 16;
/// A mismatched range this short is answered with its events directly.
pub const PROBE_DIRECT: u64 = 16;
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

/// Exact sparse inventory, or a legacy high-watermark plus explicit holes.
/// `have_hi` alone does not prove coverage of a sparse log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthorWant {
    pub author: NodeId,
    pub have_hi: u64,
    pub holes: Vec<u64>,
    /// Prefix fingerprint check (GATE-2 N2), see [`PrefixCheck`].
    pub check: Option<PrefixCheck>,
    /// Exact inventory for a bounded sparse range, preferred over `holes`.
    pub sparse: Option<SparseCheck>,
}

/// All locally held IDs in `(after, through]`. Absent sequence numbers need
/// no enumeration. `u64::MAX` as the upper bound includes the entire tail.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparseCheck {
    pub after: u64,
    pub through: u64,
    pub ids: Vec<(u64, EventId)>,
}

/// Server has compared/delivered every held event through this position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SparseAck {
    pub author: NodeId,
    pub through: u64,
}

/// "My prefix fingerprint (`Store::prefix_fp`) at `at` is `fp`; any
/// difference is above `lo`." A server that disagrees answers with a
/// [`SyncMsg::Probe`] (or, for a short range, the events themselves), so
/// twins that no single node holds both of are still found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrefixCheck {
    pub lo: u64,
    pub at: u64,
    pub fp: [u8; 32],
}

/// Client request: "send me what I lack in `room`" for authors in the id
/// range `(from, to]` (`None` = unbounded).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncSince {
    pub room: RoomId,
    pub from: Option<NodeId>,
    pub to: Option<NodeId>,
    /// Ascending by author, all inside `(from, to]`.
    pub want: Vec<AuthorWant>,
    /// [`sync_auth`]`(secret, requester, server, room)`.
    pub auth: [u8; 32],
}

impl SyncSince {
    /// A request for everything in `room` (empty client, one page).
    pub fn everything(room: RoomId, auth: [u8; 32]) -> Self {
        Self {
            room,
            from: None,
            to: None,
            want: Vec::new(),
            auth,
        }
    }

    fn in_range(&self, n: &NodeId) -> bool {
        self.from.is_none_or(|f| *n > f) && self.to.is_none_or(|t| *n <= t)
    }

    /// Size limits and ordering a server enforces before answering.
    fn well_formed(&self) -> bool {
        self.want.len() <= MAX_AUTHORS_PER_REQUEST
            && self.want.windows(2).all(|w| w[0].author < w[1].author)
            && self.want.iter().all(|w| {
                self.in_range(&w.author)
                    && w.holes.len() <= MAX_HOLES_PER_AUTHOR
                    && w.holes.iter().all(|h| (1..=w.have_hi).contains(h))
                    && w.check.is_none_or(|c| c.lo < c.at && c.at <= w.have_hi)
                    && w.sparse.as_ref().is_none_or(|c| {
                        c.after < c.through
                            && c.ids.len() <= SPARSE_IDS
                            && c.ids.windows(2).all(|v| v[0].0 < v[1].0)
                            && c.ids
                                .iter()
                                .all(|(seq, _)| *seq > c.after && *seq <= c.through)
                    })
            })
            && match (self.from, self.to) {
                (Some(f), Some(t)) => f < t,
                _ => true,
            }
    }
}

/// One page of the client's per-author summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestPage {
    pub want: Vec<AuthorWant>,
    /// Upper bound of the page; `None` when it reaches the last author.
    pub to: Option<NodeId>,
    /// Hole windows the page's most holey author needs (at least 1).
    pub windows: usize,
}

/// The page of `summary` after `from`, listing hole window `window` of each
/// author (summary from `Store::seq_summary(MAX_HOLES_PER_AUTHOR *
/// HOLE_WINDOWS)`).
pub fn request_page(
    summary: &BTreeMap<NodeId, AuthorSeqs>,
    from: Option<NodeId>,
    window: usize,
) -> RequestPage {
    let lo = from.map_or(Bound::Unbounded, Bound::Excluded);
    let mut rest = summary.range((lo, Bound::Unbounded));
    let mut windows = 1;
    let want: Vec<AuthorWant> = rest
        .by_ref()
        .take(MAX_AUTHORS_PER_REQUEST)
        .map(|(author, s)| {
            let mine = s.holes.len().div_ceil(MAX_HOLES_PER_AUTHOR).max(1);
            windows = windows.max(mine);
            let start = (window % mine) * MAX_HOLES_PER_AUTHOR;
            let holes = s
                .holes
                .iter()
                .copied()
                .skip(start)
                .take(MAX_HOLES_PER_AUTHOR)
                .collect();
            AuthorWant {
                author: *author,
                have_hi: s.max,
                holes,
                check: None,
                sparse: None,
            }
        })
        .collect();
    let to = if rest.next().is_some() {
        want.last().map(|w| w.author)
    } else {
        None
    };
    RequestPage {
        want,
        to,
        windows: windows.min(HOLE_WINDOWS),
    }
}

/// Server side of one request: wire events the client lacks within `caps`,
/// and whether more remain. Indexed per-author queries only.
pub fn answer_request(store: &Store, req: &SyncSince, caps: SyncCaps) -> Result<Answer, SyncError> {
    if !req.well_formed() {
        return Err(SyncError::Protocol("malformed request"));
    }
    // Leave frame room for metadata independently of the configured event cap.
    let caps = SyncCaps {
        max_events: caps
            .max_events
            .min(MAX_RESPONSE_EVENTS - 2 * req.want.len()),
        ..caps
    };
    let want: BTreeMap<NodeId, &AuthorWant> = req.want.iter().map(|w| (w.author, w)).collect();
    let (mut out, mut bytes) = (Vec::new(), 0usize);
    let mut probes = Vec::new();
    let mut sparse = Vec::new();
    let done = |events, probes, sparse, more| {
        Ok(Answer {
            events,
            probes,
            sparse,
            more,
        })
    };
    // Authors in `(from, to]`, read a page at a time (GATE-2 N4): work is
    // bounded by the listed authors plus the response caps, since every
    // author the client lacks yields at least one event.
    let scan_cap = MAX_AUTHORS_PER_REQUEST.saturating_add(caps.max_events);
    let (mut cursor, mut scanned) = (req.from, 0usize);
    'authors: loop {
        let page = store.seq_summary_page(cursor, SERVER_AUTHOR_PAGE, 0)?;
        let full = page.len() == SERVER_AUTHOR_PAGE;
        for (author, held) in &page {
            cursor = Some(*author);
            if req.to.is_some_and(|t| *author > t) {
                break 'authors;
            }
            scanned += 1;
            if scanned > scan_cap {
                return done(out, probes, sparse, true);
            }
            let w = want.get(author);
            if let Some(c) = w.and_then(|w| w.sparse.as_ref()) {
                let ids = store.seq_ids_after(author, c.after, SPARSE_IDS + 1)?;
                let mut through = c.after;
                let mut complete = true;
                for (i, &(seq, id)) in ids.iter().enumerate() {
                    if seq > c.through {
                        break;
                    }
                    if i == SPARSE_IDS {
                        complete = false;
                        break;
                    }
                    if !c.ids.contains(&(seq, id)) {
                        let events = store.events_for(author, seq - 1, &[], 1)?;
                        let Some(e) = events.first() else {
                            return Err(SyncError::Protocol("sparse event disappeared"));
                        };
                        let wire = e.encode()?;
                        if wire.len() > MAX_EVENT_BYTES {
                            return Err(SyncError::Protocol("stored event exceeds cap"));
                        }
                        if out.len() >= caps.max_events
                            || (!out.is_empty()
                                && bytes.saturating_add(wire.len()) > caps.max_bytes)
                        {
                            if through > c.after {
                                sparse.push(SparseAck {
                                    author: *author,
                                    through,
                                });
                            }
                            return done(out, probes, sparse, true);
                        }
                        bytes = bytes.saturating_add(wire.len());
                        out.push(wire);
                    }
                    through = seq;
                }
                sparse.push(SparseAck {
                    author: *author,
                    through: if complete { c.through } else { through },
                });
                continue;
            }
            let (have_hi, holes) = w.map_or((0, &[][..]), |w| (w.have_hi, w.holes.as_slice()));
            let mut ranged = Vec::new();
            if let Some(c) = w.and_then(|w| w.check) {
                match check_prefix(store, author, held.hi, c)? {
                    Some(Checked::Probe(p)) => probes.push(p),
                    Some(Checked::Range(lo, at)) => {
                        let n = usize::try_from(at - lo).unwrap_or(usize::MAX);
                        ranged = store.events_for(author, lo, &[], n)?;
                    }
                    None => {}
                }
            }
            if held.max <= have_hi && holes.is_empty() && ranged.is_empty() {
                continue;
            }
            let left = caps.max_events.saturating_sub(out.len());
            if left == 0 {
                return done(out, probes, sparse, true);
            }
            let regular = store.events_for(author, have_hi, holes, left)?;
            for e in ranged.into_iter().chain(regular) {
                let wire = e.encode()?;
                if wire.len() > MAX_EVENT_BYTES {
                    continue; // stored by an older build; never sent
                }
                if out.len() >= caps.max_events
                    || (!out.is_empty() && bytes.saturating_add(wire.len()) > caps.max_bytes)
                {
                    return done(out, probes, sparse, true);
                }
                bytes = bytes.saturating_add(wire.len());
                out.push(wire);
            }
        }
        if !full {
            break;
        }
    }
    // Twin serving (ADR-002 addendum): after all regular events, the kept
    // twin of every equivocated seq of each listed author, so a client that
    // holds a higher twin converges to the lowest id. Twins only fill spare
    // capacity and never set `more`, so they can't starve regular events.
    'twins: for w in &req.want {
        if w.sparse.is_some() {
            continue; // exact ID comparison already covers twins
        }
        for e in store.twins_for(&w.author, MAX_TWINS_PER_AUTHOR)? {
            let left = caps.max_events.saturating_sub(out.len());
            let wire = e.encode()?;
            if left == 0 || bytes.saturating_add(wire.len()) > caps.max_bytes {
                break 'twins;
            }
            bytes = bytes.saturating_add(wire.len());
            out.push(wire);
        }
    }
    // Authors absent at the server still acknowledge their empty ranges.
    for w in &req.want {
        if let Some(c) = &w.sparse
            && !sparse.iter().any(|a| a.author == w.author)
        {
            sparse.push(SparseAck {
                author: w.author,
                through: c.through,
            });
        }
    }
    done(out, probes, sparse, false)
}

/// What [`answer_request`] sends: events, prefix probes, and `more`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Answer {
    pub events: Vec<Vec<u8>>,
    pub probes: Vec<Probe>,
    pub sparse: Vec<SparseAck>,
    pub more: bool,
}

/// The server's prefix fingerprints of `author` at ascending `points`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Probe {
    pub author: NodeId,
    pub points: Vec<(u64, [u8; 32])>,
}

enum Checked {
    Probe(Probe),
    /// Prefixes differ inside `(lo, at]`, short enough to send outright.
    Range(u64, u64),
}

/// Server side of a [`PrefixCheck`] for `author`, whose contiguous prefix
/// here is `hi`. `None`: nothing to say (it matches, or we hold nothing
/// above `lo`).
fn check_prefix(
    store: &Store,
    author: &NodeId,
    hi: u64,
    c: PrefixCheck,
) -> Result<Option<Checked>, SyncError> {
    if hi <= c.lo {
        return Ok(None);
    }
    let fp = |seq| -> Result<[u8; 32], SyncError> {
        store
            .prefix_fp(author, seq)?
            .ok_or(SyncError::Protocol("prefix below hi missing"))
    };
    if hi < c.at {
        // Behind the client: let it compare at our prefix end.
        return Ok(Some(Checked::Probe(Probe {
            author: *author,
            points: vec![(hi, fp(hi)?)],
        })));
    }
    if fp(c.at)? == c.fp {
        return Ok(None);
    }
    let span = c.at - c.lo;
    if span <= PROBE_DIRECT {
        return Ok(Some(Checked::Range(c.lo, c.at)));
    }
    let mut points: Vec<(u64, [u8; 32])> = Vec::new();
    for j in 1..=PROBE_POINTS {
        let step = u64::try_from(u128::from(span) * u128::from(j) / u128::from(PROBE_POINTS))
            .unwrap_or(span);
        let seq = c.lo + step;
        if points.last().is_none_or(|(p, _)| *p < seq) {
            points.push((seq, fp(seq)?));
        }
    }
    Ok(Some(Checked::Probe(Probe {
        author: *author,
        points,
    })))
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
    /// Prefix fingerprints answering a mismatched [`PrefixCheck`].
    Probe(Probe),
    /// Acknowledged progress through an exact sparse inventory.
    Sparse(SparseAck),
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
    #[error("event encoding: {0}")]
    Proto(#[from] hm_proto::ProtoError),
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
        Ok(req) if req.well_formed() => budget
            .rooms
            .authorize(&req, budget.remote, budget.local)?
            .map(|(store, caps)| (store, caps, req)),
        Ok(_) => {
            tracing::debug!("malformed sync request");
            None
        }
        Err(e) => {
            tracing::debug!(error = %e, "bad sync request");
            None
        }
    };
    let Some((store, caps, req)) = authorized else {
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
    let answer = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
        answer_request(&*store.lock()?, &req, caps)
    })
    .await?;
    let Answer {
        mut events,
        mut probes,
        mut sparse,
        mut more,
    } = match answer {
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
        sparse.clear(); // never acknowledge events removed by the byte budget
        more = true;
    }
    let probe_bytes: usize = probes.iter().map(|p| 40 + 40 * p.points.len()).sum();
    if total.saturating_add(probe_bytes) > left {
        probes.clear(); // the client asks again next sync
    } else {
        total += probe_bytes;
    }
    let sparse_bytes = sparse.len() * 48;
    if total.saturating_add(sparse_bytes) > left {
        sparse.clear();
    } else {
        total += sparse_bytes;
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
    for p in probes {
        write_frame(send, &SyncMsg::Probe(p)).await?;
    }
    for ack in sparse {
        write_frame(send, &SyncMsg::Sparse(ack)).await?;
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
    /// Future-dated events parked by the store's skew guard.
    pub deferred: usize,
    /// Events with the same (author, seq) as another, different event.
    pub equivocations: usize,
}

/// Client-side reconciliation state for one sync run.
///
/// Each author is checked once per run at its prefix end. A mismatch
/// answered by a [`Probe`] narrows to a suspect range `(lo, at]`, re-checked
/// next round until the server sends that range's events (which exposes
/// the twin to the store) or the prefixes agree.
#[derive(Debug, Clone, Default)]
pub struct PrefixChecks {
    suspect: BTreeMap<NodeId, (u64, u64)>,
    settled: BTreeSet<NodeId>,
    sparse_done: BTreeSet<NodeId>,
}

impl PrefixChecks {
    /// Authors still being narrowed down.
    pub fn suspects(&self) -> usize {
        self.suspect.len()
    }

    /// The check to send for `author`, whose prefix here ends at `hi`.
    fn check(
        &self,
        store: &Store,
        author: &NodeId,
        hi: u64,
    ) -> Result<Option<PrefixCheck>, SyncError> {
        let (lo, at) = match self.suspect.get(author) {
            Some(&(lo, at)) if at <= hi => (lo, at),
            _ if self.settled.contains(author) || hi == 0 => return Ok(None),
            _ => (0, hi),
        };
        Ok(store
            .prefix_fp(author, at)?
            .map(|fp| PrefixCheck { lo, at, fp }))
    }

    /// Fold one round's probes in. `asked`: the checks that were sent.
    fn absorb(
        &mut self,
        store: &Store,
        asked: &[(NodeId, PrefixCheck)],
        probes: &[Probe],
    ) -> Result<(), SyncError> {
        for (author, c) in asked {
            let Some(p) = probes.iter().find(|p| p.author == *author) else {
                // Matched, or the range's events came back: done this run.
                self.suspect.remove(author);
                self.settled.insert(*author);
                continue;
            };
            let mut lo = c.lo;
            let mut narrowed = None;
            for &(seq, theirs) in &p.points {
                if seq <= lo || seq > c.at {
                    return Err(SyncError::Protocol("probe point out of range"));
                }
                match store.prefix_fp(author, seq)? {
                    Some(mine) if mine == theirs => lo = seq,
                    Some(_) => {
                        narrowed = Some((lo, seq));
                        break;
                    }
                    None => break,
                }
            }
            match narrowed {
                // Strictly narrower, so a run converges in few rounds.
                Some(r) if r != (c.lo, c.at) || r.1 - r.0 <= PROBE_DIRECT => {
                    self.suspect.insert(*author, r);
                }
                _ => {
                    self.suspect.remove(author);
                    self.settled.insert(*author);
                }
            }
        }
        Ok(())
    }
}

/// Outcome of one [`sync_page`] round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRound {
    /// Server cut the response short.
    pub more: bool,
    pub inserted: usize,
    /// Upper bound of the page asked for; `None` = it was the last page.
    pub to: Option<NodeId>,
    /// Hole windows this page needs; see [`request_page`].
    pub windows: usize,
    /// Nonzero while a prefix mismatch or acknowledged sparse page progresses.
    pub suspects: usize,
    /// Every sparse inventory in this author page was fully acknowledged.
    pub complete: bool,
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
) -> Result<(Vec<Event>, Vec<Probe>, Vec<SparseAck>, bool), SyncError> {
    let (mut send, mut recv) = conn.open_bi().await?;
    write_frame(&mut send, req).await.map_err(refused_or)?;
    send.finish()?;

    let mut events = Vec::new();
    let mut probes = Vec::new();
    let mut sparse = Vec::new();
    let (mut frames, mut bytes) = (0usize, 0usize);
    let more = loop {
        let msg =
            read_frame_within::<SyncMsg>(&mut recv, MAX_RESPONSE_FRAME, limits.frame_deadline)
                .await
                .map_err(refused_or)?;
        match msg {
            SyncMsg::Event(wire) => {
                if wire.len() > MAX_EVENT_BYTES {
                    return Err(SyncError::Protocol("event exceeds MAX_EVENT_BYTES"));
                }
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
            SyncMsg::Probe(p) => {
                frames += 1;
                let listed = req
                    .want
                    .iter()
                    .any(|w| w.author == p.author && w.check.is_some());
                if frames > MAX_RESPONSE_EVENTS
                    || !listed
                    || p.points.is_empty()
                    || p.points.len() > PROBE_POINTS as usize
                    || probes.iter().any(|q: &Probe| q.author == p.author)
                {
                    return Err(SyncError::Protocol("bad probe"));
                }
                probes.push(p);
            }
            SyncMsg::Sparse(ack) => {
                frames += 1;
                bytes = bytes.saturating_add(48);
                stats.bytes = stats.bytes.saturating_add(48);
                let check = req
                    .want
                    .iter()
                    .find(|w| w.author == ack.author)
                    .and_then(|w| w.sparse.as_ref());
                if frames > MAX_RESPONSE_EVENTS
                    || bytes > MAX_RESPONSE_BYTES
                    || stats.bytes > limits.max_bytes
                    || check.is_none_or(|c| ack.through <= c.after || ack.through > c.through)
                    || sparse.iter().any(|a: &SparseAck| a.author == ack.author)
                {
                    return Err(SyncError::Protocol("bad sparse acknowledgement"));
                }
                sparse.push(ack);
            }
            SyncMsg::End { more } => break more,
            SyncMsg::Refused(r) => return Err(SyncError::Refused(r)),
        }
    };
    Ok((events, probes, sparse, more))
}

/// One request/response round for the summary page after `from`, within
/// `limits.request_deadline`. `local` is this node's endpoint id.
#[allow(clippy::too_many_arguments)]
pub async fn sync_page(
    conn: &Connection,
    local: EndpointId,
    secret: &RoomSecret,
    store: &SharedStore,
    stats: &mut SyncStats,
    limits: &SyncLimits,
    from: Option<NodeId>,
    window: usize,
    checks: &mut PrefixChecks,
) -> Result<PageRound, SyncError> {
    let peer = NodeId::from_bytes(*conn.remote_id().as_bytes());
    let (req, to, windows, asked) = {
        let s = store.lock()?;
        let room = s.room();
        if room != secret.room_id() {
            return Err(SyncError::RoomMismatch);
        }
        // One page plus one author, so `request_page` knows if more follow.
        let summary = s.seq_summary_page(from, MAX_AUTHORS_PER_REQUEST + 1, 0)?;
        let mut page = request_page(&summary, from, window);
        let mut asked = Vec::new();
        for w in &mut page.want {
            if !checks.sparse_done.contains(&w.author) {
                let after = s.sparse_cursor(&peer, &w.author)?;
                let ids = s.seq_ids_after(&w.author, after, SPARSE_IDS)?;
                let through = if ids.len() == SPARSE_IDS {
                    ids.last().map_or(u64::MAX, |(seq, _)| *seq)
                } else {
                    u64::MAX
                };
                w.sparse = Some(SparseCheck {
                    after,
                    through,
                    ids,
                });
            } else {
                let hi = summary.get(&w.author).map_or(0, |a| a.hi);
                w.check = checks.check(&s, &w.author, hi)?;
                if let Some(c) = w.check {
                    asked.push((w.author, c));
                }
            }
        }
        let req = SyncSince {
            room,
            from,
            to: page.to,
            want: page.want,
            auth: sync_auth(secret, local, conn.remote_id(), room),
        };
        (req, page.to, page.windows, asked)
    };
    let rejected_before = stats.rejected;
    let (events, probes, sparse, more) = within(
        limits.request_deadline,
        "sync request",
        request_round(conn, &req, stats, limits),
    )
    .await?;
    stats.rounds += 1;
    let decoded_cleanly = stats.rejected == rejected_before;

    let sparse_progress = !sparse.is_empty()
        && (sparse.iter().any(|a| a.through != u64::MAX)
            || sparse.len() < req.want.iter().filter(|w| w.sparse.is_some()).count());
    let page_complete = !req.want.is_empty()
        && !more
        && req.want.iter().all(|w| {
            checks.sparse_done.contains(&w.author)
                || sparse
                    .iter()
                    .any(|a| a.author == w.author && a.through == u64::MAX)
        });

    let store = store.clone();
    let mut state = std::mem::take(checks);
    let (report, state) = tokio::task::spawn_blocking(move || -> Result<_, SyncError> {
        let mut s = store.lock()?;
        let report = s.append_batch(&events)?;
        // After ingest, so a twin that just arrived is already reflected.
        state.absorb(&s, &asked, &probes)?;
        // Never skip a rejected/deferred event on the strength of an ack.
        if decoded_cleanly && report.rejected == 0 && report.deferred == 0 {
            for ack in sparse {
                let done = ack.through == u64::MAX;
                s.set_sparse_cursor(&peer, &ack.author, if done { 0 } else { ack.through })?;
                if done {
                    state.sparse_done.insert(ack.author);
                    state.settled.insert(ack.author);
                }
            }
            if page_complete {
                s.set_sync_author_cursor(&peer, to)?;
            }
        }
        Ok((report, state))
    })
    .await??;
    *checks = state;
    stats.inserted += report.inserted;
    stats.duplicates += report.duplicates;
    stats.rejected += report.rejected;
    stats.deferred += report.deferred;
    stats.equivocations += report.equivocations;
    tracing::debug!(
        inserted = report.inserted,
        rejected = report.rejected,
        more,
        "sync round"
    );
    let suspects = usize::from(
        decoded_cleanly && sparse_progress && report.rejected == 0 && report.deferred == 0,
    ) + req
        .want
        .iter()
        .filter(|w| checks.suspect.contains_key(&w.author))
        .count();
    Ok(PageRound {
        more,
        inserted: report.inserted,
        to,
        windows,
        suspects,
        complete: decoded_cleanly && page_complete && report.rejected == 0 && report.deferred == 0,
    })
}

/// Pull everything `conn`'s peer has that `store` lacks, page by page.
///
/// A page is re-asked while events or acknowledged sparse ranges progress;
/// a server saying `more` without progress cannot loop us. Cursors survive
/// bounded runs; at most `limits.max_rounds` rounds execute per connection.
pub async fn sync_with(
    conn: &Connection,
    local: EndpointId,
    secret: &RoomSecret,
    store: &SharedStore,
    limits: &SyncLimits,
) -> Result<SyncStats, SyncError> {
    let mut stats = SyncStats::default();
    let peer = NodeId::from_bytes(*conn.remote_id().as_bytes());
    let mut from = store.lock()?.sync_author_cursor(&peer)?;
    let mut window = 0;
    let mut checks = PrefixChecks::default();
    while stats.rounds < limits.max_rounds {
        let r = sync_page(
            conn,
            local,
            secret,
            store,
            &mut stats,
            limits,
            from,
            window,
            &mut checks,
        )
        .await?;
        if r.complete {
            match r.to {
                Some(to) => {
                    from = Some(to);
                    window = 0;
                    continue;
                }
                None => break,
            }
        }
        if r.suspects > 0 {
            // Keep narrowing prefix mismatches on this page.
            continue;
        }
        if r.inserted > 0 {
            // Progress shifts the hole list: start its windows over.
            window = 0;
            continue;
        }
        if window + 1 < r.windows {
            window += 1;
            continue;
        }
        window = 0;
        match r.to {
            Some(to) => from = Some(to),
            None => {
                // Also wrap an empty resumed page (e.g. authors were removed).
                if !r.more {
                    store.lock()?.set_sync_author_cursor(&peer, None)?;
                }
                break;
            }
        }
    }
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
