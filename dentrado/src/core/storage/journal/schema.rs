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

use super::record::{Event, LocData, LocGroup, LocSender, LocUser};
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
}

impl Schemas {
    /// Declares every layout on first use and returns their log-local ids.
    ///
    /// # Errors
    ///
    /// [`Error::ForeignValue`] if the log was written with a nested layout this
    /// runtime does not have, or whatever [`Journal::intern`] refuses.
    pub(crate) async fn declare<R: RuntimeTypes>(journal: &mut Journal) -> Result<Self, Error> {
        check_value_shapes::<R>(journal)?;
        let user = declare::<LocUser>(journal, LocUser::NAME).await?;
        let sender = declare::<LocSender>(journal, LocSender::NAME).await?;
        let group = declare::<LocGroup>(journal, LocGroup::NAME).await?;
        let data = declare::<LocData>(journal, LocData::NAME).await?;
        let event = declare::<Event<'static>>(journal, Event::NAME).await?;
        let group_value = intern_value::<R::Group>(journal).await?;
        let data_value = intern_value::<R::Data>(journal).await?;
        // No record names a body schema (an event carries raw bytes), but the
        // log still declares the layout: a build whose `R::Body` differs must
        // refuse the log at open rather than misdecode event bodies.
        intern_value::<R::Body>(journal).await?;
        Ok(Self {
            user,
            sender,
            group,
            data,
            event,
            group_value,
            data_value,
        })
    }
}

/// The declaration name every nested value shape uses.
const VALUE_NAME: &str = "value";

/// A log that already holds a nested layout this runtime does not have cannot
/// be interpreted: event records name no body schema of their own, so their
/// bytes would be decoded under the wrong shape.
fn check_value_shapes<R: RuntimeTypes>(journal: &Journal) -> Result<(), Error> {
    let known = [R::Group::shape(), R::Data::shape(), R::Body::shape()];
    for decl in journal.decls().iter() {
        if decl.name == VALUE_NAME && !known.contains(&decl.shape) {
            return Err(Error::ForeignValue {
                id: decl.id.0,
                name: decl.name.clone(),
                shape: decl.shape.clone(),
            });
        }
    }
    Ok(())
}

async fn intern_value<T: Shape>(journal: &mut Journal) -> Result<SchemaId, Error> {
    let shape = T::shape();
    journal
        .intern(Decl {
            name: VALUE_NAME,
            shape: &shape,
        })
        .await
}

/// Declares `shape` under `name` in `journal` on first use and returns its id.
///
/// The shape is computed once per call; the journal's registry does the
/// deduplication, so a second call is a lookup, not a second declaration.
///
/// # Errors
///
/// Whatever [`Journal::intern`] refuses — a shape conflict, the format's own
/// ids, or a full registry.
pub async fn declare<T: Shape>(journal: &mut Journal, name: &str) -> Result<SchemaId, Error> {
    let shape = T::shape();
    journal
        .intern(Decl {
            name,
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
