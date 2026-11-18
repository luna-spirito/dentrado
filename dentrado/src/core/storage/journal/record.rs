//! The records `dentrado` writes to the journal.
//!
//! Every layout is pinned by a golden test: a shape is a schema identity, so a
//! change to one of these strings is a new journal schema, not a refactor. The
//! declaration *name* is stable metadata; names may recur under a new shape
//! (a migration), shapes never do.
//!
//! Each record is self-contained: a loc record carries the raw identity bytes
//! it localizes, so replay never needs a lookup to interpret it. Nested
//! payloads (`group`, `content`, an event's body) travel as their own bytes
//! plus the id of the schema that describes them.

use crate::types::Shape;

/// A user identity, interned under a local id.
///
/// `identity_server_pk` is carried verbatim: identity stays concrete (the
/// journal never derives it), and a local id is only meaningful against the
/// log that assigned it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
pub struct LocUser {
    pub loc: u64,
    pub id: u64,
    pub identity_server_pk: [u8; 32],
}

impl LocUser {
    pub const NAME: &'static str = "loc-user";
}

/// A sender public key, optionally linked to a user.
///
/// The user record is written first so `user` can be resolved during replay
/// without ordering the fold against a future declaration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
pub struct LocSender {
    pub loc: u64,
    pub pk: [u8; 32],
    pub user: Option<u64>,
}

impl LocSender {
    pub const NAME: &'static str = "loc-sender";
}

/// A group key, interned under a local id.
///
/// `msg_type` is part of the identity (one group, one message kind);
/// `group_schema` names the layout of `group`.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
pub struct LocGroup {
    pub loc: u64,
    pub msg_type: u64,
    pub group_schema: u16,
    pub group: Vec<u8>,
}

impl LocGroup {
    pub const NAME: &'static str = "loc-group";
}

/// A content-addressed value, interned under a local id.
///
/// `hash` is the content address (verified before the record is written), and
/// `content_schema` names the layout of `content`. `timestamp` is the
/// cross-node ordering fact, kept with the bytes so replay can rebuild every
/// index without re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
pub struct LocData {
    pub loc: u64,
    pub timestamp: u32,
    pub hash: [u8; 32],
    pub content_schema: u16,
    pub content: Vec<u8>,
}

impl LocData {
    pub const NAME: &'static str = "loc-data";
}

/// One event in a group's shard.
///
/// The envelope is a fixed 28-byte prefix so a replay can read `group`,
/// `sender`, and the dedup keys and copy an event's body into the cache
/// without decoding it. A postcard struct would prepend a length to `body`,
/// which is exactly the byte the fold must not pay for.
///
/// The prefix is
/// `group u64 | sender u64 | tx_id u32 | timestamp u32 | source_node u32`,
/// and `body` is every remaining byte. The body's layout is the runtime's one
/// `R::Body` shape — pinned by the log's nested declarations, not repeated on
/// every record.
#[derive(Clone, Debug, PartialEq, Eq, Shape)]
pub struct Event<'a> {
    pub group: u64,
    pub sender: u64,
    pub tx_id: u32,
    pub timestamp: u32,
    pub source_node: u32,
    pub body: &'a [u8],
}

impl<'a> Event<'a> {
    pub const NAME: &'static str = "event";

    /// The fixed prefix, before the body.
    pub const HEADER_LEN: usize = 28;

    /// The record's bytes: the envelope followed by the body verbatim.
    ///
    /// # Panics
    ///
    /// Only if the allocator refuses the record; there is nothing to recover.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(Self::HEADER_LEN + self.body.len());
        out.extend_from_slice(&self.group.to_le_bytes());
        out.extend_from_slice(&self.sender.to_le_bytes());
        out.extend_from_slice(&self.tx_id.to_le_bytes());
        out.extend_from_slice(&self.timestamp.to_le_bytes());
        out.extend_from_slice(&self.source_node.to_le_bytes());
        out.extend_from_slice(self.body);
        out
    }

    /// Parse an event, borrowing its body — no copy, no decode.
    #[must_use]
    pub fn decode(bytes: &'a [u8]) -> Option<Self> {
        let header = bytes.get(..Self::HEADER_LEN)?;
        Some(Self {
            group: u64::from_le_bytes(header[0..8].try_into().ok()?),
            sender: u64::from_le_bytes(header[8..16].try_into().ok()?),
            tx_id: u32::from_le_bytes(header[16..20].try_into().ok()?),
            timestamp: u32::from_le_bytes(header[20..24].try_into().ok()?),
            source_node: u32::from_le_bytes(header[24..28].try_into().ok()?),
            body: &bytes[Self::HEADER_LEN..],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shapes are identities. Changing one is a migration, so the text is
    /// spelled out here rather than built.
    #[test]
    fn every_shape_is_pinned() {
        assert_eq!(
            LocUser::shape(),
            "LocUser{loc:u64,id:u64,identity_server_pk:[u8;32]}"
        );
        assert_eq!(
            LocSender::shape(),
            "LocSender{loc:u64,pk:[u8;32],user:Option<u64>}"
        );
        assert_eq!(
            LocGroup::shape(),
            "LocGroup{loc:u64,msg_type:u64,group_schema:u16,group:Vec<u8>}"
        );
        assert_eq!(
            LocData::shape(),
            "LocData{loc:u64,timestamp:u32,hash:[u8;32],content_schema:u16,content:Vec<u8>}"
        );
        assert_eq!(
            Event::shape(),
            "Event{group:u64,sender:u64,tx_id:u32,timestamp:u32,source_node:u32,body:Vec<u8>}"
        );
    }

    #[test]
    fn a_name_is_stable() {
        assert_eq!(LocUser::NAME, "loc-user");
        assert_eq!(LocSender::NAME, "loc-sender");
        assert_eq!(LocGroup::NAME, "loc-group");
        assert_eq!(LocData::NAME, "loc-data");
        assert_eq!(Event::NAME, "event");
    }

    #[test]
    fn an_event_round_trips_its_body_verbatim() {
        let event = Event {
            group: 7,
            sender: 11,
            tx_id: 3,
            timestamp: 1_700_000_000,
            source_node: 2,
            body: b"payload",
        };
        let bytes = event.encode();
        assert_eq!(bytes.len(), Event::HEADER_LEN + 7);
        assert_eq!(Event::decode(&bytes), Some(event));
    }

    #[test]
    fn a_short_event_is_refused() {
        assert_eq!(Event::decode(&[0u8; Event::HEADER_LEN - 1]), None);
        let empty = Event::decode(&[0u8; Event::HEADER_LEN]).expect("a bare header is an event");
        assert_eq!(empty.body, b"");
    }
}
