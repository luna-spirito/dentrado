//! The records dentrado writes to the journal, and how replay regains their
//! meaning.
//!
//! The journal itself is domain-blind; everything it needs to know about a
//! `LocUserId`, a group or an event lives here, written as a record with a
//! shape. Schema ids are log-local and assigned on first use — see
//! [`schema::Schemas`].
//!
//! Replay ([`replay`]) is a pure journal concern: it verifies declarations,
//! decodes nested payloads, and hands typed records to a [`Sink`]. The page
//! cache implements the sink; this module never touches the cache.

pub mod record;
pub mod schema;
pub(crate) mod replay;
