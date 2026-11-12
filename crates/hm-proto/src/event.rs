//! Signed, content-addressed room events.
//!
//! Canonical bytes = `DOMAIN || WIRE_VERSION || postcard(room, author, seq, lamport, ts, body)`.
//! `id = blake3(canonical)`; `sig = ed25519(author.node, canonical)`.
//! Wire bytes = `WIRE_VERSION || postcard(Event)`.

use std::fmt;

use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::error::ProtoError;
use crate::ids::{AgentRef, EventId, Lamport, NodeId, RoomId, TaskId, to_hex};
use crate::refs::Ref;

/// Wire format version; first byte of every encoded event.
pub const WIRE_VERSION: u8 = 2;

/// Domain separation so event signatures can't be replayed as anything else.
const DOMAIN: &[u8] = b"herdmesh/event";

/// Member role in a room.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    Owner,
    Member,
    Observer,
}

impl Role {
    /// Whether a node with this role may author events of `kind`. Views
    /// ignore (but still store) events whose author lacks permission.
    pub fn may(self, kind: EventKind) -> bool {
        use EventKind as K;
        match self {
            Self::Owner => true,
            Self::Member => !matches!(kind, K::RoleGrant | K::Genesis),
            Self::Observer => matches!(
                kind,
                K::Join | K::Leave | K::Presence | K::Ask | K::Answer | K::Note
            ),
        }
    }
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
    /// Ends the lease taken by the `LeaseClaim` event `claim`. Only honored
    /// from the claim's author node.
    LeaseRelease {
        claim: EventId,
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
    /// First event of a room. Its id is carried in the ticket; only that
    /// Genesis anchors roles.
    Genesis {
        owner: NodeId,
    },
    /// Owner-only: set `node`'s role.
    RoleGrant {
        node: NodeId,
        role: Role,
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
    Genesis,
    RoleGrant,
}

impl EventKind {
    pub const ALL: [EventKind; 15] = [
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
        Self::Genesis,
        Self::RoleGrant,
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
            Self::Genesis { .. } => EventKind::Genesis,
            Self::RoleGrant { .. } => EventKind::RoleGrant,
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
    /// Per author node sequence number: 1, 2, 3, ... (single writer per node).
    pub seq: u64,
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
    seq: &'a u64,
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
        seq: u64,
        lamport: Lamport,
        ts: u64,
        body: EventBody,
    ) -> Result<Self, ProtoError> {
        if seq == 0 {
            return Err(ProtoError::ZeroSeq);
        }
        let mut ev = Self {
            id: EventId::from_bytes([0; 32]),
            room,
            author: AgentRef::new(NodeId::from_verifying_key(&key.verifying_key()), agent_name),
            seq,
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
            seq: &self.seq,
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
        if self.seq == 0 {
            return Err(ProtoError::ZeroSeq);
        }
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
