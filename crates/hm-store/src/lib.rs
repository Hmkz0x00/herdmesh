//! Append-only signed event log and SQLite materialized views.
//!
//! [`Store`] verifies and stores events; [`Views`] are rebuilt deterministically
//! from the event set ordered by (lamport, author node, id).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
pub mod settle;
mod store;
mod views;

pub use error::StoreError;
pub use store::{AppendOutcome, Store};
pub use views::{
    ClaimOutcome, ClaimPolicy, DecisionRec, Lease, Member, OrderKey, Task, Views, next_lamport,
    order_key, order_only,
};

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, "hm-store");
    }
}
