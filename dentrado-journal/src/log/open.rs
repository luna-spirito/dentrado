//! Opening: declarations, tail repair, and the refusal to guess.
//!
//! Recovery reads the files that are there and nothing else. Segment zero is
//! scanned whole for declarations; the last segment is walked frame by frame
//! and truncated at its first bad frame; every segment in between is only
//! checked for a header and a footer, because its frames are verified as they
//! are replayed anyway.

use std::path::Path;

use compio::fs::OpenOptions;

use super::{Journal, Options};
use crate::cursor::Cursor;
use crate::error::Error;
use crate::frame::Frame;
use crate::schema::{self, Registry, SchemaId};
use crate::segment::{self, Reader};

pub(super) async fn open(dir: impl AsRef<Path>, options: Options) -> Result<Journal, Error> {
    let dir = dir.as_ref().to_path_buf();
    std::fs::create_dir_all(&dir)?;
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
    };
    if journal.last.is_some() && ids.contains(&segment::DECL) {
        journal.load_decls().await?;
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
                if !fault.is_creation() || self.last != Some(segment::DECL) {
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
            if self.last != Some(segment::DECL) {
                return Err(Error::corrupt(segment::DECL, walked.end, "damaged frame"));
            }
            self.truncated += len - walked.end;
            file.set_len(walked.end).await?;
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
                if !fault.is_creation() {
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
        let walked = segment::walk(
            &mut reader,
            segment,
            segment::HEADER_LEN as u64,
            len,
            async |cursor, frame: Frame<'_>| {
                if frame.schema == SchemaId::DECL {
                    return Err(Error::corrupt(
                        segment,
                        cursor.offset,
                        "a declaration outside the declaration segment",
                    ));
                }
                if registry.get(frame.schema).is_none() {
                    return Err(Error::Undeclared { id: frame.schema.0 });
                }
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .await?;
        self.last_end = walked.end;
        let closed = walked.end != len
            && walked.end.saturating_add(segment::FOOTER_LEN as u64) == len
            && segment::decode_footer(reader.at(walked.end, segment::FOOTER_LEN).await?)
                .is_some_and(|footer| {
                    footer.count == walked.count
                        && footer.digest == *walked.digest.finalize().as_bytes()
                });
        if closed {
            return Ok(());
        }
        if walked.end < len {
            self.truncated += len - walked.end;
            file.set_len(walked.end).await?;
        }
        self.active = Some(segment::Writer::resume(file, segment, &walked));
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
