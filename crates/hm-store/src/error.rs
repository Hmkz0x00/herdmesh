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
}
