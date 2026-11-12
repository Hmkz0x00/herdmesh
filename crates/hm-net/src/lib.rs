//! iroh endpoint, room gossip, request/response, tickets.
//!
//! [`RoomTicket`] / [`RoomSecret`] define a room; [`Net`] is the bound iroh
//! endpoint with gossip and the reserved [`SYNC_ALPN`].
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod id;
mod net;
pub mod sync;
mod ticket;

pub use id::{endpoint_id_from_node, node_id_from_endpoint};
pub use net::{Net, NetConfig, NetError, RelayChoice, SYNC_ALPN, load_or_create_key};
pub use sync::{
    ERR_REFUSED, MAX_REQUEST_FRAME, MAX_RESPONSE_FRAME, Refusal, SharedStore, SyncError,
    SyncHandler, SyncMsg, SyncRooms, SyncSince, SyncStats, read_frame, sync_auth, sync_from,
    sync_once, sync_with, write_frame,
};
pub use ticket::{RandomError, RoomSecret, RoomTicket, TICKET_PREFIX, TICKET_VERSION, TicketError};

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, "hm-net");
    }
}
