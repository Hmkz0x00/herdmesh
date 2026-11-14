//! Read-only live room view JSON types (ADR-003).
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedMember {
    pub id: String,
    pub node: String,
    pub name: String,
    pub role: String,
    pub local: bool,
    pub present: bool,
    pub status: String,
    pub activity: String,
    pub last_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedLease {
    pub id: String,
    pub paths: Vec<String>,
    pub holder: String,
    pub state: String,
    pub expires_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedTask {
    pub id: String,
    pub title: String,
    pub status: String,
    pub owner: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedEvent {
    /// Full hex id (use it with the event-detail endpoint).
    pub id: String,
    pub lamport: u64,
    pub ts_ms: u64,
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
    pub text: String,
    /// `false` when `text` was cut at 200 chars.
    pub full: bool,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// `"observer"` for a message whose author was removed (or is a mere
    /// observer); absent for ordinary events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Feed {
    pub room: String,
    pub you: String,
    pub now_ms: u64,
    /// Opaque local insertion position; unrelated to event Lamport clocks.
    pub cursor: u64,
    /// Some collections, history, or response bytes were omitted by safety caps.
    #[serde(default)]
    pub truncated: bool,
    pub members: Vec<FeedMember>,
    pub leases: Vec<FeedLease>,
    pub tasks: Vec<FeedTask>,
    pub events: Vec<FeedEvent>,
}

/// One message with its complete (redacted, 8 KiB-capped) text, for
/// `GET /<token>/api/event`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FullEvent {
    pub id: String,
    pub kind: String,
    pub from: String,
    pub to: Option<String>,
    pub ts_ms: u64,
    pub text: String,
    pub refs: Vec<String>,
    #[serde(default)]
    pub truncated: bool,
    /// See [`FeedEvent::role`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}
