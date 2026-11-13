//! The one validator for person and agent names (ADR-004 section 1).
//!
//! Names are signed into every event and shown to peers, so a name that
//! looks like a secret is rejected (GATE-3 N7). CLI flags, config, genesis,
//! session signing and the daemon all call [`check_name`].

use crate::redact::contains_secret;

/// Longest allowed name.
pub const NAME_MAX: usize = 32;

/// Why a name was rejected. `Display` never echoes a rejected name, because a
/// rejected name may be a secret the user pasted by mistake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("name is empty")]
    Empty,
    #[error("name is longer than {NAME_MAX} characters")]
    TooLong,
    #[error("name must start with a lowercase letter and contain only a-z, 0-9 and '-'")]
    BadChars,
    #[error("name looks like a secret and was rejected")]
    LooksSecret,
}

/// `[a-z][a-z0-9-]{0,31}` and not secret-looking.
pub fn check_name(s: &str) -> Result<&str, NameError> {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return Err(NameError::Empty);
    };
    if s.len() > NAME_MAX {
        return Err(NameError::TooLong);
    }
    if !first.is_ascii_lowercase()
        || !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(NameError::BadChars);
    }
    if contains_secret(s) {
        return Err(NameError::LooksSecret);
    }
    Ok(s)
}

/// Boolean form of [`check_name`].
pub fn valid_name(s: &str) -> bool {
    check_name(s).is_ok()
}

/// Reduce arbitrary text (an OS user name) to a valid name: lowercase,
/// `[a-z0-9-]` only, must start with a letter, at most `max` chars.
/// Returns `None` when nothing usable is left.
pub fn sanitize_name(raw: &str, max: usize) -> Option<String> {
    let mut out: String = raw
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    out = out
        .trim_start_matches(|c: char| !c.is_ascii_lowercase())
        .trim_end_matches('-')
        .to_owned();
    out.truncate(max.min(NAME_MAX));
    let out = out.trim_end_matches('-').to_owned();
    valid_name(&out).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_plain_names() {
        for ok in ["a", "alice", "alice-claude", "bob-2", "cli"] {
            assert_eq!(check_name(ok), Ok(ok));
        }
        assert!(valid_name(&format!("a{}", "b".repeat(31))));
    }

    #[test]
    fn rejects_bad_shapes() {
        assert_eq!(check_name(""), Err(NameError::Empty));
        assert_eq!(check_name(&"a".repeat(33)), Err(NameError::TooLong));
        for bad in [
            "Alice", "1abc", "-a", "a_b", "a.b", "a b", "a/b", "né", "a:b",
        ] {
            assert_eq!(check_name(bad), Err(NameError::BadChars), "{bad}");
        }
    }

    #[test]
    fn rejects_secret_looking_names_without_echoing_them() {
        // GATE-3 N7: a pasted token as --agent must never become an author label.
        let token = "ghp-abcdefghijklmnopqrstuvwxyz0123456789";
        let e = check_name(token).unwrap_err();
        assert!(matches!(e, NameError::LooksSecret | NameError::TooLong));
        assert!(!e.to_string().contains("ghp"));
        for secret in [
            "sk-ant-api03-AbCdEf0123456789",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_0123456789abcdef0123456789abcdef0123",
        ] {
            assert!(!valid_name(secret), "{secret}");
        }
    }

    #[test]
    fn sanitize_reduces_os_user_names() {
        assert_eq!(sanitize_name("Hnkzoxoo", 16).as_deref(), Some("hnkzoxoo"));
        assert_eq!(
            sanitize_name("John.Smith_Jr", 16).as_deref(),
            Some("john-smith-jr")
        );
        assert_eq!(sanitize_name("42Alice", 16).as_deref(), Some("alice"));
        assert_eq!(
            sanitize_name("averyveryverylongusername", 16).as_deref(),
            Some("averyveryverylon")
        );
        assert_eq!(sanitize_name("---", 16), None);
        assert_eq!(sanitize_name("", 16), None);
        assert_eq!(sanitize_name("trail--", 16).as_deref(), Some("trail"));
    }
}
