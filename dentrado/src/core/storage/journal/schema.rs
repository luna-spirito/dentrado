//! Interning record shapes and encoding their bytes.
//!
//! A schema id is log-local: it is assigned the first time a shape appears in
//! *this* log and never reused. Two processes opening the same log agree
//! because both read the declarations from segment zero; two different logs
//! may hand the same shape different ids, which is why cached materializations
//! are keyed by the registry fingerprint rather than by raw ids.

use dentrado_journal::{Decl, Error, Journal, SchemaId};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::record::Record;

/// Declares `R`'s layout in `journal` on first use and returns its id.
///
/// The shape is computed once per call; the journal's registry does the
/// deduplication, so a second call is a lookup, not a second declaration.
///
/// # Errors
///
/// Whatever [`Journal::intern`] refuses — a shape conflict, the format's own
/// ids, or a full registry.
pub async fn declare<R: Record>(journal: &mut Journal) -> Result<SchemaId, Error> {
    let shape = R::shape();
    journal
        .intern(Decl {
            name: R::NAME,
            shape: &shape,
        })
        .await
}

/// A record's postcard bytes — the body of its journal frame.
///
/// # Errors
///
/// [`postcard::Error`] if a `Serialize` impl refuses; derived impls do not.
pub fn encode<R: Serialize>(record: &R) -> Result<Vec<u8>, postcard::Error> {
    postcard::to_stdvec(record)
}

/// The inverse of [`encode`].
///
/// # Errors
///
/// [`postcard::Error`] for a truncated or malformed payload, or bytes that do
/// not decode into `R`.
pub fn decode<R: DeserializeOwned>(bytes: &[u8]) -> Result<R, postcard::Error> {
    postcard::from_bytes(bytes)
}
