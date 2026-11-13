//! herdmesh CLI internals: data dir layout, room sessions, room commands.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod board;
pub mod clipboard;
pub mod daemon;
pub mod data;
pub mod display;
pub mod naming;
pub mod notify;
pub mod project;
pub mod repair;
pub mod room;
pub mod session;
pub mod setup;

pub mod view;
