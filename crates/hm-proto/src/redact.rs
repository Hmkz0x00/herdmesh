//! Conservative outbound secret filtering, with no I/O or logging.
//!
//! A fixed number of scans, bounded regex patterns and monotonic cursors keep work
//! linear in input bytes. Inputs or expanded outputs over 256 KiB fail closed.
//! Entropy is a heuristic, not a guarantee: encoded/custom secrets may need more
//! context. Only entropy/AWS-neighbour detection exempts `git:` SHA references.
//! Shannon thresholds are 3.5 bits/byte for hex and 4.3 for base64 (minimum 32
//! bytes); lowercase snake_case identifiers are excluded. AWS secret proximity
//! is at most 256 bytes from an access ID. Marker labels are stable API.
use regex::Regex;
use std::sync::OnceLock;

pub const MAX_INPUT_BYTES: usize = 256 * 1024;
const AWS_NEAR_BYTES: usize = 256;
const HEX_ENTROPY: f64 = 3.5;
const BASE64_ENTROPY: f64 = 4.3;

/// Reason for each replacement, in source order (duplicates are retained).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    PrivateKey,
    AwsAccessKey,
    AwsSecretKey,
    GithubToken,
    OpenAiKey,
    AnthropicKey,
    SlackToken,
    GoogleApiKey,
    Jwt,
    Assignment,
    Bearer,
    Password,
    Entropy,
    InputLimit,
    FilterError,
}
impl Kind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PrivateKey => "private_key",
            Self::AwsAccessKey => "aws_access_key",
            Self::AwsSecretKey => "aws_secret_key",
            Self::GithubToken => "github_token",
            Self::OpenAiKey => "openai_key",
            Self::AnthropicKey => "anthropic_key",
            Self::SlackToken => "slack_token",
            Self::GoogleApiKey => "google_api_key",
            Self::Jwt => "jwt",
            Self::Assignment => "assignment",
            Self::Bearer => "bearer",
            Self::Password => "password",
            Self::Entropy => "entropy",
            Self::InputLimit => "input_limit",
            Self::FilterError => "filter_error",
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redacted {
    pub text: String,
    pub hits: Vec<Kind>,
}

struct Patterns {
    pem: Regex,
    bearer: Regex,
    credential: Regex,
}
fn patterns() -> Result<&'static Patterns, &'static regex::Error> {
    static PATTERNS: OnceLock<Result<Patterns, regex::Error>> = OnceLock::new();
    PATTERNS.get_or_init(|| Ok(Patterns {
        credential: Regex::new(r"(?:AKIA|ASIA)[A-Z0-9]{16}|(?:ghp_|gho_|ghs_|github_pat_|sk-ant-|sk-)[A-Za-z0-9_-]{20}|(?:xox[bparsec]-|xapp-)[A-Za-z0-9_-]{10}|AIza[A-Za-z0-9_-]{35}")?,
        pem: Regex::new(r"-----BEGIN ([A-Z0-9][A-Z0-9 -]{0,96})-----")?,
        bearer: Regex::new(r"(?i)\bauthorization[ \t]{0,16}:[ \t]{0,16}bearer[ \t]{1,16}")?,
    })).as_ref()
}
fn closed(kind: Kind) -> Redacted {
    Redacted {
        text: format!("[REDACTED:{}]", kind.as_str()),
        hits: vec![kind],
    }
}
fn marker(s: &str) -> bool {
    let Some(label) = s
        .strip_prefix("[REDACTED:")
        .and_then(|s| s.strip_suffix(']'))
    else {
        return false;
    };
    matches!(
        label,
        "private_key"
            | "aws_access_key"
            | "aws_secret_key"
            | "github_token"
            | "openai_key"
            | "anthropic_key"
            | "slack_token"
            | "google_api_key"
            | "jwt"
            | "assignment"
            | "bearer"
            | "password"
            | "entropy"
            | "input_limit"
            | "filter_error"
    )
}
fn mark(tags: &mut [Option<Kind>], mut start: usize, mut end: usize, kind: Kind) {
    if start == end {
        return;
    }
    // Union overlapping contexts instead of leaving fragments that change on a
    // second pass. Earlier (more specific) reasons win over enclosing contexts.
    let chosen = tags[start..end]
        .iter()
        .flatten()
        .next()
        .copied()
        .unwrap_or(kind);
    if tags[start..end].iter().all(|tag| *tag == Some(chosen)) {
        return;
    }
    if let Some(left) = tags[start] {
        while start > 0 && tags[start - 1] == Some(left) {
            start -= 1;
        }
    }
    if let Some(right) = tags[end - 1] {
        while end < tags.len() && tags[end] == Some(right) {
            end += 1;
        }
    }
    tags[start..end].fill(Some(chosen));
}
fn token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_-/+.".contains(&b)
}
fn base64_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"_-/+=".contains(&b)
}
fn git_sha(input: &str, start: usize, token: &str) -> bool {
    start >= 4
        && &input.as_bytes()[start - 4..start] == b"git:"
        && token.len() == 40
        && token.bytes().all(|b| b.is_ascii_hexdigit())
}
fn family(token: &str) -> Option<Kind> {
    let suffix = |prefix: &str, min: usize| {
        token.strip_prefix(prefix).is_some_and(|s| {
            s.len() >= min
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        })
    };
    if token.len() == 20
        && (token.starts_with("AKIA") || token.starts_with("ASIA"))
        && token
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Some(Kind::AwsAccessKey);
    }
    if ["ghp_", "gho_", "ghs_", "github_pat_"]
        .iter()
        .any(|p| suffix(p, 20))
    {
        return Some(Kind::GithubToken);
    }
    if suffix("sk-ant-", 20) {
        return Some(Kind::AnthropicKey);
    }
    if suffix("sk-", 20) {
        return Some(Kind::OpenAiKey);
    }
    if [
        "xoxb-", "xoxp-", "xoxa-", "xoxr-", "xoxs-", "xoxe-", "xoxc-", "xapp-",
    ]
    .iter()
    .any(|p| suffix(p, 10))
    {
        return Some(Kind::SlackToken);
    }
    if suffix("AIza", 35) {
        return Some(Kind::GoogleApiKey);
    }
    let mut parts = token.split('.');
    if let (Some(a), Some(b), Some(c), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    {
        let url = |s: &str| {
            s.len() >= 8
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        };
        if a.starts_with("eyJ") && url(a) && url(b) && url(c) {
            return Some(Kind::Jwt);
        }
    }
    None
}
fn high_entropy(token: &str) -> bool {
    if token.len() < 32 || !token.bytes().all(base64_byte) {
        return false;
    }
    let hex = token.bytes().all(|b| b.is_ascii_hexdigit());
    // Lowercase snake_case identifiers are ordinary code, not standard base64.
    if token.contains('_') && token.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') {
        return false;
    }
    let mut counts = [0usize; 128];
    for b in token.bytes() {
        counts[b as usize] += 1;
    }
    let len = token.len() as f64;
    let entropy: f64 = counts
        .into_iter()
        .filter(|&n| n > 0)
        .map(|n| {
            let p = n as f64 / len;
            -p * p.log2()
        })
        .sum();
    entropy >= if hex { HEX_ENTROPY } else { BASE64_ENTROPY }
}

/// Replace sensitive spans while preserving their surrounding text and keys.
/// Already-generated markers remain untouched. Oversize input/output fails closed.
/// `hits` describes replacements; re-filtering `text` yields the same text, no hits.
pub fn redact(input: &str) -> Redacted {
    if input.len() > MAX_INPUT_BYTES {
        return closed(Kind::InputLimit);
    }
    let Ok(p) = patterns() else {
        return closed(Kind::FilterError);
    };
    let bytes = input.as_bytes();
    let mut tags = vec![None; input.len()];
    // PEM blocks outrank all inner credentials. An incomplete private block consumes
    // the remaining input rather than allowing truncated key material to escape.
    let mut consumed = 0;
    for m in p.pem.find_iter(input) {
        let label = &m.as_str()[11..m.len() - 5];
        if m.start() < consumed || !label.ends_with("PRIVATE KEY") {
            continue;
        }
        let end_marker = format!("-----END {label}-----");
        let end = input[m.end()..]
            .find(&end_marker)
            .map_or(input.len(), |n| m.end() + n + end_marker.len());
        mark(&mut tags, m.start(), end, Kind::PrivateKey);
        consumed = end;
    }
    for m in p.bearer.find_iter(input) {
        let end = bytes[m.end()..]
            .iter()
            .position(|b| b.is_ascii_whitespace() || b"\"',;".contains(b))
            .map_or(input.len(), |n| m.end() + n);
        if !marker(&input[m.end()..end]) {
            mark(&mut tags, m.end(), end, Kind::Bearer);
        }
    }
    consumed = 0;
    for (colon, _) in input.match_indices("://") {
        if colon < consumed {
            continue;
        }
        let mut scheme = colon;
        while scheme > 0
            && (bytes[scheme - 1].is_ascii_alphanumeric() || b"+.-".contains(&bytes[scheme - 1]))
        {
            scheme -= 1;
        }
        if scheme == colon || !bytes[scheme].is_ascii_alphabetic() {
            continue;
        }
        let authority_start = colon + 3;
        let end = bytes[authority_start..]
            .iter()
            .position(|b| b.is_ascii_whitespace() || b"/\"'<>?#".contains(b))
            .map_or(input.len(), |n| authority_start + n);
        consumed = end;
        let authority = &input[authority_start..end];
        if let Some(at) = authority.rfind('@') {
            if let Some(colon) = authority[..at].find(':') {
                let start = authority_start + colon + 1;
                let end = authority_start + at;
                if !marker(&input[start..end]) {
                    mark(&mut tags, start, end, Kind::Password);
                }
            }
        }
    }
    consumed = 0;
    for (equal, _) in input.match_indices('=') {
        if equal < consumed {
            continue;
        }
        let mut name_end = equal;
        while name_end > 0 && b" \t".contains(&bytes[name_end - 1]) {
            name_end -= 1;
        }
        let mut name_start = name_end;
        while name_start > 0
            && (bytes[name_start - 1].is_ascii_alphanumeric() || bytes[name_start - 1] == b'_')
        {
            name_start -= 1;
        }
        if name_start == name_end {
            continue;
        }
        let upper = input[name_start..name_end].to_ascii_uppercase();
        if !["SECRET", "TOKEN", "PASSWORD", "API_KEY", "PRIVATE"]
            .iter()
            .any(|s| upper.contains(s))
        {
            continue;
        }
        let mut start = equal + 1;
        while start < bytes.len() && b" \t".contains(&bytes[start]) {
            start += 1;
        }
        let quote = bytes
            .get(start)
            .copied()
            .filter(|b| *b == b'\'' || *b == b'"');
        if quote.is_some() {
            start += 1;
        }
        let mut end = start;
        let mut escaped = false;
        while let Some(&b) = bytes.get(end) {
            if quote.is_some() && tags[end] == Some(Kind::PrivateKey) {
                while end < tags.len() && tags[end] == Some(Kind::PrivateKey) {
                    end += 1;
                }
                escaped = false;
                continue;
            }
            if b == b'\n' || b == b'\r' {
                break;
            }
            if let Some(q) = quote {
                if b == q && !escaped {
                    break;
                }
                escaped = b == b'\\' && !escaped;
            } else if b.is_ascii_whitespace() || b";,#\"'".contains(&b) {
                break;
            }
            end += 1;
        }
        consumed = end;
        let value = &input[start..end];
        // Bare Rust expressions are not dotenv values. Quoted literals are values.
        let before_name = bytes[name_start.saturating_sub(32)..name_start].trim_ascii_end();
        let rust_binding = before_name.ends_with(b"let") || before_name.ends_with(b"let mut");
        if quote.is_none() && (rust_binding || value.contains(['(', ')', '{', '}']) || value == "=")
        {
            continue;
        }
        if !marker(value) {
            mark(&mut tags, start, end, Kind::Assignment);
        }
    }
    // Prefix matches are bounded; extend each once so long synthetic credentials
    // cannot leak their suffix. Non-sensitive field names never hide a known key.
    let mut aws = Vec::new();
    consumed = 0;
    for m in p.credential.find_iter(input) {
        if m.start() < consumed {
            continue;
        }
        let mut end = m.end();
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || b"_-".contains(&bytes[end]))
        {
            end += 1;
        }
        consumed = end;
        if let Some(kind) = family(&input[m.start()..end]) {
            if kind == Kind::AwsAccessKey {
                aws.push(m.start());
            }
            mark(&mut tags, m.start(), end, kind);
        }
    }
    // Collect disjoint ASCII tokens once; all subsequent token work is linear.
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !token_byte(bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && token_byte(bytes[i]) {
            i += 1;
        }
        let mut padding = i;
        while padding < bytes.len() && bytes[padding] == b'=' {
            padding += 1;
        }
        if padding == bytes.len() || !token_byte(bytes[padding]) {
            i = padding;
        }
        // Sentence punctuation is not part of a key; JWT interior dots remain.
        let mut end = i;
        while end > start && bytes[end - 1] == b'.' {
            end -= 1;
        }
        if start == end {
            continue;
        }
        if end - start < 26 || tags[start..end].iter().any(Option::is_some) {
            continue;
        }
        let token = &input[start..end];
        if let Some(kind) = family(token) {
            mark(&mut tags, start, end, kind);
        }
        if end - start >= 32 {
            tokens.push((start, end));
        }
    }
    let mut next_aws = 0;
    for (start, end) in tokens {
        if tags[start..end].iter().any(Option::is_some) {
            continue;
        }
        let token = &input[start..end];
        if git_sha(input, start, token) {
            continue;
        }
        while next_aws < aws.len() && aws[next_aws] < start {
            next_aws += 1;
        }
        let near = aws
            .get(next_aws)
            .is_some_and(|&a| a.saturating_sub(end) <= AWS_NEAR_BYTES)
            || next_aws
                .checked_sub(1)
                .and_then(|n| aws.get(n))
                .is_some_and(|&a| start.saturating_sub(a + 20) <= AWS_NEAR_BYTES);
        if near && token.len() == 40 && token.bytes().all(base64_byte) {
            mark(&mut tags, start, end, Kind::AwsSecretKey);
        } else if high_entropy(token) {
            mark(&mut tags, start, end, Kind::Entropy);
        }
    }
    let mut text = String::with_capacity(input.len());
    let mut hits = Vec::new();
    let mut start = 0;
    while start < tags.len() {
        let tag = tags[start];
        let mut end = start + 1;
        while end < tags.len() && tags[end] == tag {
            end += 1;
        }
        if let Some(kind) = tag {
            text.push_str("[REDACTED:");
            text.push_str(kind.as_str());
            text.push(']');
            hits.push(kind);
        } else {
            text.push_str(&input[start..end]);
        }
        if text.len() > MAX_INPUT_BYTES {
            return closed(Kind::InputLimit);
        }
        start = end;
    }
    Redacted { text, hits }
}
/// True for detected secrets, oversized input, or an unavailable filter.
pub fn contains_secret(text: &str) -> bool {
    !redact(text).hits.is_empty()
}
