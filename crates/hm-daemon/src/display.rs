//! Shared terminal hygiene for daemon feeds and CLI output.

const ELLIPSIS: char = '…';

/// Escaped form of `c` if it must not reach a terminal raw.
fn escape(c: char) -> Option<String> {
    match c {
        '\n' => Some("\\n".into()),
        '\r' => Some("\\r".into()),
        '\t' => Some("\\t".into()),
        '\\' => Some("\\\\".into()),
        // C0, DEL, C1 (incl. single-byte CSI U+009B).
        c if c.is_control() => Some(format!("\\x{:02x}", u32::from(c))),
        // Bidi overrides/isolates, zero-width and line/paragraph separators:
        // they reorder or hide what the reader sees.
        '\u{200b}'..='\u{200f}'
        | '\u{2028}'..='\u{202e}'
        | '\u{2060}'..='\u{2069}'
        | '\u{feff}' => Some(format!("\\u{{{:x}}}", u32::from(c))),
        _ => None,
    }
}

/// One-line, escape-free rendering of `s`, at most `max_len` chars.
///
/// Control characters (ESC, CR, LF, ...), bidi and zero-width characters are
/// shown as visible escapes; a backslash is doubled so escapes can't be
/// forged. Longer output is cut at a whole character / escape and ends with
/// `…` (counted in `max_len`).
pub fn sanitize_display(s: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut len = 0usize;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let piece = escape(c).unwrap_or_else(|| c.to_string());
        let n = piece.chars().count();
        // Keep one slot for the ellipsis unless this is the last piece.
        let room = if chars.peek().is_some() {
            max_len.saturating_sub(1)
        } else {
            max_len
        };
        if len + n > room {
            if max_len > 0 {
                out.push(ELLIPSIS);
            }
            return out;
        }
        out.push_str(&piece);
        len += n;
    }
    out
}
