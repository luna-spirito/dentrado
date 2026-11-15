//! The log itself: a directory of segments, the schemas interned in it, and
//! the cursor just past its last record.

mod open;
mod write;

use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use compio::fs::File;

use crate::acked::{self, Acked};
use crate::cursor::Cursor;
use crate::error::{Error, ReplayError};
use crate::frame::Frame;
use crate::schema::{Registry, SchemaId};
use crate::segment::{self, Reader};

/// How a log is laid out beyond the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// How large a data segment may grow before the next append starts a new
    /// one. The unit is the batch: an append that would cross the boundary
    /// rotates as a whole, and a batch larger than the limit gets a segment of
    /// its own.
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
/// One handle per directory: `open` takes the directory's lock, so a second
/// handle is refused rather than allowed to interleave. Reads and writes share
/// one handle — [`Journal::replay_from`] takes `&self`, so nesting a replay
/// inside a writer's loop is fine, but opening the same directory twice is not.
///
/// After an append, a sync or a close returns an error, the journal is
/// poisoned: the failed write left a tail no writer can reason about, so every
/// later call fails with [`Error::Poisoned`] until the handle is dropped and
/// the log reopened.
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
    acked: Option<Acked>,
    recorder: acked::Recorder,
    broken: bool,
    _lock: std::fs::File,
}

impl Journal {
    /// Opens (or creates) the log in `dir`.
    ///
    /// This is a pure function of the files present: declarations are read from
    /// segment zero, the last segment's unfinished tail is truncated, and
    /// closed segments are only checked for a header and a footer. Repair is
    /// the only thing it writes apart from creating the directory and the
    /// `acked` file. The directory is locked for as long as the journal lives,
    /// and nothing is created for the lock.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] for interior damage, a hole in the segment ids, a
    /// declaration that contradicts the registry, a record naming an undeclared
    /// schema, or damage at or below the acked head. [`Error::Version`] for a
    /// log this build cannot read, [`Error::Locked`] for a second writer.
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

    /// Just past the last record — the append cursor, and the cursor a consumer
    /// resumes from. Not the durability boundary: everything at or below
    /// [`Journal::acked`] is durable, everything above it is only written.
    #[must_use]
    pub fn head(&self) -> Cursor {
        self.head
    }

    /// The durable boundary: everything before it survived a [`Journal::sync`],
    /// which is what a consumer may treat as promised.
    #[must_use]
    pub fn acked(&self) -> Option<Cursor> {
        self.acked.and_then(|acked| acked.data)
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

    /// Makes every append so far durable, and says so.
    ///
    /// The segments are synced first and the acked head second, so the record
    /// of the ack can never run ahead of the data it describes: a crash between
    /// the two leaves the head behind, which only makes the next open more
    /// forgiving. Nothing is written when there is nothing new to ack.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem says; the log stays a valid prefix, and the
    /// journal is poisoned.
    pub async fn sync(&mut self) -> Result<(), Error> {
        self.live()?;
        let done = self.sync_segments().await;
        self.poison(done)
    }

    /// Closes the segment being written, so that the next append rotates.
    ///
    /// Not needed for correctness — a crash leaves a tail that the next open
    /// truncates — but it keeps a clean shutdown's log fully closed.
    ///
    /// # Errors
    ///
    /// Whatever the filesystem says; the journal is poisoned.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.live()?;
        let done = async {
            // Declarations first: sealing the data segment must never make a
            // record durable before the declaration it names, or a crash leaves
            // a closed segment that can never be interpreted.
            if let Some(writer) = &mut self.decl {
                writer.sync().await?;
            }
            if let Some(mut writer) = self.active.take() {
                writer.close().await?;
            }
            self.sync_segments().await
        }
        .await;
        self.poison(done)
    }

    async fn sync_segments(&mut self) -> Result<(), Error> {
        if let Some(writer) = &mut self.decl {
            writer.sync().await?;
        }
        if let Some(writer) = &mut self.active {
            writer.sync().await?;
        }
        self.record_ack().await
    }

    /// Writes down how far the log is acked, skipping the write when nothing
    /// moved. Called only after the segments themselves are durable.
    async fn record_ack(&mut self) -> Result<(), Error> {
        let acked = Acked {
            decl_end: self.decl_end.unwrap_or(0),
            data: match self.last {
                Some(segment) if segment != segment::DECL => {
                    Some(Cursor::new(segment, self.last_end))
                }
                _ => None,
            },
        };
        if acked.is_empty() || Some(acked) == self.acked {
            return Ok(());
        }
        self.recorder.save(acked).await?;
        self.acked = Some(acked);
        Ok(())
    }

    /// Fails every call once a write has failed: the tail is half-applied, and
    /// only a reopen — which truncates it — can tell how much landed.
    fn live(&self) -> Result<(), Error> {
        if self.broken {
            Err(Error::Poisoned)
        } else {
            Ok(())
        }
    }

    /// Marks the journal unusable if `done` failed, and hands it back.
    fn poison<T>(&mut self, done: Result<T, Error>) -> Result<T, Error> {
        if done.is_err() {
            self.broken = true;
        }
        done
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
    /// stops with an error. A visitor that breaks is never followed by a log
    /// error — the replay stops there — so the visitor's own error is the one
    /// that always comes back.
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
            // An empty segment still counts as visited: the cursor must land on
            // `head()` after a full replay, including a rotation a crash left
            // with no records yet.
            if start > end {
                continue;
            }
            let mut reader = Reader::new(file);
            let registry = &self.registry;
            let mut consumed = 0;
            let walked = segment::walk(
                &mut reader,
                segment,
                start,
                end,
                async |cursor, frame: Frame<'_>| {
                    consumed = frame.encoded_len() as u64;
                    // Segment zero holds declarations and nothing else; every other
                    // segment holds consumer records and nothing else. Format-owned
                    // ids (a stray purge, say) never appear in a data stream.
                    let belongs = if segment == segment::DECL {
                        frame.schema == SchemaId::DECL
                    } else {
                        frame.schema.is_user()
                    };
                    if !belongs {
                        return Err(Error::corrupt(
                            segment,
                            cursor.offset,
                            "a record with a schema that does not belong here",
                        ));
                    }
                    if registry.get(frame.schema).is_none() {
                        return Err(Error::Undeclared { id: frame.schema.0 });
                    }
                    visit(cursor, frame).await
                },
            )
            .await?;
            // The visitor stopped before the record it broke on; it saw that
            // record, so the resume point is past it.
            at = Cursor::new(
                segment,
                walked.end + if walked.broke { consumed } else { 0 },
            );
            if walked.broke {
                return Ok(at);
            }
            if walked.end != end {
                // The log holds the directory's lock, so nobody else can be
                // writing here: a walk that stops short of the head is damage,
                // including in the segment we are appending to.
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

#[cfg(test)]
mod tests {
    use std::future::Future;

    use super::Journal;
    use crate::error::Error;
    use crate::frame::Frame;
    use crate::schema::SchemaId;
    use crate::segment;

    fn run<F: Future>(f: F) -> F::Output {
        compio::runtime::Runtime::new().unwrap().block_on(f)
    }

    #[test]
    fn a_format_owned_record_in_a_data_segment_is_refused() {
        run(async {
            let dir = tempfile::tempdir().unwrap();
            segment::Writer::create(dir.path(), segment::DECL)
                .await
                .unwrap()
                .close()
                .await
                .unwrap();
            let mut data = segment::Writer::create(dir.path(), 1).await.unwrap();
            let mut bytes = Vec::new();
            Frame::new(SchemaId::PURGE, b"not a consumer record").encode(&mut bytes);
            data.write(bytes).await.unwrap();
            data.close().await.unwrap();

            let opened = Journal::open(dir.path()).await.map(|_| ());
            assert!(
                matches!(opened, Err(Error::Corrupt { .. })),
                "a purge record is not consumer data: {opened:?}"
            );
        });
    }
}
