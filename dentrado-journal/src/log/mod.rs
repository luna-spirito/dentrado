//! The log itself: a directory of segments, the schemas interned in it, and
//! the cursor just past its last record.

mod open;
mod write;

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use compio::fs::File;

use crate::cursor::Cursor;
use crate::error::{Error, ReplayError};
use crate::frame::Frame;
use crate::schema::{Registry, SchemaId};
use crate::segment::{self, Reader};

/// How a log is laid out beyond the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// How large a data segment may grow before the next record starts a new
    /// one. One record may exceed it; such a record gets a segment of its own.
    pub rotate_bytes: u64,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            rotate_bytes: segment::ROTATE_BYTES,
        }
    }
}

/// An append-only log.
///
/// Single-threaded and `!Send`, like the rest of the runtime: it holds open
/// files and does positional io through `compio`. An append reaches the file
/// immediately; it is only durable after [`Journal::sync`].
///
/// One writer per directory: the log takes no lock, so a second `open` of the
/// same directory would append at the same offsets. Readers ([`Journal::replay_from`])
/// are fine alongside a writer.
#[derive(Debug)]
pub struct Journal {
    dir: PathBuf,
    options: Options,
    registry: Registry,
    decl: Option<segment::Writer>,
    active: Option<segment::Writer>,
    last: Option<u64>,
    decl_end: Option<u64>,
    last_end: u64,
    head: Cursor,
    truncated: u64,
}

impl Journal {
    /// Opens (or creates) the log in `dir`.
    ///
    /// This is a pure function of the files present: declarations are read from
    /// segment zero, the last segment's torn tail is truncated, and closed
    /// segments are only checked for a header and a footer. Nothing is written
    /// unless there is damage to repair.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for interior damage, a hole in the segment ids, a
    /// declaration that contradicts the registry, or a record naming an
    /// undeclared schema. [`Error::Version`] for a log this build cannot read.
    pub async fn open(dir: impl AsRef<Path>) -> Result<Self, Error> {
        open::open(dir, Options::default()).await
    }

    /// [`Journal::open`] with non-default [`Options`].
    ///
    /// # Errors
    ///
    /// As [`Journal::open`].
    pub async fn open_with(dir: impl AsRef<Path>, options: Options) -> Result<Self, Error> {
        open::open(dir, options).await
    }

    /// Just past the last record: the durability boundary, and the cursor a
    /// consumer resumes from once it has applied everything.
    #[must_use]
    pub fn head(&self) -> Cursor {
        self.head
    }

    /// How many bytes of torn tail this open repaired.
    #[must_use]
    pub fn truncated(&self) -> u64 {
        self.truncated
    }

    /// The schemas interned so far.
    #[must_use]
    pub fn decls(&self) -> &Registry {
        &self.registry
    }

    /// Makes every append so far durable. One `fdatasync` per segment at most.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem says; the log stays a valid prefix.
    pub async fn sync(&mut self) -> Result<(), Error> {
        if let Some(writer) = &mut self.decl {
            writer.sync().await?;
        }
        if let Some(writer) = &mut self.active {
            writer.sync().await?;
        }
        Ok(())
    }

    /// Closes the segment being written, so that the next append rotates.
    ///
    /// Not needed for correctness — a crash leaves a tail that the next open
    /// truncates — but it keeps a clean shutdown's log fully closed.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem says.
    pub async fn close(&mut self) -> Result<(), Error> {
        if let Some(mut writer) = self.active.take() {
            writer.close().await?;
        }
        self.sync().await
    }

    /// Replays every record from `from` onwards.
    ///
    /// Returns the cursor just past the last record the visitor saw — the
    /// resume point, whether the replay ran to the end or the visitor stopped
    /// it. `visit` sees declarations too (they are records like any other).
    ///
    /// The visitor is async: it may await on the record (`Frame` borrows one
    /// read window, which stays valid until the next record is read), and it
    /// may stop with [`ControlFlow::Break`] after consuming it. Records are
    /// never decoded and never copied.
    ///
    /// Closed segments are verified as they are replayed — footer count and
    /// digest included — so a full replay is also a full integrity check. The
    /// segment being written is bounded by the head, and a torn tail stops the
    /// replay silently (it was already truncated when the log opened).
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for damage in any closed segment, [`Error::Undeclared`]
    /// for a record whose schema this log never declared.
    pub async fn replay_from<F>(&self, from: Cursor, visit: F) -> Result<Cursor, Error>
    where
        F: AsyncFnMut(Cursor, Frame<'_>) -> ControlFlow<()>,
    {
        let mut visit = visit;
        self.replay(from, async |cursor, frame: Frame<'_>| {
            Ok(visit(cursor, frame).await)
        })
        .await
    }

    /// [`Journal::replay_from`] with a visitor that may fail.
    ///
    /// The consumer's error is handed back untouched as [`ReplayError::Visit`],
    /// so a replay can be one `?`-chain with the work it feeds (a cache write,
    /// a fold that refuses a record).
    ///
    /// # Errors
    ///
    /// As [`Journal::replay_from`], plus [`ReplayError::Visit`] if the visitor
    /// stops with an error.
    pub async fn try_replay_from<E, F>(
        &self,
        from: Cursor,
        visit: F,
    ) -> Result<Cursor, ReplayError<E>>
    where
        F: AsyncFnMut(Cursor, Frame<'_>) -> Result<ControlFlow<()>, E>,
    {
        let mut visit = visit;
        let mut refused = None;
        let walked = self
            .replay(from, async |cursor, frame: Frame<'_>| {
                match visit(cursor, frame).await {
                    Ok(flow) => Ok(flow),
                    Err(e) => {
                        refused = Some(e);
                        Ok(ControlFlow::Break(()))
                    }
                }
            })
            .await;
        match (walked, refused) {
            (Err(e), _) => Err(ReplayError::Log(e)),
            (Ok(_), Some(e)) => Err(ReplayError::Visit(e)),
            (Ok(at), None) => Ok(at),
        }
    }

    /// Scrubs the whole log: every frame's crc and every closed segment's
    /// footer. One sequential read, no decoding.
    ///
    /// Run it on a schedule if the disk is suspect — it turns silent bit rot
    /// into a loud error before the damage reaches a backup.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for damage anywhere in the log.
    pub async fn verify(&self) -> Result<(), Error> {
        self.replay_from(Cursor::START, async |_, _| ControlFlow::Continue(()))
            .await
            .map(|_| ())
    }

    async fn replay<F>(&self, from: Cursor, mut visit: F) -> Result<Cursor, Error>
    where
        F: AsyncFnMut(Cursor, Frame<'_>) -> Result<ControlFlow<()>, Error>,
    {
        let Some(last) = self.last else {
            return Ok(from);
        };
        let mut at = from;
        for segment in from.segment..=last {
            if segment == segment::DECL && self.decl_end.is_none() {
                continue;
            }
            let file = File::open(self.dir.join(segment::file_name(segment))).await?;
            let len = file.metadata().await?.len();
            let Some(end) = self.end_of(segment, len) else {
                continue;
            };
            let start = if segment == from.segment {
                from.offset.max(segment::HEADER_LEN as u64)
            } else {
                segment::HEADER_LEN as u64
            };
            if start >= end {
                continue;
            }
            let mut reader = Reader::new(file);
            let registry = &self.registry;
            let walked = segment::walk(&mut reader, segment, start, end, async |cursor, frame| {
                if frame.schema == SchemaId::DECL && segment != segment::DECL {
                    return Err(Error::corrupt(
                        segment,
                        cursor.offset,
                        "a declaration outside the declaration segment",
                    ));
                }
                if registry.get(frame.schema).is_none() {
                    return Err(Error::Undeclared { id: frame.schema.0 });
                }
                visit(cursor, frame).await
            })
            .await?;
            at = Cursor::new(segment, walked.end);
            if walked.broke {
                return Ok(at);
            }
            if walked.end != end {
                // Only the segment being written may end early; `open` has
                // already truncated it, so this is a concurrent writer.
                if Some(segment) == self.last && self.active.is_some() {
                    return Ok(at);
                }
                return Err(Error::corrupt(segment, walked.end, "damaged frame"));
            }
            self.footer_checked(&mut reader, segment, start, end, len, &walked)
                .await?;
        }
        Ok(at)
    }

    fn end_of(&self, segment: u64, len: u64) -> Option<u64> {
        if segment == segment::DECL {
            self.decl_end
        } else if Some(segment) == self.last {
            Some(self.last_end)
        } else {
            Some(len.saturating_sub(segment::FOOTER_LEN as u64))
        }
    }

    /// Verifies a closed segment's footer, if what was replayed was the whole
    /// segment (a resumed replay cannot check it).
    async fn footer_checked(
        &self,
        reader: &mut Reader,
        segment: u64,
        start: u64,
        end: u64,
        len: u64,
        walked: &segment::Walked,
    ) -> Result<(), Error> {
        let whole = start == segment::HEADER_LEN as u64
            && end.saturating_add(segment::FOOTER_LEN as u64) == len;
        if !whole {
            return Ok(());
        }
        let bytes = reader.at(end, segment::FOOTER_LEN).await?;
        let footer = segment::decode_footer(bytes)
            .ok_or_else(|| Error::corrupt(segment, end, "no footer"))?;
        if footer.count != walked.count || footer.digest != *walked.digest.finalize().as_bytes() {
            return Err(Error::corrupt(segment, end, "the footer disagrees"));
        }
        Ok(())
    }
}
