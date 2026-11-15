//! One record's bytes on the wire.
//!
//! ```text
//! payload_len u32 | crc32 u32 | schema_id u16 | payload
//! ```
//!
//! The header is a fixed ten bytes, so finding the next frame never runs a
//! decoder. `crc32` (IEEE) covers `schema_id | payload`.

use crate::error::Error;
use crate::schema::SchemaId;

/// The fixed frame header.
pub const HEADER_LEN: usize = 10;

/// The largest payload one record may carry.
pub const MAX_PAYLOAD: usize = 16 << 20;

/// A record: what was written, and what a replay yields.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Frame<'a> {
    /// What the payload means.
    pub schema: SchemaId,
    /// The bytes themselves; the journal never looks inside.
    pub payload: &'a [u8],
}

impl<'a> Frame<'a> {
    /// A record of `payload`, in the layout of `schema`.
    #[must_use]
    pub const fn new(schema: SchemaId, payload: &'a [u8]) -> Self {
        Self { schema, payload }
    }

    /// How many bytes this frame takes on disk.
    #[must_use]
    pub const fn encoded_len(&self) -> usize {
        HEADER_LEN + self.payload.len()
    }

    /// Refuses a record the format cannot carry.
    ///
    /// # Errors
    ///
    /// [`Error::TooLarge`] past [`MAX_PAYLOAD`].
    pub fn check(&self) -> Result<(), Error> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(Error::TooLarge {
                what: "a payload",
                len: self.payload.len(),
                max: MAX_PAYLOAD,
            });
        }
        Ok(())
    }

    /// Appends the frame to `out`.
    ///
    /// # Panics
    ///
    /// If [`Frame::check`] has not passed — the length no longer fits a `u32`.
    pub fn encode(&self, out: &mut Vec<u8>) {
        let len = u32::try_from(self.payload.len()).expect("checked against MAX_PAYLOAD");
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&crc(self.schema, self.payload).to_le_bytes());
        out.extend_from_slice(&self.schema.0.to_le_bytes());
        out.extend_from_slice(self.payload);
    }
}

/// The payload length a frame header announces, if `bytes` holds one.
#[must_use]
pub fn payload_len(bytes: &[u8]) -> Option<usize> {
    let header = bytes.get(..HEADER_LEN)?;
    Some(u32::from_le_bytes(header[0..4].try_into().ok()?) as usize)
}

/// The frame `bytes` starts with, and how long it is.
///
/// `None` if `bytes` does not begin with one whole, valid frame — a short read,
/// an implausible length, or a CRC that does not match.
#[must_use]
pub fn decode(bytes: &[u8]) -> Option<(Frame<'_>, usize)> {
    let payload_len = payload_len(bytes)?;
    if payload_len > MAX_PAYLOAD {
        return None;
    }
    let total = payload_len.checked_add(HEADER_LEN)?;
    let raw = bytes.get(..total)?;
    let crc = u32::from_le_bytes(raw[4..8].try_into().ok()?);
    // `raw[8..]` is exactly `schema_id | payload`.
    if crc != crc32fast::hash(&raw[8..]) {
        return None;
    }
    let schema = SchemaId(u16::from_le_bytes(raw[8..10].try_into().ok()?));
    Some((Frame::new(schema, &raw[HEADER_LEN..]), total))
}

fn crc(schema: SchemaId, payload: &[u8]) -> u32 {
    let mut hasher = crc32fast::Hasher::new();
    hasher.update(&schema.0.to_le_bytes());
    hasher.update(payload);
    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::{Frame, HEADER_LEN, MAX_PAYLOAD, decode, payload_len};
    use crate::schema::SchemaId;

    #[test]
    fn round_trips() {
        let frame = Frame::new(SchemaId(16), b"hello");
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        assert_eq!(bytes.len(), frame.encoded_len());
        assert_eq!(payload_len(&bytes), Some(5));
        assert_eq!(decode(&bytes), Some((frame, HEADER_LEN + 5)));
    }

    #[test]
    fn an_empty_payload_is_a_frame() {
        let frame = Frame::new(SchemaId(1), b"");
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        assert_eq!(bytes.len(), HEADER_LEN);
        assert_eq!(decode(&bytes), Some((frame, HEADER_LEN)));
    }

    #[test]
    fn any_single_bit_flip_is_refused() {
        let frame = Frame::new(SchemaId(16), b"payload");
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        for bit in 0..bytes.len() * 8 {
            let mut torn = bytes.clone();
            torn[bit / 8] ^= 1 << (bit % 8);
            assert!(decode(&torn).is_none(), "bit {bit} survived");
        }
    }

    #[test]
    fn a_short_frame_is_incomplete() {
        let frame = Frame::new(SchemaId(16), b"payload");
        let mut bytes = Vec::new();
        frame.encode(&mut bytes);
        for cut in 0..bytes.len() {
            assert!(decode(&bytes[..cut]).is_none(), "{cut} bytes decoded");
        }
    }

    #[test]
    fn an_implausible_length_is_refused() {
        let mut bytes = Vec::new();
        Frame::new(SchemaId(16), b"x").encode(&mut bytes);
        bytes[0..4].copy_from_slice(&u32::try_from(MAX_PAYLOAD + 1).unwrap().to_le_bytes());
        assert!(decode(&bytes).is_none());
    }
}
