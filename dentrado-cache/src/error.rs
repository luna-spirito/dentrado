//! The store's one error type.

/// A store operation error. `Io` — the disk refused; `Eof` — a read ran
/// past the file's end (a page never written); `Steps` — the step budget
/// ran out (image corruption); the rest are honest protocol refusals (no
/// such table, no such draft, the pool is taken, etc.). No contested
/// pages remain: the pool waits for flights and capacity on its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Eof,
    CorruptImage,
    /// The image's table count differs from the opener's fixed set.
    TableMismatch,
    /// The table index is outside the opener's fixed set.
    NoTable,
    UnknownDraft,
    PoolTaken,
    ConsumedVanished,
    /// A publication's lists disagree — a page both consumed and freed,
    /// a freed page still held (a draft, dying, back in a pool), a
    /// duplicate. Refused whole; nothing was registered.
    BadSeal(&'static str),
    ChannelClosed,
    Steps(&'static str),
}

impl StoreError {
    /// The fault line between absence and refusal: running past the
    /// file's end is absence (a never-written page), everything else
    /// the disk refusing.
    pub(crate) fn from_io(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::UnexpectedEof {
            Self::Eof
        } else {
            Self::Io(e.to_string())
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Eof => write!(f, "io: eof"),
            Self::CorruptImage => write!(f, "open: corrupt image"),
            Self::TableMismatch => write!(f, "open: table count mismatch"),
            Self::NoTable => write!(f, "table: no such table"),
            Self::UnknownDraft => write!(f, "draft: unknown"),
            Self::PoolTaken => write!(f, "alloc: pool taken"),
            Self::ConsumedVanished => write!(f, "put: consumed vanished"),
            Self::BadSeal(why) => write!(f, "put: {why}"),
            Self::ChannelClosed => write!(f, "channel closed"),
            Self::Steps(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for StoreError {}
