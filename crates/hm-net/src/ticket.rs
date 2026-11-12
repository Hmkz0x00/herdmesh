//! Room secrets and shareable room tickets.
//!
//! Text form: `hmroom1` + lowercase base32 (no padding) of
//! `postcard(TicketWire) || blake3(postcard)[..4]`. The 4-byte checksum makes
//! typos and single-character tampering fail loudly. It is **not**
//! authentication: anyone holding a ticket holds the room secret.

use std::fmt;
use std::str::FromStr;

use data_encoding::BASE32_NOPAD;
use hm_proto::RoomId;
use iroh::EndpointAddr;
use iroh_gossip::TopicId;
use serde::{Deserialize, Serialize};

/// Text prefix of every ticket; the trailing `1` is the text-format version.
pub const TICKET_PREFIX: &str = "hmroom1";
/// Payload version inside the encoded ticket.
pub const TICKET_VERSION: u8 = 1;

const CHECKSUM_LEN: usize = 4;
const TOPIC_DOMAIN: &[u8] = b"herdmesh/topic";

/// Errors decoding a [`RoomTicket`].
#[derive(Debug, thiserror::Error)]
pub enum TicketError {
    #[error("ticket must start with `{TICKET_PREFIX}`")]
    Prefix,
    #[error("ticket is not valid base32")]
    Base32,
    #[error("ticket is truncated or its checksum does not match")]
    Checksum,
    #[error("ticket payload is malformed")]
    Payload,
    #[error("unsupported ticket version {found} (expected {TICKET_VERSION})")]
    Version { found: u8 },
    #[error("ticket room id does not match its secret")]
    RoomIdMismatch,
}

/// Errors creating a [`RoomSecret`].
#[derive(Debug, thiserror::Error)]
#[error("OS random number generator failed: {0}")]
pub struct RandomError(getrandom::Error);

/// 32 random bytes shared by every room member. Anyone holding it can join.
///
/// `Debug` is redacted; there is deliberately no `Display`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomSecret([u8; 32]);

impl RoomSecret {
    /// A fresh secret from the OS RNG.
    pub fn generate() -> Result<Self, RandomError> {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(RandomError)?;
        Ok(Self(b))
    }

    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// `blake3(secret)`, the full 32-byte hash as a [`RoomId`].
    pub fn room_id(&self) -> RoomId {
        RoomId::from_bytes(*blake3::hash(&self.0).as_bytes())
    }

    /// Gossip topic: `blake3("herdmesh/topic" || secret)`.
    pub fn topic_id(&self) -> TopicId {
        let mut h = blake3::Hasher::new();
        h.update(TOPIC_DOMAIN);
        h.update(&self.0);
        TopicId::from_bytes(*h.finalize().as_bytes())
    }
}

impl fmt::Debug for RoomSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RoomSecret(..)")
    }
}

/// Everything needed to join a room: the secret plus peers to dial first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomTicket {
    pub secret: RoomSecret,
    /// Must equal `secret.room_id()`; checked on decode.
    pub room_id: RoomId,
    pub bootstrap: Vec<EndpointAddr>,
}

#[derive(Serialize, Deserialize)]
struct TicketWire {
    v: u8,
    secret: RoomSecret,
    room_id: RoomId,
    bootstrap: Vec<EndpointAddr>,
}

impl RoomTicket {
    pub fn new(secret: RoomSecret, bootstrap: Vec<EndpointAddr>) -> Self {
        let room_id = secret.room_id();
        Self {
            secret,
            room_id,
            bootstrap,
        }
    }

    /// Encode with an explicit payload version. Exposed for version-check tests.
    #[doc(hidden)]
    pub fn encode_with_version(&self, v: u8) -> String {
        let wire = TicketWire {
            v,
            secret: self.secret.clone(),
            room_id: self.room_id,
            bootstrap: self.bootstrap.clone(),
        };
        // Serializing owned plain data into a Vec cannot fail in practice;
        // an empty payload would still be rejected on decode.
        let mut bytes = postcard::to_allocvec(&wire).unwrap_or_default();
        let sum = blake3::hash(&bytes);
        bytes.extend_from_slice(&sum.as_bytes()[..CHECKSUM_LEN]);
        let mut out = String::from(TICKET_PREFIX);
        out.push_str(&BASE32_NOPAD.encode(&bytes).to_ascii_lowercase());
        out
    }
}

impl fmt::Display for RoomTicket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.encode_with_version(TICKET_VERSION))
    }
}

impl FromStr for RoomTicket {
    type Err = TicketError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let body = s.strip_prefix(TICKET_PREFIX).ok_or(TicketError::Prefix)?;
        let bytes = BASE32_NOPAD
            .decode(body.to_ascii_uppercase().as_bytes())
            .map_err(|_| TicketError::Base32)?;
        if bytes.len() <= CHECKSUM_LEN {
            return Err(TicketError::Checksum);
        }
        let (payload, sum) = bytes.split_at(bytes.len() - CHECKSUM_LEN);
        if &blake3::hash(payload).as_bytes()[..CHECKSUM_LEN] != sum {
            return Err(TicketError::Checksum);
        }
        // Version is the first postcard field (a u8 is one raw byte).
        match payload.first() {
            Some(&TICKET_VERSION) => {}
            Some(&found) => return Err(TicketError::Version { found }),
            None => return Err(TicketError::Payload),
        }
        let wire: TicketWire = postcard::from_bytes(payload).map_err(|_| TicketError::Payload)?;
        if wire.room_id != wire.secret.room_id() {
            return Err(TicketError::RoomIdMismatch);
        }
        Ok(Self {
            secret: wire.secret,
            room_id: wire.room_id,
            bootstrap: wire.bootstrap,
        })
    }
}
