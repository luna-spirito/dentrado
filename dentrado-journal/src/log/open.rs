//! Opening: declarations, tail repair, and the refusal to guess.
//!
//! Recovery reads the files that are there and nothing else. Segment zero is
//! scanned whole for declarations; the last segment is walked frame by frame
//! and truncated at its first bad frame; every segment in between is only
//! checked for a header and a footer, because its frames are verified as they
//! are replayed anyway.
//!
//! The acked head decides what "its first bad frame" may be forgiven. Past it
//! the bytes were never acknowledged and any valid prefix is the truth; at or
//! below it the bytes were promised, so a frame that does not decode is damage
//! and the log refuses to open.

use std::ops::ControlFlow;
use std::path::Path;

use compio::fs::OpenOptions;

use super::{Journal, Options};
use crate::acked;
use crate::cursor::Cursor;
use crate::error::Error;
use crate::frame::Frame;
use crate::schema::{self, Registry, SchemaId};
use crate::segment::{self, Reader};

pub(super) async fn open(dir: impl AsRef<Path>, options: Options) -> Result<Journal, Error> {
    let dir = dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&dir)?;
    let lock = lock(&dir)?;
    let (recorder, acked) = acked::Recorder::open(&dir).await?;
    let ids = list(&dir)?;
    dense(&ids)?;

    let mut journal = Journal {
        dir,
        options,
        registry: Registry::new(),
        decl: None,
        active: None,
        last: ids.last().copied(),
        decl_end: None,
        last_end: 0,
        head: Cursor::START,
        truncated: 0,
        acked,
        recorder,
        broken: false,
        _lock: lock,
    };
    // An acked head names a segment that must still be there: the log promised
    // bytes this directory no longer holds.
    if journal.acked.is_some_and(|acked| acked.decl_end > 0) && !ids.contains(&segment::DECL) {
        return Err(Error::corrupt(
            segment::DECL,
            0,
            "the acked head names a segment that is gone",
        ));
    }
    if let Some(data) = journal.acked.and_then(|acked| acked.data)
        && journal.last.is_none_or(|last| data.segment > last)
    {
        return Err(Error::corrupt(
            data.segment,
            0,
            "the acked head names a segment that is gone",
        ));
    }
    if journal.last.is_some() && ids.contains(&segment::DECL) {
        journal.load_decls().await?;
    }
    for id in &ids {
        if *id != segment::DECL && Some(*id) != journal.last {
            journal.check_sealed(*id).await?;
        }
    }
    match journal.last {
        Some(segment::DECL) => journal.last_end = journal.decl_end.unwrap_or_default(),
        Some(last) => journal.load_tail(last).await?,
        None => {}
    }
    journal.head = match journal.last {
        Some(last) => Cursor::new(last, journal.last_end),
        None => Cursor::START,
    };
    Ok(journal)
}

/// Takes the directory's lock, so that a second writer is refused instead of
/// interleaving at the same offsets. Nothing is created for it.
fn lock(dir: &Path) -> Result<std::fs::File, Error> {
    let file = std::fs::File::open(dir)?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(Error::Locked),
        Err(std::fs::TryLockError::Error(e)) => Err(e.into()),
    }
}

impl Journal {
    /// Reads every declaration in segment zero, which is the only place they
    /// live.
    async fn load_decls(&mut self) -> Result<(), Error> {
        let path = self.dir.join(segment::file_name(segment::DECL));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await?;
        let len = file.metadata().await?.len();
        let mut reader = Reader::new(file.clone());
        let bytes = reader.at(0, segment::HEADER_LEN).await?;
        match segment::decode_header(bytes) {
            Ok(header) if header.segment == segment::DECL => {}
            Ok(_) => {
                return Err(Error::corrupt(
                    segment::DECL,
                    0,
                    "segment header names another segment",
                ));
            }
            Err(fault) => {
                let acked = self.acked.is_some_and(|acked| acked.decl_end > 0);
                if !Self::forgivable(acked, fault) || self.last != Some(segment::DECL) {
                    return Err(fault.error(segment::DECL));
                }
                self.truncated += len;
                let writer = segment::Writer::create(&self.dir, segment::DECL).await?;
                self.decl_end = Some(writer.end());
                self.decl = Some(writer);
                return Ok(());
            }
        }
        let registry = &mut self.registry;
        let walked = segment::walk(
            &mut reader,
            segment::DECL,
            segment::HEADER_LEN as u64,
            len,
            async |cursor, frame: Frame<'_>| {
                if frame.schema != SchemaId::DECL {
                    return Err(Error::corrupt(
                        segment::DECL,
                        cursor.offset,
                        "segment zero holds declarations only",
                    ));
                }
                registry.declare(schema::decode_decl(frame.payload)?)?;
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .await?;
        if walked.end != len {
            // A declaration past the acked end was never acknowledged, so the
            // tail may be cut. Anything at or below it was promised, and its
            // loss is damage — even when data segments exist, because then the
            // records that named it are acked too.
            if self.acked.is_some_and(|acked| acked.decl_end > walked.end) {
                return Err(Error::corrupt(
                    segment::DECL,
                    walked.end,
                    "damage below the acked head",
                ));
            }
            self.truncated += len - walked.end;
            file.set_len(walked.end).await?;
            self.decl_end = Some(walked.end);
            return Ok(());
        }
        self.decl_end = Some(walked.end);
        Ok(())
    }

    /// Scans the last segment, truncating a torn tail and keeping the digest of
    /// what survived so that the next close can finish the footer.
    async fn load_tail(&mut self, segment: u64) -> Result<(), Error> {
        let path = self.dir.join(segment::file_name(segment));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .await?;
        let len = file.metadata().await?.len();
        let mut reader = Reader::new(file.clone());
        let bytes = reader.at(0, segment::HEADER_LEN).await?;
        match segment::decode_header(bytes) {
            Ok(header) if header.segment == segment => {}
            Ok(_) => {
                return Err(Error::corrupt(
                    segment,
                    0,
                    "segment header names another segment",
                ));
            }
            Err(fault) => {
                // A segment header this build cannot read is a crash during
                // creation only if nothing in the segment was ever acked.
                let acked = self.acked.is_some_and(|acked| acked.reaches(segment));
                if !Self::forgivable(acked, fault) {
                    return Err(fault.error(segment));
                }
                self.truncated += len;
                let writer = segment::Writer::create(&self.dir, segment).await?;
                self.last_end = writer.end();
                self.active = Some(writer);
                return Ok(());
            }
        }
        let registry = &self.registry;
        let acked = self.acked.and_then(|acked| acked.offset_in(segment));
        let walked = segment::walk(
            &mut reader,
            segment,
            segment::HEADER_LEN as u64,
            len,
            async |cursor, frame: Frame<'_>| {
                if !frame.schema.is_user() {
                    return Err(Error::corrupt(
                        segment,
                        cursor.offset,
                        "a record with a schema that does not belong here",
                    ));
                }
                if registry.get(frame.schema).is_none() {
                    // Past the acked head a record may name a declaration that
                    // was torn away with it: nothing promised is lost by
                    // stopping here, and the tail is cut below.
                    if acked.is_some_and(|acked| cursor.offset >= acked) {
                        return Ok(ControlFlow::Break(()));
                    }
                    return Err(Error::Undeclared { id: frame.schema.0 });
                }
                Ok(ControlFlow::Continue(()))
            },
        )
        .await?;
        self.last_end = walked.end;
        // A footer at the end of the file is the seal: this segment is closed,
        // so nothing past the last good frame may be forgiven — a walk that
        // stopped early, or a footer that disagrees, is damage, not a tail
        // still being written. Only an unsealed file is a torn tail.
        let sealed = match len.checked_sub(segment::FOOTER_LEN as u64) {
            Some(at) => segment::decode_footer(reader.at(at, segment::FOOTER_LEN).await?)
                .map(|footer| (at, footer)),
            None => None,
        };
        if let Some((at, footer)) = sealed {
            if walked.end != at {
                return Err(Error::corrupt(
                    segment,
                    walked.end,
                    "damage inside a sealed segment",
                ));
            }
            if footer.count != walked.count || footer.digest != *walked.digest.finalize().as_bytes()
            {
                return Err(Error::corrupt(segment, at, "the footer disagrees"));
            }
            return Ok(());
        }
        // Unsealed: an unfinished append, so everything past the acked head may
        // go. Damage at or below it was promised to a writer that got a reply.
        if let Some(acked) = acked
            && walked.end < acked
        {
            return Err(Error::corrupt(
                segment,
                walked.end,
                "damage below the acked head",
            ));
        }
        if walked.end < len {
            self.truncated += len - walked.end;
            file.set_len(walked.end).await?;
        }
        self.active = Some(segment::Writer::resume(file, segment, &walked));
        Ok(())
    }

    /// Whether a segment header this build cannot read is explained by a crash
    /// while the segment was being created. Only when no byte of it was ever
    /// acked: otherwise the header is damage, and recreating the file would
    /// throw away records the log promised to keep.
    fn forgivable(acked: bool, fault: segment::HeaderFault) -> bool {
        !acked && fault.is_creation()
    }

    /// Checks that an interior segment is structurally sealed: its header names
    /// that segment, and a footer closes the file. Its frames are verified as
    /// they are replayed, never at open.
    async fn check_sealed(&self, segment: u64) -> Result<(), Error> {
        let path = self.dir.join(segment::file_name(segment));
        let file = OpenOptions::new().read(true).open(&path).await?;
        let len = file.metadata().await?.len();
        let mut reader = Reader::new(file);
        match segment::decode_header(reader.at(0, segment::HEADER_LEN).await?) {
            Ok(header) if header.segment == segment => {}
            Ok(_) => {
                return Err(Error::corrupt(
                    segment,
                    0,
                    "segment header names another segment",
                ));
            }
            Err(fault) => return Err(fault.error(segment)),
        }
        let Some(at) = len.checked_sub(segment::FOOTER_LEN as u64) else {
            return Err(Error::corrupt(
                segment,
                len,
                "a sealed segment is too short",
            ));
        };
        if segment::decode_footer(reader.at(at, segment::FOOTER_LEN).await?).is_none() {
            return Err(Error::corrupt(segment, at, "no footer"));
        }
        Ok(())
    }
}

fn list(dir: &Path) -> Result<Vec<u64>, Error> {
    let mut ids = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(segment::parse_file_name)
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

fn dense(ids: &[u64]) -> Result<(), Error> {
    if let Some((expected, _)) = ids
        .iter()
        .enumerate()
        .find(|(expected, id)| **id != *expected as u64)
    {
        return Err(Error::corrupt(
            expected as u64,
            0,
            "segment ids are not dense",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::dense;

    #[test]
    fn gap_check() {
        assert!(dense(&[]).is_ok());
        assert!(dense(&[0, 1, 2]).is_ok());
        assert!(dense(&[1]).is_err());
        assert!(dense(&[0, 2]).is_err());
        assert!(dense(&[0, 1, 1]).is_err());
    }
}
