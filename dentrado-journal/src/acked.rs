//! The acked head: how far the log was acknowledged, written down outside it.
//!
//! From inside a segment a torn append and a rotted one look identical — a
//! valid prefix followed by junk — so the log alone cannot tell "the writer was
//! interrupted here" from "bytes that were acknowledged are gone". It need not
//! guess: [`Journal::sync`] writes down where it got to, in a file of its own,
//! *after* the segments themselves are durable. Opening the log then knows
//! which of the two it is looking at: damage below the acked head is rot and is
//! loud, damage past it is an unfinished append and is truncated away.
//!
//! The file is two slots written in alternation, each with a sequence number
//! and a crc. A torn write of this file therefore costs the newest slot, not
//! the record: the other slot still names an earlier acked head, which remains
//! true because an ack only moves forward. Losing the file altogether is also
//! safe — the log falls back to reading its tail as an interrupted append,
//! exactly as it did before this file existed.

use std::io;
use std::path::{Path, PathBuf};

use compio::buf::BufResult;
use compio::fs::{File, OpenOptions};
use compio::io::{AsyncReadAt, AsyncWriteAtExt};

use crate::cursor::Cursor;
use crate::error::Error;
use crate::segment;

/// The file that remembers the acked head. Not a segment: it has no frames,
/// is never replayed, and may be deleted at the cost of the extra strictness.
const FILE: &str = "acked";

/// One slot: `seq u64 | decl_end u64 | segment u64 | offset u64 | crc32 u32 |
/// padding`.
const SLOT: usize = 40;

/// How many slots are written in alternation.
const SLOTS: u64 = 2;

/// No data segment has been acked yet.
const NONE: u64 = u64::MAX;

/// How far the log has been acknowledged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Acked {
    /// The durable end of segment zero: the declarations up to here survive.
    pub decl_end: u64,
    /// The durable data position, `None` until a data segment exists.
    pub data: Option<Cursor>,
}

impl Acked {
    /// Whether `segment` may hold acked records.
    #[must_use]
    pub fn reaches(self, segment: u64) -> bool {
        self.data.is_some_and(|data| data.segment >= segment)
    }

    /// The acked offset in `segment`, if the acked head is in it.
    #[must_use]
    pub fn offset_in(self, segment: u64) -> Option<u64> {
        self.data
            .filter(|data| data.segment == segment)
            .map(|data| data.offset)
    }

    /// Whether anything at all has been acked.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.decl_end == 0 && self.data.is_none()
    }
}

/// The acked head's file and the slot to write next.
#[derive(Debug)]
pub(crate) struct Recorder {
    path: PathBuf,
    file: Option<File>,
    seq: u64,
}

impl Recorder {
    /// Reads the newest valid slot, creating nothing: a log that was never
    /// synced has no such file, and opening it must not make one.
    pub(crate) async fn open(dir: &Path) -> Result<(Self, Option<Acked>), Error> {
        let mut recorder = Self {
            path: dir.join(FILE),
            file: None,
            seq: 0,
        };
        let file = match File::open(&recorder.path).await {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok((recorder, None)),
            Err(e) => return Err(e.into()),
        };
        let bytes = read(&file).await?;
        let newest = bytes
            .as_chunks::<SLOT>()
            .0
            .iter()
            .filter_map(|slot| decode(slot))
            .fold(None, |newest: Option<(u64, Acked)>, slot| match newest {
                Some((seq, _)) if seq >= slot.0 => newest,
                _ => Some(slot),
            });
        recorder.seq = newest.map_or(0, |(seq, _)| seq);
        recorder.file = Some(file);
        Ok((recorder, newest.map(|(_, acked)| acked)))
    }

    /// Records `acked`, filling the slot the previous record did not use.
    pub(crate) async fn save(&mut self, acked: Acked) -> Result<(), Error> {
        if self.file.is_none() {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(&self.path)
                .await?;
            segment::sync_dir(self.path.parent().expect("a path below a directory"))?;
            self.file = Some(file);
        }
        let seq = self.seq + 1;
        let at = (seq % SLOTS) * SLOT as u64;
        let file = self.file.as_mut().expect("opened above");
        let BufResult(result, _) = file.write_all_at(encode(seq, acked).to_vec(), at).await;
        result?;
        file.sync_data().await?;
        self.seq = seq;
        Ok(())
    }
}

/// Both slots, zero-filled past the end of the file.
async fn read(file: &File) -> Result<[u8; SLOT * 2], Error> {
    let BufResult(result, bytes) = file.read_at([0u8; SLOT * 2], 0).await;
    result?;
    Ok(bytes)
}

fn encode(seq: u64, acked: Acked) -> [u8; SLOT] {
    let (segment, offset) = match acked.data {
        Some(data) => (data.segment, data.offset),
        None => (NONE, 0),
    };
    let mut bytes = [0u8; SLOT];
    bytes[0..8].copy_from_slice(&seq.to_le_bytes());
    bytes[8..16].copy_from_slice(&acked.decl_end.to_le_bytes());
    bytes[16..24].copy_from_slice(&segment.to_le_bytes());
    bytes[24..32].copy_from_slice(&offset.to_le_bytes());
    let crc = crc32fast::hash(&bytes[..32]);
    bytes[32..36].copy_from_slice(&crc.to_le_bytes());
    bytes
}

fn decode(bytes: &[u8]) -> Option<(u64, Acked)> {
    let field = |range: std::ops::Range<usize>| -> Option<u64> {
        Some(u64::from_le_bytes(bytes.get(range)?.try_into().ok()?))
    };
    let seq = field(0..8)?;
    let decl_end = field(8..16)?;
    let segment = field(16..24)?;
    let offset = field(24..32)?;
    let crc = u32::from_le_bytes(bytes.get(32..36)?.try_into().ok()?);
    if crc32fast::hash(bytes.get(..32)?) != crc {
        return None;
    }
    let data = (segment != NONE).then(|| Cursor::new(segment, offset));
    Some((seq, Acked { decl_end, data }))
}

#[cfg(test)]
mod tests {
    use super::{Acked, NONE, SLOT, decode, encode};
    use crate::cursor::Cursor;

    fn acked() -> Acked {
        Acked {
            decl_end: 96,
            data: Some(Cursor::new(7, 1024)),
        }
    }

    #[test]
    fn slots_round_trip() {
        let bytes = encode(3, acked());
        assert_eq!(decode(&bytes), Some((3, acked())));
    }

    #[test]
    fn no_data_survives_the_round_trip() {
        let acked = Acked {
            decl_end: 64,
            data: None,
        };
        assert_eq!(decode(&encode(1, acked)), Some((1, acked)));
    }

    /// Everything up to and including the crc carries meaning; the padding
    /// after it does not, and a flip there is not damage.
    const MEANING: usize = 36;

    #[test]
    fn a_torn_slot_is_refused() {
        let bytes = encode(3, acked());
        for cut in 0..MEANING {
            assert_eq!(decode(&bytes[..cut]), None, "cut at {cut}");
        }
        assert_eq!(decode(&bytes), Some((3, acked())), "a whole slot decodes");
        for bit in 0..MEANING * 8 {
            let mut victim = bytes;
            victim[bit / 8] ^= 1 << (bit % 8);
            assert_eq!(decode(&victim), None, "bit {bit}");
        }
        let mut padded = bytes;
        padded[SLOT - 1] ^= 0xff;
        assert_eq!(decode(&padded), Some((3, acked())), "padding is ignored");
    }

    #[test]
    fn an_unused_slot_names_nothing() {
        let bytes = encode(
            1,
            Acked {
                decl_end: 0,
                data: None,
            },
        );
        assert_eq!(u64::from_le_bytes(bytes[16..24].try_into().unwrap()), NONE);
        assert!(decode(&bytes).unwrap().1.is_empty());
    }
}
