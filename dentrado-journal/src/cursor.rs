//! Where a record lives.

use std::fmt;

/// A record's address: the segment it was appended to and the byte offset of
/// its frame within that segment.
///
/// Cursors are totally ordered by `(segment, offset)`, and because segments are
/// append-only that order is the log's order. Appending never moves an existing
/// record, so a cursor is a durable resume point — a consumer that remembers
/// one has applied everything strictly before it.
///
/// [`Cursor::START`] is before the first record; the cursor returned by
/// [`Journal::head`](crate::Journal::head) is just past the last one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor {
    /// The segment id.
    pub segment: u64,
    /// The byte offset of the frame inside that segment.
    pub offset: u64,
}

impl Cursor {
    /// Before every record: a full replay.
    pub const START: Self = Self::new(0, 0);

    /// An address, spelled out.
    #[must_use]
    pub const fn new(segment: u64, offset: u64) -> Self {
        Self { segment, offset }
    }
}

impl fmt::Display for Cursor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.segment, self.offset)
    }
}

#[cfg(test)]
mod tests {
    use super::Cursor;

    #[test]
    fn orders_by_segment_then_offset() {
        assert!(Cursor::new(0, 99) < Cursor::new(1, 0));
        assert!(Cursor::new(1, 10) < Cursor::new(1, 11));
        assert!(Cursor::START < Cursor::new(0, 32));
    }
}
