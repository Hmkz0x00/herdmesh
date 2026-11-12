//! Room gossip: publish and receive signed [`Event`]s on the room topic.
//!
//! Wire format is exactly [`Event::encode`]. Every received message is
//! screened by [`screen_event`] (size cap, decode, room, id + signature);
//! failures are counted in [`DropStats`] and logged at debug **without the
//! body**. Survivors are deduplicated by [`EventId`] with a bounded LRU
//! ([`SeenSet`]) shared by sender and receiver, so peers' re-broadcasts and
//! echoes of our own events are never yielded twice.
//!
//! Gossip is best-effort and has no history: events published before a peer
//! joined are not replayed here (that is the sync ALPN's job).

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

use futures_util::StreamExt as _;
use hm_proto::{Event, EventId, NodeId, RoomId};
use iroh::EndpointAddr;
use iroh::address_lookup::MemoryLookup;
use iroh_gossip::api::{ApiError, Event as GossipEvent, GossipReceiver, GossipSender};

use crate::id::node_id_from_endpoint;
use crate::net::Net;
use crate::ticket::RoomSecret;

/// Largest encoded event we accept or send (shared with sync and the store).
pub use hm_proto::limits::MAX_EVENT_BYTES;
/// iroh-gossip frame cap: our event cap plus headroom for gossip framing.
/// (iroh-gossip's own default is 4096, too small for our events.)
pub(crate) const GOSSIP_MAX_MESSAGE_BYTES: usize = MAX_EVENT_BYTES + 4 * 1024;
/// How many recent event ids are remembered for dedup.
pub const SEEN_CAPACITY: usize = 4096;

/// Why a received message was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    Oversize,
    Malformed,
    WrongRoom,
    BadSignature,
}

/// Counters of dropped messages (duplicates are counted but are not errors).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DropStats {
    pub oversize: u64,
    pub malformed: u64,
    pub wrong_room: u64,
    pub bad_signature: u64,
    pub duplicate: u64,
}

impl DropStats {
    fn count(&mut self, r: DropReason) {
        match r {
            DropReason::Oversize => self.oversize += 1,
            DropReason::Malformed => self.malformed += 1,
            DropReason::WrongRoom => self.wrong_room += 1,
            DropReason::BadSignature => self.bad_signature += 1,
        }
    }
}

/// Validate one received message for `room`. Pure; no logging.
pub fn screen_event(bytes: &[u8], room: &RoomId) -> Result<Event, DropReason> {
    if bytes.len() > MAX_EVENT_BYTES {
        return Err(DropReason::Oversize);
    }
    let ev = Event::decode(bytes).map_err(|_| DropReason::Malformed)?;
    if ev.room != *room {
        return Err(DropReason::WrongRoom);
    }
    ev.verify().map_err(|_| DropReason::BadSignature)?;
    Ok(ev)
}

/// Bounded LRU set of event ids.
#[derive(Debug)]
pub struct SeenSet {
    cap: usize,
    tick: u64,
    by_id: HashMap<EventId, u64>,
    by_tick: BTreeMap<u64, EventId>,
}

impl SeenSet {
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            tick: 0,
            by_id: HashMap::new(),
            by_tick: BTreeMap::new(),
        }
    }

    /// Mark `id` as most recently seen. Returns `true` if it was new.
    pub fn insert(&mut self, id: EventId) -> bool {
        self.tick += 1;
        if let Some(old) = self.by_id.insert(id, self.tick) {
            self.by_tick.remove(&old);
            self.by_tick.insert(self.tick, id);
            return false;
        }
        self.by_tick.insert(self.tick, id);
        if self.by_id.len() > self.cap {
            if let Some((_, oldest)) = self.by_tick.pop_first() {
                self.by_id.remove(&oldest);
            }
        }
        true
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

type Seen = Arc<Mutex<SeenSet>>;

fn lock(seen: &Seen) -> MutexGuard<'_, SeenSet> {
    // A panic while holding this lock cannot leave the set inconsistent in a
    // way that matters for dedup, so recover from poisoning.
    seen.lock().unwrap_or_else(|p| p.into_inner())
}

/// What a [`RoomReceiver`] yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoomEvent {
    /// A new, verified event for this room (boxed: events are large).
    Event(Box<Event>),
    /// A direct gossip neighbor connected (input for Presence later).
    NeighborUp(NodeId),
    /// A direct gossip neighbor went away.
    NeighborDown(NodeId),
    /// We fell behind and gossip dropped messages; sync should catch up.
    Lagged,
}

/// Errors from room gossip.
#[derive(Debug, thiserror::Error)]
pub enum GossipError {
    #[error("gossip: {0}")]
    Api(#[from] ApiError),
    #[error("endpoint closed")]
    EndpointClosed,
    #[error("room subscription closed")]
    Closed,
    #[error("encoding event: {0}")]
    Encode(#[from] hm_proto::ProtoError),
    #[error("event is for a different room")]
    WrongRoom,
    #[error("encoded event is {0} bytes, cap is {MAX_EVENT_BYTES}")]
    Oversize(usize),
}

/// Publishing half of a room subscription. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RoomSender {
    inner: GossipSender,
    room: RoomId,
    seen: Seen,
}

impl RoomSender {
    /// Broadcast `ev` to the room. The id is marked seen so echoes are
    /// not yielded back by our receiver.
    pub async fn publish(&self, ev: &Event) -> Result<(), GossipError> {
        if ev.room != self.room {
            return Err(GossipError::WrongRoom);
        }
        let bytes = ev.encode()?;
        if bytes.len() > MAX_EVENT_BYTES {
            return Err(GossipError::Oversize(bytes.len()));
        }
        lock(&self.seen).insert(ev.id);
        self.inner.broadcast(bytes.into()).await?;
        Ok(())
    }
}

/// Receiving half: yields verified, deduplicated [`RoomEvent`]s.
#[derive(Debug)]
pub struct RoomReceiver {
    inner: GossipReceiver,
    room: RoomId,
    seen: Seen,
    drops: DropStats,
    /// Events read while waiting in [`Self::joined`], handed out by `next`.
    pending: VecDeque<RoomEvent>,
}

impl RoomReceiver {
    /// Wait until at least one gossip neighbor is connected.
    ///
    /// Unlike `GossipReceiver::joined`, nothing read while waiting is lost:
    /// events are screened as usual and buffered for [`Self::next`].
    pub async fn joined(&mut self) -> Result<(), GossipError> {
        while self.neighbor_count() == 0 {
            match self.read().await {
                Some(Ok(ev)) => self.pending.push_back(ev),
                Some(Err(e)) => return Err(e),
                None => return Err(GossipError::Closed),
            }
        }
        Ok(())
    }

    /// Number of current direct gossip neighbors.
    pub fn neighbor_count(&self) -> usize {
        self.inner.neighbors().count()
    }

    /// Next room event; `None` when the subscription ends.
    pub async fn next(&mut self) -> Option<Result<RoomEvent, GossipError>> {
        if let Some(ev) = self.pending.pop_front() {
            return Some(Ok(ev));
        }
        self.read().await
    }

    async fn read(&mut self) -> Option<Result<RoomEvent, GossipError>> {
        loop {
            let ev = match self.inner.next().await? {
                Ok(ev) => ev,
                Err(e) => return Some(Err(e.into())),
            };
            match ev {
                GossipEvent::Received(msg) => {
                    let from = msg.delivered_from;
                    match screen_event(&msg.content, &self.room) {
                        Ok(ev) => {
                            if lock(&self.seen).insert(ev.id) {
                                return Some(Ok(RoomEvent::Event(Box::new(ev))));
                            }
                            self.drops.duplicate += 1;
                        }
                        Err(reason) => {
                            self.drops.count(reason);
                            tracing::debug!(
                                %from, ?reason, len = msg.content.len(),
                                "dropped gossip message"
                            );
                        }
                    }
                }
                GossipEvent::NeighborUp(id) => {
                    return Some(Ok(RoomEvent::NeighborUp(node_id_from_endpoint(id))));
                }
                GossipEvent::NeighborDown(id) => {
                    return Some(Ok(RoomEvent::NeighborDown(node_id_from_endpoint(id))));
                }
                GossipEvent::Lagged => return Some(Ok(RoomEvent::Lagged)),
                // Non-exhaustive upstream enum: ignore future variants.
                _ => {}
            }
        }
    }

    /// Snapshot of drop counters so far.
    pub fn drops(&self) -> DropStats {
        self.drops
    }
}

/// Join the room's gossip topic.
///
/// `bootstrap` peers are registered with the endpoint's address lookup (so
/// they are dialable even with relay/discovery disabled) and used as the
/// initial gossip neighbors. Returns immediately; use
/// [`RoomReceiver::joined`] to wait for a neighbor.
pub async fn join_room(
    net: &Net,
    secret: &RoomSecret,
    bootstrap: Vec<EndpointAddr>,
) -> Result<(RoomSender, RoomReceiver), GossipError> {
    let me = net.endpoint().id();
    let peers: Vec<EndpointAddr> = bootstrap.into_iter().filter(|a| a.id != me).collect();
    let ids = peers.iter().map(|a| a.id).collect();
    if !peers.is_empty() {
        let lookup = MemoryLookup::new();
        for a in peers {
            lookup.add_endpoint_info(a);
        }
        net.endpoint()
            .address_lookup()
            .map_err(|_| GossipError::EndpointClosed)?
            .add(lookup);
    }
    let (tx, rx) = net
        .gossip()
        .subscribe(secret.topic_id(), ids)
        .await?
        .split();
    let room = secret.room_id();
    let seen = Arc::new(Mutex::new(SeenSet::new(SEEN_CAPACITY)));
    Ok((
        RoomSender {
            inner: tx,
            room,
            seen: Arc::clone(&seen),
        },
        RoomReceiver {
            inner: rx,
            room,
            seen,
            drops: DropStats::default(),
            pending: VecDeque::new(),
        },
    ))
}
