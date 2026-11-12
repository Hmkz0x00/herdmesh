//! Per-machine daemon: owns the node key and the room stores, serves local
//! agent adapters over a same-user local socket, publishes via [`NetHandle`].
//!
//! - [`ipc`]: socket, framing, token handshake, client.
//! - [`Daemon`]: the implementation of every call (events, cards, briefs).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod core;
pub mod ipc;

pub use core::{DAEMON_AUTHOR, Daemon, NetHandle, NullNet, Ping, redact_body, wrapped_cost};

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
