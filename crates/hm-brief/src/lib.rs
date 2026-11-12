//! Token-budgeted briefs and digests built from the store (MASTER_PROMPT §2).
//!
//! - L0 [`room_card`]: who/what/where in ≤ [`ROOM_CARD_MAX`] tokens.
//! - L1 [`brief`]: what agent X missed since its cursor, relevance-filtered,
//!   ≤ `max_tokens` (default [`BRIEF_DEFAULT_MAX`]).
//! - L2 [`get_event`] / [`get_thread`]: full detail by id, on demand.
//! - [`build_digest`]: deterministic summary of everything up to a lamport.
//!
//! Every peer-authored string goes through [`wrap_untrusted`]. Whole items
//! are kept or dropped, never cut; least relevant items are dropped first.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod brief;
mod card;
mod detail;
mod digest;
mod fit;
mod text;

pub use brief::{Brief, brief};
pub use card::room_card;
pub use detail::{find_event, get_event, get_thread};
pub use digest::{Digest, build_digest};
pub use hm_proto::budget::{BRIEF_DEFAULT_MAX, ROOM_CARD_MAX};
pub use text::{
    ITEM_TEXT_MAX_CHARS, LEGEND, agent_label, clean, short_id, short_node, wrap_max, wrap_untrusted,
};

/// Default digest cap in tokens. Lives here because `hm-proto::budget` is
/// outside this crate's lease; the lead may move it.
pub const DIGEST_DEFAULT_MAX: usize = 200;

/// Crate name, used by `herdmesh doctor` and tests to prove linkage.
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");

/// Errors from building views.
#[derive(Debug, thiserror::Error)]
pub enum BriefError {
    #[error("store: {0}")]
    Store(#[from] hm_store::StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_name_matches_package() {
        assert_eq!(CRATE_NAME, "hm-brief");
    }
}
