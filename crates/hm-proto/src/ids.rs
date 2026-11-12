//! Identifiers: rooms, nodes, agents, events, tasks, Lamport clocks.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::error::RefParseError;

/// Lowercase hex of `bytes`.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from(HEX[usize::from(b >> 4)]));
        s.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    s
}

/// Parse exactly 64 lowercase hex chars into 32 bytes.
pub(crate) fn from_hex32(s: &str) -> Option<[u8; 32]> {
    fn nibble(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, pair) in b.chunks_exact(2).enumerate() {
        out[i] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

macro_rules! id32 {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        pub struct $name([u8; 32]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; 32]) -> Self {
                Self(bytes)
            }
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&to_hex(&self.0))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                // Short form keeps logs readable.
                write!(f, "{}({})", stringify!($name), &to_hex(&self.0)[..10])
            }
        }
    };
}

id32!(
    /// Room identifier (derived from the room secret by `hm-net`).
    RoomId
);
id32!(
    /// Machine identity: the ed25519 public key that signs events (ADR-001 note 5).
    NodeId
);
id32!(
    /// Event identifier: blake3 of the canonical unsigned bytes.
    EventId
);

impl NodeId {
    pub fn from_verifying_key(key: &ed25519_dalek::VerifyingKey) -> Self {
        Self(key.to_bytes())
    }
}

impl EventId {
    /// Parse the 64-char lowercase hex form.
    pub fn from_hex(s: &str) -> Option<Self> {
        from_hex32(s).map(Self)
    }
}

/// Logical clock. Ordering uses Lamport only; `ts` is informational (ADR-001 note 6).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
pub struct Lamport(pub u64);

/// An author: machine key plus agent label under that key.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct AgentRef {
    pub node: NodeId,
    pub agent_name: String,
}

impl AgentRef {
    pub fn new(node: NodeId, agent_name: impl Into<String>) -> Self {
        Self {
            node,
            agent_name: agent_name.into(),
        }
    }
}

/// Human-friendly task id, e.g. `t-12`. Chars: `[A-Za-z0-9_.-]`, 1..=64.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TaskId(String);

impl TaskId {
    pub const MAX_LEN: usize = 64;

    pub fn new(s: impl Into<String>) -> Result<Self, RefParseError> {
        let s = s.into();
        let valid = !s.is_empty()
            && s.len() <= Self::MAX_LEN
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
        if valid {
            Ok(Self(s))
        } else {
            Err(RefParseError::BadTaskId)
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TaskId {
    type Error = RefParseError;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Self::new(s)
    }
}

impl From<TaskId> for String {
    fn from(t: TaskId) -> Self {
        t.0
    }
}

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip() {
        let b = [0x01, 0xab, 0xff, 0x00];
        assert_eq!(to_hex(&b), "01abff00");
        let full = [0x5au8; 32];
        assert_eq!(from_hex32(&to_hex(&full)), Some(full));
        assert_eq!(from_hex32("AB"), None);
        assert_eq!(from_hex32(&"AB".repeat(32)), None);
    }

    #[test]
    fn task_id_validation() {
        assert!(TaskId::new("t-1.x_y").is_ok());
        assert!(TaskId::new("").is_err());
        assert!(TaskId::new("a b").is_err());
        assert!(TaskId::new("x".repeat(65)).is_err());
    }
}
