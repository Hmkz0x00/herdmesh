//! Adapter over the herdr CLI.
//!
//! Provides inspection of running agents via `herdr agent list` and prompt
//! notifications via `herdr agent prompt`.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod agent;
pub mod cli;
pub mod error;
pub mod fake;

pub use agent::{
    HerdrAgent, HerdrAgentStatus, HerdrStatus, parse_agent_list_json, strip_control_chars,
};
pub use cli::{
    HERDR_EXE, Herdr, HerdrCli, SystemHerdrCli, list_agents, notify, resolve_herdr_bin,
    resolve_herdr_bin_with,
};
pub use error::{HerdrError, Result};
pub use fake::FakeHerdrCli;

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, "hm-herdr");
    }
}
