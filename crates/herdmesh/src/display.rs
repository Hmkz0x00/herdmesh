//! Terminal hygiene for peer-authored text (ADR-002 §5).
//!
//! Anything a peer wrote (agent names, task titles, lease paths, error text
//! that may carry a peer's close reason) goes through [`sanitize_display`]
//! before `room run` / `room status` print it, so a peer can't inject ANSI
//! escapes, move the cursor, fake extra lines, or flood the terminal.

/// Cap for names (agent, room).
pub const NAME_MAX: usize = 32;
/// Cap for task titles, lease paths and ids.
pub const TEXT_MAX: usize = 80;
/// Cap for error messages.
pub const ERROR_MAX: usize = 160;

pub use hm_daemon::display::sanitize_display;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(sanitize_display("alice", NAME_MAX), "alice");
        assert_eq!(sanitize_display("src/net/**", TEXT_MAX), "src/net/**");
        assert_eq!(sanitize_display("héllo 世界", TEXT_MAX), "héllo 世界");
    }

    #[test]
    fn ansi_escape_sequences_are_neutralised() {
        let evil = "\x1b[2J\x1b[31mred\x1b]0;title\x07\u{9b}31m";
        let out = sanitize_display(evil, 200);
        assert!(!out.chars().any(char::is_control), "{out:?}");
        assert_eq!(out, "\\x1b[2J\\x1b[31mred\\x1b]0;title\\x07\\x9b31m");
    }

    #[test]
    fn cr_lf_cannot_fake_lines() {
        let out = sanitize_display("ok\r\nsync      fake +999 new\n", 200);
        assert!(!out.contains('\n') && !out.contains('\r'), "{out:?}");
        assert_eq!(out, "ok\\r\\nsync      fake +999 new\\n");
    }

    #[test]
    fn backslash_is_doubled_so_escapes_cannot_be_forged() {
        assert_eq!(sanitize_display("a\\x1b", 20), "a\\\\x1b");
    }

    #[test]
    fn bidi_and_zero_width_are_visible() {
        let out = sanitize_display("a\u{202e}b\u{200b}c", 40);
        assert_eq!(out, "a\\u{202e}b\\u{200b}c");
    }

    #[test]
    fn length_is_capped_without_splitting_escapes() {
        let out = sanitize_display(&"x".repeat(100), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
        // Exactly max_len fits without an ellipsis.
        assert_eq!(sanitize_display("abcde", 5), "abcde");
        // An escape that doesn't fit is dropped whole.
        let out = sanitize_display("abc\x1bdef", 6);
        assert_eq!(out, "abc…");
        assert_eq!(sanitize_display("abc", 0), "");
    }
}
