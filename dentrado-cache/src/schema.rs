//! The schemas: how a table's records name their dependent pages. The
//! store never decodes a record's bytes — but it must be able to
//! compute a record's *closure* (the pages that die with the table),
//! so each table declares how its inline bytes point at pages, and
//! each pointed page declares the same. A schema is a pair of plain
//! functions — the store touches a page's bytes only through them,
//! never with its own codecs.
//!
//! Phase A has no production writer of dependent pages (records are
//! flat values); the schemas exist so `drop_table` and the partition
//! invariants walk closures honestly, and Phase B's page-valued
//! records plug in without another store surgery.

use crate::error::StoreError;
use crate::page::PageId;

/// A page a record names, with the schema of its own bytes — the next
/// hop of the closure walk.
pub type Dep = (PageId, PageSchema);

/// A page's schema: given the page's bytes (the whole page — the
/// layout is the owner's), the pages it directly names.
#[derive(Clone, Copy)]
pub struct PageSchema {
    pub deps: fn(&[u8]) -> Result<Vec<Dep>, StoreError>,
}

/// A table's schema: given a record's inline bytes, the pages it
/// directly names.
#[derive(Clone, Copy)]
pub struct TableSchema {
    pub val_deps: fn(&[u8]) -> Result<Vec<Dep>, StoreError>,
}

/// Names no pages — the schema-less table's and the fallback's half.
fn no_deps(_: &[u8]) -> Result<Vec<Dep>, StoreError> {
    Ok(Vec::new())
}

/// The schema-less schema: records that name no pages — and a table
/// must then take none (`put`'s `take` lists stay empty, or the
/// partition invariant reports its orphans).
pub const NO_DEPS: TableSchema = TableSchema { val_deps: no_deps };

/// The schema-less page half.
pub const NO_DEPS_PAGE: PageSchema = PageSchema { deps: no_deps };

/// A table's identity at `open`: opaque bytes (the caller's postcard
/// of a name and version — the store only compares them) plus the
/// schema its records obey.
#[derive(Clone)]
pub struct TableSpec {
    pub id: Vec<u8>,
    pub schema: TableSchema,
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
