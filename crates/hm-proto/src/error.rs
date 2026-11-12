//! Error types for `hm-proto`.

/// Errors from encoding, decoding, signing and verifying events.
#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("postcard: {0}")]
    Postcard(#[from] postcard::Error),
    #[error("empty input")]
    Empty,
    #[error("unsupported wire version {0}")]
    UnsupportedVersion(u8),
    #[error("trailing bytes after event")]
    TrailingBytes,
    #[error("event id does not match content")]
    IdMismatch,
    #[error("author node is not a valid ed25519 key")]
    BadAuthorKey,
    #[error("bad signature")]
    BadSignature,
    #[error("event seq must start at 1")]
    ZeroSeq,
}

/// Errors from parsing a [`crate::Ref`] or one of its parts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RefParseError {
    #[error("unknown ref scheme (want git:, task: or event:)")]
    UnknownScheme,
    #[error("git sha must be 7-64 lowercase hex chars")]
    BadSha,
    #[error("git path must be non-empty, without '#' or control chars")]
    BadPath,
    #[error("line range must be L<n> or L<a>-<b> with 1 <= a <= b")]
    BadLines,
    #[error("task id must be 1-64 chars of [A-Za-z0-9_.-]")]
    BadTaskId,
    #[error("event id must be 64 lowercase hex chars")]
    BadEventId,
}
