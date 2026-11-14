//! herdmesh CLI internals: data dir layout, room sessions, room commands.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod board;
pub mod clients;
pub mod clipboard;
pub mod daemon;
pub mod data;
pub mod display;
pub mod herdr_notify;
pub mod naming;
pub mod notify;
pub mod project;
pub mod repair;
pub mod room;
pub mod safefs;
pub mod session;
pub mod setup;

pub mod view;
