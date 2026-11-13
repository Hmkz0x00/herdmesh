//! Automatic person and agent names (ADR-004 section 1).
//!
//! * person: `person` in `herdmesh.toml`, else the OS user name reduced to
//!   `[a-z0-9-]`, at most 16 chars.
//! * agent: `<person>-<kind>`, then `-2`, `-3` ... when another live agent of
//!   the same kind already runs on this machine. "Live" means its MCP process
//!   holds an advisory lock on `agents/<name>.lock`; the OS drops the lock when
//!   the process dies, so a crashed agent never blocks its name.
//!
//! Every name goes through the one validator in `hm_proto::names`.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

use hm_proto::names::{NAME_MAX, NameError, check_name, sanitize_name};

use crate::data::Config;

/// Longest auto-derived person name.
pub const PERSON_MAX: usize = 16;
/// Fallback when the OS user name has no usable characters.
pub const FALLBACK_PERSON: &str = "user";
/// Most same-kind agents we number before giving up.
const MAX_SUFFIX: u32 = 99;

#[derive(Debug, thiserror::Error)]
pub enum NamingError {
    #[error("{what}: {source}")]
    Invalid {
        what: &'static str,
        source: NameError,
    },
    #[error("agent name files: {0}")]
    Io(#[from] io::Error),
    #[error("more than {MAX_SUFFIX} '{0}' agents are running on this machine")]
    TooMany(String),
}

/// Validate a user-supplied name (CLI flag or config value).
pub fn validate(what: &'static str, s: &str) -> Result<String, NamingError> {
    check_name(s)
        .map(str::to_owned)
        .map_err(|source| NamingError::Invalid { what, source })
}

/// OS user name from environment-like lookups (`USERNAME`, `USER`, `LOGNAME`).
pub fn os_user(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    ["USERNAME", "USER", "LOGNAME"]
        .iter()
        .find_map(|k| env(k).filter(|v| !v.trim().is_empty()))
}

/// Person name derived from a raw OS user name.
pub fn derive_person(raw: Option<&str>) -> String {
    raw.and_then(|r| sanitize_name(r, PERSON_MAX))
        .unwrap_or_else(|| FALLBACK_PERSON.to_owned())
}

/// The person name for this machine: `herdmesh.toml` first, else the OS user.
pub fn person(cfg: &Config) -> Result<String, NamingError> {
    if let Some(p) = &cfg.person {
        return validate("person in herdmesh.toml", p);
    }
    let raw = os_user(|k| std::env::var(k).ok());
    Ok(derive_person(raw.as_deref()))
}

/// `<person>-<kind>` for n = 1, `<person>-<kind>-<n>` after that; always a
/// valid name (the base is shortened to leave room for the suffix).
pub fn candidate(person: &str, kind: &str, n: u32) -> Result<String, NamingError> {
    let kind = validate("agent kind", kind)?;
    let suffix = if n <= 1 {
        String::new()
    } else {
        format!("-{n}")
    };
    let mut base = format!("{person}-{kind}");
    base.truncate(NAME_MAX - suffix.len());
    let base = base.trim_end_matches('-');
    validate("agent name", &format!("{base}{suffix}"))
}

/// A claimed agent name. Keep it alive for as long as the agent runs.
#[derive(Debug)]
pub struct AgentName {
    pub name: String,
    _lock: File,
}

/// Claim the first free `<person>-<kind>[-n]` name on this machine.
// File::try_lock is newer than the workspace MSRV but the toolchain is pinned
// well above it; there is no std alternative for advisory locks.
#[allow(clippy::incompatible_msrv)]
pub fn claim_agent_name(dir: &Path, person: &str, kind: &str) -> Result<AgentName, NamingError> {
    std::fs::create_dir_all(dir)?;
    for n in 1..=MAX_SUFFIX {
        let name = candidate(person, kind, n)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.join(format!("{name}.lock")))?;
        match file.try_lock() {
            Ok(()) => return Ok(AgentName { name, _lock: file }),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
    }
    Err(NamingError::TooMany(format!("{person}-{kind}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn person_comes_from_the_os_user_name() {
        let env = |k: &str| (k == "USERNAME").then(|| "Hnkz.Oxoo".to_owned());
        assert_eq!(derive_person(os_user(env).as_deref()), "hnkz-oxoo");
        let none = |_: &str| None;
        assert_eq!(derive_person(os_user(none).as_deref()), "user");
        assert_eq!(derive_person(Some("!!!")), "user");
        assert_eq!(
            derive_person(Some("A-very-long-user-name-indeed")).len(),
            16
        );
        let blank = |k: &str| match k {
            "USERNAME" => Some("  ".to_owned()),
            "USER" => Some("Bob".to_owned()),
            _ => None,
        };
        assert_eq!(derive_person(os_user(blank).as_deref()), "bob");
    }

    #[test]
    fn config_person_wins_and_is_validated() {
        let cfg = Config {
            person: Some("alice".into()),
            ..Config::default()
        };
        assert_eq!(person(&cfg).unwrap(), "alice");
        let bad = Config {
            person: Some("Not Valid".into()),
            ..Config::default()
        };
        assert!(person(&bad).is_err());
    }

    #[test]
    fn candidates_number_duplicates_and_stay_valid() {
        assert_eq!(candidate("alice", "claude", 1).unwrap(), "alice-claude");
        assert_eq!(candidate("alice", "claude", 2).unwrap(), "alice-claude-2");
        assert_eq!(candidate("alice", "codex", 3).unwrap(), "alice-codex-3");
        let long = candidate(&"p".repeat(16), &"k".repeat(31), 12).unwrap();
        assert!(long.len() <= NAME_MAX && long.ends_with("-12"), "{long}");
        assert!(candidate("alice", "Bad Kind", 1).is_err());
        assert!(candidate("alice", "", 1).is_err());
    }

    #[test]
    fn second_live_agent_of_a_kind_gets_a_suffix_and_names_are_reused_after_exit() {
        let t = tempfile::tempdir().unwrap();
        let a = claim_agent_name(t.path(), "alice", "claude").unwrap();
        let b = claim_agent_name(t.path(), "alice", "claude").unwrap();
        let c = claim_agent_name(t.path(), "alice", "claude").unwrap();
        let other = claim_agent_name(t.path(), "alice", "codex").unwrap();
        assert_eq!(
            (a.name.as_str(), b.name.as_str(), c.name.as_str()),
            ("alice-claude", "alice-claude-2", "alice-claude-3")
        );
        assert_eq!(other.name, "alice-codex");
        drop(b);
        assert_eq!(
            claim_agent_name(t.path(), "alice", "claude").unwrap().name,
            "alice-claude-2"
        );
    }
}
