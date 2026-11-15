//! Segments: one append-only file each, plus its header and footer codecs.
//!
//! ```text
//! header (32 B):  "DJNL" | version u16 | header_len u16 | segment u64 | [u8; 16]
//! frames:         see [`crate::frame`]
//! footer (44 B):  "DJNF" | count u64 | digest [u8; 32]
//! ```
//!
//! Segment zero holds declarations, never rotates and never gets a footer: it
//! is scanned whole when the log opens, which is what makes every other
//! segment declaration-free and cheap to skip. A footer means "closed" — the
//! only kind of segment nothing is ever appended to again.

use std::io;
use std::ops::ControlFlow;
use std::path::Path;

use compio::buf::BufResult;
use compio::fs::{File, OpenOptions};
use compio::io::{AsyncReadAt, AsyncWriteAtExt};

use crate::cursor::Cursor;
use crate::error::Error;
use crate::frame::{self, Frame};

/// The only on-disk format this build understands.
pub const LOG_VERSION: u16 = 1;
/// The segment header.
pub const HEADER_LEN: usize = 32;
/// The segment footer.
pub const FOOTER_LEN: usize = 44;
/// The declaration segment: the only one that never rotates.
pub const DECL: u64 = 0;
/// The first data segment.
pub const DATA_SEGMENT: u64 = 1;
/// The default rotation point.
pub const ROTATE_BYTES: u64 = 64 << 20;
/// Segment file suffix.
const SUFFIX: &str = ".jrnl";

/// How much a scan or a replay reads ahead.
const CHUNK: usize = 256 << 10;
const MAGIC: [u8; 4] = *b"DJNL";
const DONE: [u8; 4] = *b"DJNF";

/// The file name of a segment.
#[must_use]
pub fn file_name(segment: u64) -> String {
    format!("{segment:020}{SUFFIX}")
}

/// The segment id a file name stands for, if it is one of ours. Digits only,
/// so that one segment has exactly one name.
#[must_use]
pub fn parse_file_name(name: &str) -> Option<u64> {
    let digits = name.strip_suffix(SUFFIX)?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The id a segment header announces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The segment this file claims to be.
    pub segment: u64,
}

/// Why a segment does not start with a header this build wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HeaderFault {
    Short,
    Magic,
    Version(u16),
}

impl HeaderFault {
    /// Whether a crash during segment creation could have produced this, i.e.
    /// whether the file may be started over instead of being declared damaged.
    pub(crate) fn is_creation(self) -> bool {
        matches!(self, Self::Short | Self::Magic)
    }

    pub(crate) fn error(self, segment: u64) -> Error {
        match self {
            Self::Short => Error::corrupt(segment, 0, "truncated segment header"),
            Self::Magic => Error::corrupt(segment, 0, "not a journal segment"),
            Self::Version(found) => Error::Version { segment, found },
        }
    }
}

#[allow(clippy::cast_possible_truncation)] // HEADER_LEN is 32
fn encode_header(segment: u64) -> [u8; HEADER_LEN] {
    let mut bytes = [0u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(&MAGIC);
    bytes[4..6].copy_from_slice(&LOG_VERSION.to_le_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    bytes[8..16].copy_from_slice(&segment.to_le_bytes());
    bytes
}

pub(crate) fn decode_header(bytes: &[u8]) -> Result<Header, HeaderFault> {
    let raw = bytes.get(..HEADER_LEN).ok_or(HeaderFault::Short)?;
    if raw[0..4] != MAGIC {
        return Err(HeaderFault::Magic);
    }
    let version = u16::from_le_bytes(raw[4..6].try_into().expect("two bytes"));
    if version != LOG_VERSION {
        return Err(HeaderFault::Version(version));
    }
    let segment = u64::from_le_bytes(raw[8..16].try_into().expect("eight bytes"));
    Ok(Header { segment })
}

/// What a closed segment vouches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Footer {
    /// How many frames the segment holds.
    pub count: u64,
    /// Blake3 over every frame byte, header aside.
    pub digest: [u8; 32],
}

fn encode_footer(footer: Footer) -> [u8; FOOTER_LEN] {
    let mut bytes = [0u8; FOOTER_LEN];
    bytes[0..4].copy_from_slice(&DONE);
    bytes[4..12].copy_from_slice(&footer.count.to_le_bytes());
    bytes[12..44].copy_from_slice(&footer.digest);
    bytes
}

/// `None` if `bytes` is not exactly a footer.
#[must_use]
pub fn decode_footer(bytes: &[u8]) -> Option<Footer> {
    if bytes.len() != FOOTER_LEN || bytes[0..4] != DONE {
        return None;
    }
    Some(Footer {
        count: u64::from_le_bytes(bytes[4..12].try_into().ok()?),
        digest: bytes[12..44].try_into().ok()?,
    })
}

/// An open segment being appended to.
#[derive(Debug)]
pub struct Writer {
    file: File,
    segment: u64,
    end: u64,
    count: u64,
    digest: blake3::Hasher,
    dirty: bool,
}

impl Writer {
    /// Starts a segment over, header included, and makes its directory entry
    /// durable before returning.
    pub(crate) async fn create(dir: &Path, segment: u64) -> Result<Self, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.join(file_name(segment)))
            .await?;
        let mut writer = Self {
            file,
            segment,
            end: 0,
            count: 0,
            digest: blake3::Hasher::new(),
            dirty: true,
        };
        writer.put(encode_header(segment).to_vec()).await?;
        sync_dir(dir)?;
        Ok(writer)
    }

    /// Continues a segment whose frames were already scanned and whose tail was
    /// truncated to `end`.
    pub(crate) fn resume(file: File, segment: u64, walked: &Walked) -> Self {
        Self {
            file,
            segment,
            end: walked.end,
            count: walked.count,
            digest: walked.digest.clone(),
            dirty: false,
        }
    }

    /// The segment id.
    #[must_use]
    pub fn segment(&self) -> u64 {
        self.segment
    }

    /// The offset the next byte is written to.
    #[must_use]
    pub fn end(&self) -> u64 {
        self.end
    }

    /// Appends one whole frame.
    ///
    /// The footer state is advanced only once the bytes are in the file, so a
    /// failed write leaves a writer that still describes what is on disk — and
    /// a close after it would seal the truth rather than a lie.
    pub(crate) async fn write(&mut self, bytes: Vec<u8>) -> Result<(), Error> {
        let mut digest = self.digest.clone();
        digest.update(&bytes);
        self.put(bytes).await?;
        self.digest = digest;
        self.count += 1;
        Ok(())
    }

    /// Makes everything written so far durable, unless it already is.
    pub(crate) async fn sync(&mut self) -> Result<(), Error> {
        if self.dirty {
            self.file.sync_data().await?;
            self.dirty = false;
        }
        Ok(())
    }

    /// Writes the footer (except for the declaration segment) and syncs.
    pub(crate) async fn close(&mut self) -> Result<(), Error> {
        if self.segment != DECL {
            let footer = Footer {
                count: self.count,
                digest: *self.digest.finalize().as_bytes(),
            };
            self.put(encode_footer(footer).to_vec()).await?;
        }
        self.file.sync_all().await?;
        self.dirty = false;
        Ok(())
    }

    async fn put(&mut self, bytes: Vec<u8>) -> Result<(), Error> {
        let at = self.end;
        let len = bytes.len() as u64;
        let BufResult(result, _) = self.file.write_all_at(bytes, at).await;
        result?;
        self.end += len;
        self.dirty = true;
        Ok(())
    }
}

/// Makes a directory entry durable. Called once per file created; a log
/// without the fsync would forget the file's very existence after a crash.
pub(crate) fn sync_dir(dir: &Path) -> io::Result<()> {
    std::fs::File::open(dir)?.sync_all()
}

/// A read window over one segment: a chunk at a time, no per-record read.
#[derive(Debug)]
pub(crate) struct Reader {
    file: File,
    buf: Vec<u8>,
    start: u64,
}

impl Reader {
    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            buf: Vec::new(),
            start: 0,
        }
    }

    /// Up to `len` bytes starting at `offset`, short only at the end of the
    /// file. The slice is valid until the next call.
    pub(crate) async fn at(&mut self, offset: u64, len: usize) -> Result<&[u8], Error> {
        let window = self.start..self.start + self.buf.len() as u64;
        if !window.contains(&offset) || offset.saturating_add(len as u64) > window.end {
            self.buf = read(&self.file, offset, len.max(CHUNK)).await?;
            self.start = offset;
        }
        let begin = usize::try_from(offset - self.start).unwrap_or(usize::MAX);
        let end = begin.saturating_add(len).min(self.buf.len());
        Ok(self.buf.get(begin..end).unwrap_or_default())
    }
}

async fn read(file: &File, offset: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(len);
    let mut at = offset;
    while buf.len() < len {
        let BufResult(result, filled) = file.read_at(buf, at).await;
        buf = filled;
        let read = result?;
        if read == 0 {
            break;
        }
        at += read as u64;
    }
    Ok(buf)
}

/// What a walk consumed.
#[derive(Debug)]
pub(crate) struct Walked {
    /// Just past the last frame that parsed.
    pub end: u64,
    /// How many frames that was.
    pub count: u64,
    /// Blake3 over their bytes, for the footer.
    pub digest: blake3::Hasher,
    /// Whether the visitor asked to stop.
    pub broke: bool,
}

/// Walks the well-formed frames in `[start, end)`, stopping at the first one
/// that is not — a footer, a torn tail, or damage the caller must judge.
///
/// The visitor is async and gets a frame borrowing the read window, so it may
/// await on it (a cache write, say) as long as it does not try to read the same
/// segment meanwhile. Nothing is copied per record.
///
/// [`ControlFlow::Break`] means "stop, and do not count this record": the
/// returned [`Walked`] describes the prefix *before* it. A caller that
/// consumed the record anyway has to add its length back.
pub(crate) async fn walk(
    reader: &mut Reader,
    segment: u64,
    start: u64,
    end: u64,
    mut visit: impl AsyncFnMut(Cursor, Frame<'_>) -> Result<ControlFlow<()>, Error>,
) -> Result<Walked, Error> {
    let mut walked = Walked {
        end: start,
        count: 0,
        digest: blake3::Hasher::new(),
        broke: false,
    };
    while walked.end < end {
        let at = walked.end;
        let Some(payload) = frame::payload_len(reader.at(at, frame::HEADER_LEN).await?) else {
            break;
        };
        if payload > frame::MAX_PAYLOAD {
            break;
        }
        let total = frame::HEADER_LEN + payload;
        if at.saturating_add(total as u64) > end {
            break;
        }
        let raw = reader.at(at, total).await?;
        let Some((frame, _)) = frame::decode(raw) else {
            break;
        };
        if visit(Cursor::new(segment, at), frame).await?.is_break() {
            walked.broke = true;
            break;
        }
        walked.digest.update(raw);
        walked.count += 1;
        walked.end = at + total as u64;
    }
    Ok(walked)
}

#[cfg(test)]
mod tests {
    use super::{
        Footer, HEADER_LEN, HeaderFault, decode_footer, decode_header, encode_footer,
        encode_header, file_name, parse_file_name,
    };

    #[test]
    fn segment_names_round_trip() {
        for segment in [0, 1, 7, u64::MAX] {
            assert_eq!(parse_file_name(&file_name(segment)), Some(segment));
        }
        assert_eq!(parse_file_name("core-3.dstore"), None);
        assert_eq!(parse_file_name("00000000000000000001.tmp"), None);
    }

    #[test]
    fn headers_round_trip() {
        let header = decode_header(&encode_header(42)).unwrap();
        assert_eq!(header.segment, 42);
    }

    #[test]
    fn a_foreign_file_is_not_a_header() {
        assert_eq!(decode_header(b""), Err(HeaderFault::Short));
        assert_eq!(decode_header(&[0u8; HEADER_LEN]), Err(HeaderFault::Magic));
        assert!(HeaderFault::Short.is_creation());
    }

    #[test]
    fn a_newer_log_version_is_refused() {
        let mut bytes = encode_header(0);
        bytes[4..6].copy_from_slice(&2u16.to_le_bytes());
        assert_eq!(decode_header(&bytes), Err(HeaderFault::Version(2)));
        assert!(!HeaderFault::Version(2).is_creation());
    }

    #[test]
    fn footers_round_trip() {
        let footer = Footer {
            count: 9,
            digest: [7; 32],
        };
        assert_eq!(decode_footer(&encode_footer(footer)), Some(footer));
        assert_eq!(decode_footer(b"DJNF"), None);
        assert_eq!(decode_footer(&[0u8; 44]), None);
    }
}
