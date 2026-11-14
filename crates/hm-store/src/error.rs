//! Error type for `hm-store`.

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("invalid event: {0}")]
    Proto(#[from] hm_proto::ProtoError),
    #[error("codec: {0}")]
    Codec(#[from] postcard::Error),
    #[error("event or store belongs to a different room")]
    WrongRoom,
    #[error("corrupt store: bad {0}")]
    Corrupt(&'static str),
    #[error("encoded event is {0} bytes, above MAX_EVENT_BYTES")]
    Oversized(usize),
    #[error("lamport jumps too far past the local max")]
    LamportJump,
    #[error("lamport or seq clock exhausted")]
    ClockExhausted,
    #[error("store was written by an older, incompatible version; remove it and re-sync")]
    IncompatibleSchema,
    #[error("store is pinned to a different genesis event")]
    GenesisMismatch,
}
