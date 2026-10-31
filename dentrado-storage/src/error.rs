//! The store's one error type.

/// A store operation error. `Io` — the disk refused; `Steps` — the step
/// budget ran out (image corruption); the rest are honest protocol
/// refusals (no such draft, the pool is taken, etc.). No contested pages
/// remain: the pool waits for flights and capacity on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    CorruptImage,
    /// The image's table count differs from the opener's fixed set.
    TableMismatch,
    UnknownDraft,
    PoolTaken,
    ConsumedVanished,
    ChannelClosed,
    Steps(&'static str),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::CorruptImage => write!(f, "open: corrupt image"),
            Self::TableMismatch => write!(f, "open: table count mismatch"),
            Self::UnknownDraft => write!(f, "draft: unknown"),
            Self::PoolTaken => write!(f, "alloc: pool taken"),
            Self::ConsumedVanished => write!(f, "put: consumed vanished"),
            Self::ChannelClosed => write!(f, "channel closed"),
            Self::Steps(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for StoreError {}
