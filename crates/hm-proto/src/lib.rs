//! Versioned wire types, event schema, token budgets. No I/O.
//!
//! Event model and signing: see [`Event`]. Wire format: [`WIRE_VERSION`].
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod budget;
mod error;
mod event;
mod ids;
pub mod limits;
pub mod redact;
mod refs;

pub use error::{ProtoError, RefParseError};
pub use event::{
    AgentStatus, Event, EventBody, EventKind, EventSig, Role, TaskStatus, WIRE_VERSION,
};
pub use ids::{AgentRef, EventId, Lamport, NodeId, RoomId, TaskId};
pub use refs::{GitRef, LineRange, Ref};

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, "hm-proto");
    }
}
