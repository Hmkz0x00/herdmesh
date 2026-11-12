//! Append-only signed event log and SQLite materialized views.
//!
//! M0 stub: real code lands in later milestones (see docs/plans/).
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod settle;

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
