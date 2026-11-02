//! The store's one error type.

/// A store operation error. `Io` — the disk refused; `Eof` — a read ran
/// past the file's end (a page never written); `Steps` — the step budget
/// ran out (image corruption); the rest are honest protocol refusals (no
/// such table, no such draft, the pool is taken, etc.). No contested
/// pages remain: the pool waits for flights and capacity on its own.
///
/// `Poisoned` is terminal and process-wide: the driver or a round
/// panicked, or an operation failed hard enough to poison the store —
/// every later operation refuses. The store is never inspected or
/// reused after that; the caller degrades (the cache is recomputable).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    Io(String),
    Eof,
    CorruptImage,
    /// A name outside the format's bounds (empty, over
    /// [`crate::core::sb::MAX_TABLE_NAME`] bytes, or a duplicate) —
    /// `open` refuses the whole set.
    BadName,
    /// The table's name is not in the store's set (or the slot is
    /// dropped).
    NoTable,
    UnknownDraft,
    PoolTaken,
    /// The store is poisoned — see the type's doc. Terminal.
    Poisoned,
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
            Self::BadName => write!(f, "open: a name outside the format"),
            Self::NoTable => write!(f, "table: no such table"),
            Self::Poisoned => write!(f, "store: poisoned"),
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
