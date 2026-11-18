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

use super::record::{Event, LocData, LocGroup, LocSender, LocUser, Record};
use crate::{core::api::RuntimeTypes, types::Shape};

/// The journal schemas one runtime writes: the record layouts and the nested
/// value layouts (`R::Group`, `R::Data`, `R::Body`).
///
/// Nested values all share one declaration name: the shape is the identity, so
/// two roles with the same layout (an `EmptyRuntime`, say) must resolve to the
/// same schema id rather than fight over a name.
pub(crate) struct Schemas {
    pub user: SchemaId,
    pub sender: SchemaId,
    pub group: SchemaId,
    pub data: SchemaId,
    pub event: SchemaId,
    pub group_value: SchemaId,
    pub data_value: SchemaId,
    pub body_value: SchemaId,
}

impl Schemas {
    /// Declares every layout on first use and returns their log-local ids.
    ///
    /// # Errors
    ///
    /// Whatever [`Journal::intern`] refuses.
    pub(crate) async fn declare<R: RuntimeTypes>(journal: &mut Journal) -> Result<Self, Error> {
        Ok(Self {
            user: declare::<LocUser>(journal).await?,
            sender: declare::<LocSender>(journal).await?,
            group: declare::<LocGroup>(journal).await?,
            data: declare::<LocData>(journal).await?,
            event: declare::<Event<'static>>(journal).await?,
            group_value: intern_value::<R::Group>(journal).await?,
            data_value: intern_value::<R::Data>(journal).await?,
            body_value: intern_value::<R::Body>(journal).await?,
        })
    }
}

/// The declaration name every nested value shape uses.
const VALUE_NAME: &str = "dentrado/value";

async fn intern_value<T: Shape>(journal: &mut Journal) -> Result<SchemaId, Error> {
    let shape = T::shape();
    journal
        .intern(Decl {
            name: VALUE_NAME,
            shape: &shape,
        })
        .await
}

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
