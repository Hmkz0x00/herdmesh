//! Signed, content-addressed room events.
//!
//! Canonical bytes = `DOMAIN || WIRE_VERSION || postcard(room, author, lamport, ts, body)`.
//! `id = blake3(canonical)`; `sig = ed25519(author.node, canonical)`.
//! Wire bytes = `WIRE_VERSION || postcard(Event)`.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
use crate::ids::{AgentRef, EventId, Lamport, NodeId, RoomId, TaskId, to_hex};
use crate::refs::Ref;

/// Wire format version; first byte of every encoded event.
pub const WIRE_VERSION: u8 = 1;

/// Domain separation so event signatures can't be replayed as anything else.
const DOMAIN: &[u8] = b"herdmesh/event";

/// Member role in a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Owner,
    Member,
    Observer,
}

/// Agent status mirrored from herdr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AgentStatus {
    Idle,
    Working,
    Blocked,
    Done,
}

/// Task lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskStatus {
    Open,
    InProgress,
    Blocked,
    Done,
    Dropped,
}

/// Typed event payload. Variant order is part of the wire format: append only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventBody {
    Join {
        role: Role,
    },
    Leave,
    Presence {
        status: AgentStatus,
    },
    TaskAdd {
        task: TaskId,
        title: String,
        paths: Vec<String>,
    },
    TaskUpdate {
        task: TaskId,
        status: TaskStatus,
        note: Option<String>,
    },
    LeaseClaim {
        paths: Vec<String>,
        ttl_secs: u32,
    },
    LeaseRelease {
        paths: Vec<String>,
    },
    Decision {
        title: String,
        /// ≤60 tokens; enforced by `budget`, not here.
        rationale: String,
        refs: Vec<Ref>,
    },
    Handoff {
        to: Option<AgentRef>,
        summary: String,
        refs: Vec<Ref>,
        next_steps: Vec<String>,
    },
    Ask {
        to: Option<AgentRef>,
        q: String,
    },
    Answer {
        ask_id: EventId,
        a: String,
    },
    Note {
        scope: String,
        text: String,
    },
    Digest {
        since: Lamport,
        text: String,
    },
}

/// Fieldless event kind, derived from the body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EventKind {
    Join,
    Leave,
    Presence,
    TaskAdd,
    TaskUpdate,
    LeaseClaim,
    LeaseRelease,
    Decision,
    Handoff,
    Ask,
    Answer,
    Note,
    Digest,
}

impl EventKind {
    pub const ALL: [EventKind; 13] = [
        Self::Join,
        Self::Leave,
        Self::Presence,
        Self::TaskAdd,
        Self::TaskUpdate,
        Self::LeaseClaim,
        Self::LeaseRelease,
        Self::Decision,
        Self::Handoff,
        Self::Ask,
        Self::Answer,
        Self::Note,
        Self::Digest,
    ];
}

impl EventBody {
    pub fn kind(&self) -> EventKind {
        match self {
            Self::Join { .. } => EventKind::Join,
            Self::Leave => EventKind::Leave,
            Self::Presence { .. } => EventKind::Presence,
            Self::TaskAdd { .. } => EventKind::TaskAdd,
            Self::TaskUpdate { .. } => EventKind::TaskUpdate,
            Self::LeaseClaim { .. } => EventKind::LeaseClaim,
            Self::LeaseRelease { .. } => EventKind::LeaseRelease,
            Self::Decision { .. } => EventKind::Decision,
            Self::Handoff { .. } => EventKind::Handoff,
            Self::Ask { .. } => EventKind::Ask,
            Self::Answer { .. } => EventKind::Answer,
            Self::Note { .. } => EventKind::Note,
            Self::Digest { .. } => EventKind::Digest,
        }
    }
}

/// ed25519 signature bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EventSig(#[serde(with = "serde_bytes")] [u8; 64]);

impl EventSig {
    pub const fn from_bytes(bytes: [u8; 64]) -> Self {
        Self(bytes)
    }
    pub const fn as_bytes(&self) -> &[u8; 64] {
        &self.0
    }
}

impl fmt::Debug for EventSig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "EventSig({})", &to_hex(&self.0)[..10])
    }
}

/// A signed room event. Construct with [`Event::sign`]; check with [`Event::verify`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub room: RoomId,
    pub author: AgentRef,
    pub lamport: Lamport,
    /// Unix millis; informational only, never used for ordering.
    pub ts: u64,
    pub body: EventBody,
    pub sig: EventSig,
}

/// Borrowed view of the signed fields, in canonical order.
#[derive(Serialize)]
struct Unsigned<'a> {
    room: &'a RoomId,
    author: &'a AgentRef,
    lamport: &'a Lamport,
    ts: &'a u64,
    body: &'a EventBody,
}

impl Event {
    /// Build and sign an event. The author node is the signing key's public key.
    pub fn sign(
        key: &SigningKey,
        room: RoomId,
        agent_name: impl Into<String>,
        lamport: Lamport,
        ts: u64,
        body: EventBody,
    ) -> Result<Self, ProtoError> {
        let mut ev = Self {
            id: EventId::from_bytes([0; 32]),
            room,
            author: AgentRef::new(NodeId::from_verifying_key(&key.verifying_key()), agent_name),
            lamport,
            ts,
            body,
            sig: EventSig([0; 64]),
        };
        let canonical = ev.canonical_bytes()?;
        ev.id = EventId::from_bytes(*blake3::hash(&canonical).as_bytes());
        ev.sig = EventSig(key.sign(&canonical).to_bytes());
        Ok(ev)
    }

    pub fn kind(&self) -> EventKind {
        self.body.kind()
    }

    /// Bytes that are hashed and signed.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = Vec::with_capacity(128);
        out.extend_from_slice(DOMAIN);
        out.push(WIRE_VERSION);
        let unsigned = Unsigned {
            room: &self.room,
            author: &self.author,
            lamport: &self.lamport,
            ts: &self.ts,
            body: &self.body,
        };
        out.extend_from_slice(&postcard::to_allocvec(&unsigned)?);
        Ok(out)
    }

    /// blake3 of the canonical bytes.
    pub fn compute_id(&self) -> Result<EventId, ProtoError> {
        Ok(EventId::from_bytes(
            *blake3::hash(&self.canonical_bytes()?).as_bytes(),
        ))
    }

    /// Check that `id` matches the content and `sig` is the author node's signature.
    pub fn verify(&self) -> Result<(), ProtoError> {
        let canonical = self.canonical_bytes()?;
        if blake3::hash(&canonical).as_bytes() != self.id.as_bytes() {
            return Err(ProtoError::IdMismatch);
        }
        let key = VerifyingKey::from_bytes(self.author.node.as_bytes())
            .map_err(|_| ProtoError::BadAuthorKey)?;
        let sig = ed25519_dalek::Signature::from_bytes(&self.sig.0);
        key.verify_strict(&canonical, &sig)
            .map_err(|_| ProtoError::BadSignature)
    }

    /// Wire form: version byte then postcard.
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        let mut out = vec![WIRE_VERSION];
        out.extend_from_slice(&postcard::to_allocvec(self)?);
        Ok(out)
    }

    /// Parse the wire form. Does not verify; call [`Event::verify`].
    pub fn decode(bytes: &[u8]) -> Result<Self, ProtoError> {
        let (&version, rest) = bytes.split_first().ok_or(ProtoError::Empty)?;
        if version != WIRE_VERSION {
            return Err(ProtoError::UnsupportedVersion(version));
        }
        let (ev, tail) = postcard::take_from_bytes::<Self>(rest)?;
        if !tail.is_empty() {
            return Err(ProtoError::TrailingBytes);
        }
        Ok(ev)
    }
}
