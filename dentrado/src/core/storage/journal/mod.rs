//! The journaled layers 1–2: the records `dentrado` declares and the schemas
//! they are interned under.
//!
//! The journal itself is domain-blind; everything it needs to know about a
//! `LocUserId`, a group or an event is here, written as a record with a shape.
//! Schema ids are log-local and assigned on first use — see
//! [`schema::declare`].
//!
//! The fold (replaying these records into the runtime's in-memory maps) lives
//! in this module too once it exists; the records and their codecs come first
//! so P2 can pin every layout before anything depends on it.

pub mod record;
pub mod schema;
