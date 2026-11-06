//! The schemas: how a table's records name their dependent pages. The
//! store never decodes a record's bytes — but it must be able to
//! compute a record's *closure* (the pages that die with it), so each
//! table declares how its inline bytes point at pages, and each
//! pointed page declares the same. Both are dyn-safe ZST traits: a
//! schema is a handful of functions, never data.
//!
//! Phase A has no production writer of dependent pages (records are
//! flat values); the schemas exist so `drop_table` walks closures
//! honestly and Phase B's page-valued records plug in without another
//! store surgery.

use crate::error::StoreError;
use crate::page::PageId;

/// A table's schema: given a record's inline bytes, the pages it
/// directly names (each with its own schema for the next hop).
pub trait TableSchema {
    fn val_deps(&self, val: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError>;
}

/// A dependent page's schema: given the page's payload bytes, the
/// pages it directly names.
pub trait PageSchema {
    fn deps(&self, page: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError>;
}

/// The schema-less schema: bytes that name no pages — the record
/// surface's own values (inline or auto-blobbed chains, whose bulk
/// pages belong to the value chain itself, not to any schema).
pub struct NoDeps;

impl TableSchema for NoDeps {
    fn val_deps(&self, _val: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError> {
        Ok(Vec::new())
    }
}

impl PageSchema for NoDeps {
    fn deps(&self, _page: &[u8]) -> Result<Vec<(PageId, &'static dyn PageSchema)>, StoreError> {
        Ok(Vec::new())
    }
}

pub const NO_DEPS: &dyn TableSchema = &NoDeps;
pub const NO_DEPS_PAGE: &dyn PageSchema = &NoDeps;

/// A table's identity at `open`: opaque bytes (the caller's postcard
/// of a name and version — the store only compares them) plus the
/// schema its records obey.
#[derive(Clone)]
pub struct TableSpec {
    pub id: Vec<u8>,
    pub schema: &'static dyn TableSchema,
}

impl TableSpec {
    /// A schema-less table over any identity bytes.
    pub fn flat(id: Vec<u8>) -> Self {
        Self {
            id,
            schema: NO_DEPS,
        }
    }
}
