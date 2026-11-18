//! The one error type: a journal is either readable or it is not.

use std::error;
use std::fmt;
use std::io;

/// Why a declaration disagrees with the registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// A user schema id was declared for the reserved range.
    Reserved,
    /// The shape hash is not `blake3(shape)`.
    HashMismatch,
    /// This id already means a different shape.
    Redefined,
    /// This shape is already interned under another id.
    DuplicateShape,
    /// This shape is already interned under a different name.
    Renamed,
}

impl fmt::Display for Conflict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Reserved => "reserved id",
            Self::HashMismatch => "shape hash mismatch",
            Self::Redefined => "id redefined",
            Self::DuplicateShape => "shape already declared",
            Self::Renamed => "shape renamed",
        })
    }
}

/// Everything that can go wrong with a journal.
#[derive(Debug)]
pub enum Error {
    /// The filesystem said no.
    Io(io::Error),
    /// Bytes that must be a frame are not one — and they are not the tail.
    Corrupt {
        /// The segment the damage was found in.
        segment: u64,
        /// The byte offset within that segment.
        offset: u64,
        /// What was expected there.
        what: &'static str,
    },
    /// A declaration record's payload does not decode.
    Malformed(&'static str),
    /// A record names a schema id the format owns (`0..16`).
    Reserved {
        /// The id that was used.
        id: u16,
    },
    /// A record names a schema id this log never declared.
    Undeclared {
        /// The id that was used.
        id: u16,
    },
    /// Two declarations cannot coexist.
    Schema {
        /// The id being declared.
        id: u16,
        /// How the two disagree.
        conflict: Conflict,
        /// The declaration already in the registry, as `name = shape`.
        declared: String,
        /// The declaration being refused, as `name = shape`.
        incoming: String,
    },
    /// A name, shape or payload exceeds its cap.
    TooLarge {
        /// What is too long.
        what: &'static str,
        /// Its size in bytes.
        len: usize,
        /// The cap.
        max: usize,
    },
    /// A segment was written by a format version this build does not know.
    Version {
        /// The segment whose header disagrees.
        segment: u64,
        /// The version in that header.
        found: u16,
    },
    /// Every `u16` user schema id is taken.
    RegistryFull,
    /// The log declares a nested value layout that belongs to another runtime.
    /// Its event records carry no body schema of their own, so the bytes would
    /// be decoded under the wrong shape.
    ForeignValue {
        /// The schema id that declared it.
        id: u16,
        /// The declaration's name.
        name: String,
        /// Its foreign shape.
        shape: String,
    },
    /// A write failed and the journal refuses further work. Drop it and reopen:
    /// the failed write left a tail that only a reopen can truncate.
    Poisoned,
    /// Another writer holds this directory.
    Locked,
}

impl Error {
    /// Damage at a known place in a segment.
    #[must_use]
    pub fn corrupt(segment: u64, offset: u64, what: &'static str) -> Self {
        Self::Corrupt {
            segment,
            offset,
            what,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "io: {e}"),
            Self::Corrupt {
                segment,
                offset,
                what,
            } => write!(f, "segment {segment} at {offset}: {what}"),
            Self::Malformed(what) => write!(f, "malformed declaration: {what}"),
            Self::Reserved { id } => write!(f, "schema {id} belongs to the format"),
            Self::Undeclared { id } => write!(f, "schema {id} was never declared"),
            Self::Schema {
                id,
                conflict,
                declared,
                incoming,
            } => write!(
                f,
                "schema {id}: {conflict}: declared `{declared}`, incoming `{incoming}`"
            ),
            Self::TooLarge { what, len, max } => {
                write!(f, "{what} is {len} bytes, the cap is {max}")
            }
            Self::Version { segment, found } => {
                write!(
                    f,
                    "segment {segment} uses the unsupported log version {found}"
                )
            }
            Self::RegistryFull => f.write_str("every user schema id is taken"),
            Self::ForeignValue { id, name, shape } => write!(
                f,
                "schema {id} declares `{name}` with the foreign shape `{shape}`"
            ),
            Self::Poisoned => f.write_str("a write failed; reopen the journal"),
            Self::Locked => f.write_str("another writer holds this journal"),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

/// Why a replay stopped.
///
/// A replay has two ways to fail: the log itself (damage, io) or the visitor,
/// which may refuse a record it cannot apply. Keeping them apart lets a
/// consumer propagate its own error type without having to encode it as
/// journal damage.
#[derive(Debug)]
pub enum ReplayError<E> {
    /// The log could not be read, or is damaged.
    Log(Error),
    /// The visitor refused a record.
    Visit(E),
}

impl<E> From<Error> for ReplayError<E> {
    fn from(e: Error) -> Self {
        Self::Log(e)
    }
}

impl<E: fmt::Display> fmt::Display for ReplayError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Log(e) => write!(f, "log: {e}"),
            Self::Visit(e) => write!(f, "replay stopped: {e}"),
        }
    }
}

impl<E: error::Error + 'static> error::Error for ReplayError<E> {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Self::Log(e) => Some(e),
            Self::Visit(e) => Some(e),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
