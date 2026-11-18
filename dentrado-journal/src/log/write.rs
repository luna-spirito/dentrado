//! Writing: interning a layout, appending a batch, creating the log's identity,
//! and the one place a segment is ever closed.
//!
//! The order of operations is the property that matters: a declaration reaches
//! the log before any record that names it, and a segment's footer is durable
//! before the next segment exists.

use crate::cursor::Cursor;
use crate::error::{Conflict, Error};
use crate::frame::{self, Frame};
use crate::schema::{self, Decl, DeclRecord, SchemaId};
use crate::segment;

use super::Journal;

impl Journal {
    /// Interns a layout, declaring it first if this log has not seen it.
    ///
    /// The id is stable for the log's lifetime: the same shape always gets the
    /// same id, and an id never changes meaning.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`schema::MAX_TEXT`], [`Error::Schema`] if the
    /// shape already belongs to another declaration, and [`Error::RegistryFull`]
    /// when the id space is exhausted.
    pub async fn intern(&mut self, decl: Decl<'_>) -> Result<SchemaId, Error> {
        self.live()?;
        let known = self
            .registry
            .find(decl.shape)
            .and_then(|id| self.registry.get(id));
        if let Some(declared) = known {
            if !declared.id.is_user() {
                return Err(Error::Reserved { id: declared.id.0 });
            }
            if declared.name == decl.name {
                return Ok(declared.id);
            }
            return Err(Error::Schema {
                id: declared.id.0,
                conflict: Conflict::Renamed,
                declared: declared.name.clone(),
                incoming: decl.name.to_owned(),
            });
        }
        for text in [decl.name, decl.shape] {
            if text.len() > schema::MAX_TEXT {
                return Err(Error::TooLarge {
                    what: "a declaration text",
                    len: text.len(),
                    max: schema::MAX_TEXT,
                });
            }
        }
        let record = DeclRecord::new(self.registry.next_id()?, decl);
        self.registry.check(&record)?;
        let done = self.declare(&record).await;
        self.poison(done)?;
        let id = record.id;
        self.registry.insert(record);
        Ok(id)
    }

    /// Appends a batch, returning the cursor just past it.
    ///
    /// Nothing is durable until [`Journal::sync`]; nothing is visible to a
    /// replay in any other segment until the batch is written.
    ///
    /// # Errors
    ///
    /// [`Error::Reserved`] for a format-owned schema id,
    /// [`Error::Undeclared`] for one this log never declared,
    /// [`Error::TooLarge`] for a payload past [`frame::MAX_PAYLOAD`].
    pub async fn append(&mut self, frames: &[Frame<'_>]) -> Result<Cursor, Error> {
        self.live()?;
        // Everything a record can be refused for is decided before a byte is
        // written, so a bad argument never costs the log its handle.
        let mut batch = Vec::with_capacity(frames.iter().map(Frame::encoded_len).sum());
        for frame in frames {
            frame.check()?;
            if !frame.schema.is_user() {
                return Err(Error::Reserved { id: frame.schema.0 });
            }
            if self.registry.get(frame.schema).is_none() {
                return Err(Error::Undeclared { id: frame.schema.0 });
            }
            frame.encode(&mut batch);
        }
        if batch.is_empty() {
            return Ok(self.head());
        }
        let done = self.write_batch(batch).await;
        self.poison(done)
    }

    async fn write_batch(&mut self, batch: Vec<u8>) -> Result<Cursor, Error> {
        self.ensure_decl().await?;
        self.ensure_active(batch.len() as u64).await?;
        let Some(writer) = self.active.as_mut() else {
            return Err(Error::corrupt(0, 0, "no writable segment"));
        };
        writer.write(batch).await?;
        let (segment, end) = (writer.segment(), writer.end());
        self.last_end = end;
        self.head = Cursor::new(segment, end);
        Ok(self.head)
    }

    /// The log's identity: 16 random bytes, created on first use.
    ///
    /// Stable for the log's lifetime and durable before it is returned: the
    /// declaration segment is synced, so a materialization that names this
    /// identity can never outlive it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] if the identity cannot be generated or written; a write
    /// failure poisons the journal.
    pub async fn identity(&mut self) -> Result<[u8; 16], Error> {
        self.live()?;
        if let Some(identity) = self.identity {
            return Ok(identity);
        }
        let mut identity = [0u8; 16];
        getrandom::fill(&mut identity).map_err(|e| Error::Io(std::io::Error::other(e)))?;
        let done = self.write_identity(identity).await;
        self.poison(done)?;
        self.identity = Some(identity);
        Ok(identity)
    }

    async fn write_identity(&mut self, identity: [u8; 16]) -> Result<(), Error> {
        self.ensure_decl().await?;
        let mut bytes = Vec::with_capacity(frame::HEADER_LEN + identity.len());
        Frame::new(SchemaId::IDENTITY, &identity).encode(&mut bytes);
        let Some(mut writer) = self.decl.take() else {
            return Err(Error::corrupt(segment::DECL, 0, "no declaration segment"));
        };
        writer.write(bytes).await?;
        self.decl_end = Some(writer.end());
        if self.last == Some(segment::DECL) {
            self.last_end = writer.end();
            self.head = Cursor::new(segment::DECL, self.last_end);
        }
        // The identity is only useful once it is durable: a cache may record
        // it immediately after this returns.
        writer.sync().await?;
        self.decl = Some(writer);
        Ok(())
    }

    /// Writes a declaration record to segment zero, ahead of any registry
    /// change, so that a log can never contain a record it does not explain.
    async fn declare(&mut self, record: &DeclRecord) -> Result<(), Error> {
        self.ensure_decl().await?;
        let payload = schema::encode_decl(record);
        let mut bytes = Vec::with_capacity(frame::HEADER_LEN + payload.len());
        Frame::new(SchemaId::DECL, &payload).encode(&mut bytes);
        let Some(mut writer) = self.decl.take() else {
            return Err(Error::corrupt(segment::DECL, 0, "no declaration segment"));
        };
        writer.write(bytes).await?;
        self.decl_end = Some(writer.end());
        if self.last == Some(segment::DECL) {
            self.last_end = writer.end();
            self.head = Cursor::new(segment::DECL, self.last_end);
        }
        self.decl = Some(writer);
        Ok(())
    }

    /// Opens segment zero for appending, creating it if this log has none yet.
    async fn ensure_decl(&mut self) -> Result<(), Error> {
        if self.decl.is_some() {
            return Ok(());
        }
        let Some(end) = self.decl_end else {
            let writer = segment::Writer::create(&self.dir, segment::DECL).await?;
            self.open_decl(writer);
            return Ok(());
        };
        let file = compio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.dir.join(segment::file_name(segment::DECL)))
            .await?;
        let walked = segment::Walked {
            end,
            count: 0,
            digest: blake3::Hasher::new(),
            broke: false,
        };
        let writer = segment::Writer::resume(file, segment::DECL, &walked);
        self.open_decl(writer);
        Ok(())
    }

    fn open_decl(&mut self, writer: segment::Writer) {
        self.decl_end = Some(writer.end());
        if self.last.is_none() {
            self.last = Some(segment::DECL);
            self.last_end = writer.end();
            self.head = Cursor::new(segment::DECL, self.last_end);
        }
        self.decl = Some(writer);
    }

    /// Makes sure the segment being written can take one more record, rotating
    /// (or starting the first data segment) if it cannot.
    async fn ensure_active(&mut self, len: u64) -> Result<(), Error> {
        if let Some(writer) = &self.active {
            let full = writer.end() > segment::HEADER_LEN as u64
                && writer.end().saturating_add(len) > self.options.rotate_bytes;
            if !full {
                return Ok(());
            }
        }
        // Declarations first, for the same reason as `close`: a sealed segment
        // must never name a declaration that is not durable. Closing before
        // creating the next segment is what lets a crash leave this one
        // unsealed beside a live successor; `open` recognises that as damage
        // only if this segment was acked.
        if let Some(writer) = &mut self.decl {
            writer.sync().await?;
        }
        let last = self.last.unwrap_or(segment::DECL);
        if let Some(mut writer) = self.active.take() {
            writer.close().await?;
        }
        self.start(last + 1).await
    }

    async fn start(&mut self, segment: u64) -> Result<(), Error> {
        let writer = segment::Writer::create(&self.dir, segment).await?;
        self.last = Some(segment);
        self.last_end = writer.end();
        self.head = Cursor::new(segment, self.last_end);
        self.active = Some(writer);
        Ok(())
    }
}
