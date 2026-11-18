//! The records dentrado writes to the journal.
//!
//! The journal itself is domain-blind; everything it needs to know about a
//! `LocUserId`, a group or an event lives here, written as a record with a
//! shape. Schema ids are log-local and assigned on first use — see
//! `schema::Schemas`.
//!
//! Replay lives with the cache (`cache::replay`): it
//! drives the same layer 1–2 transitions the live path does, so it belongs
//! beside them rather than behind a callback trait here.

pub mod record;
pub mod schema;
