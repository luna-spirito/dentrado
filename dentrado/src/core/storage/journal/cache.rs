//! What the journaled backend materializes into the page store.
//!
//! Two tables, both flat byte stores:
//!
//! - `dentrado/meta` holds one record: the journal identity, the cursor the
//!   bodies are materialized through, the registry fingerprint, and the fold
//!   format. It is deliberately *not* a superblock field: the cache is
//!   domain-blind, and a table is its sanctioned extension point.
//! - `dentrado/events/<fingerprint>` holds event bodies, keyed
//!   `group_be8 || slot_be8` and valued `body_schema_be2 || body`.
//!
//! The fingerprint is `blake3` over the log's `(id, shape_hash)` declarations.
//! A registry change therefore names a different table: the old one is an
//! orphan and the startup GC drops it, so a body is never decoded under local
//! ids it was not written with.
//!
//! Bodies and the meta cursor are committed with the same store session at
//! startup, so "applied = C" always implies "the bodies through C exist". A
//! live process commits each body as it is observed and advances the cursor
//! only after the journal record is durable, so the cursor can lag the
//! journal (a crash before `sync_log`), never lead it. A body written for an
//! event whose append did not survive is unreferenced: the fold has no slot
//! for it.

use dentrado_cache::{Session, Storage, StoreError, TableSpec, disk::FsDisk};
use dentrado_journal::{Cursor, Registry, SchemaId};
use serde::{Deserialize, Serialize};

use crate::core::storage::{
    store::Layout,
    view::{gear_specs, gear_tables, legacy_gear_table},
};
use crate::{
    core::api::{GearTable, RuntimeTypes},
    types::{GroupEventId, LocGroupId},
};

/// The namespace every table id in this module starts with.
const NAMESPACE: &str = "dentrado"; // REVIEW: This is dentrado-cache, all the tables it contains are dentrado-specific. No need for the "namespace"

/// The layout version of [`Meta`].
pub(crate) const META_VERSION: u32 = 0;

/// The layout version of the fold this cache is built against.
pub(crate) const FOLD_VERSION: u32 = 0;

/// The single key in the meta table.
pub(crate) const META_KEY: &[u8] = b"meta";

/// A table this backend owns. Encoded as `postcard((NAMESPACE, table))`, so a
/// store shared with another subsystem leaves foreign ids alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Table {
    Meta(u32),
    Events([u8; 32]),
}

/// The meta record: what the cache has applied, and to which log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Meta {
    pub log: [u8; 16],
    /// `(segment, offset)` of the first record *not* materialized.
    pub applied: (u64, u64),
    pub fingerprint: [u8; 32],
    pub fold: u32,
}

impl Meta {
    #[must_use]
    pub(crate) fn applied(&self) -> Cursor {
        Cursor::new(self.applied.0, self.applied.1)
    }
}

/// This backend's current table ids.
#[derive(Debug, Clone)]
pub(crate) struct Tables {
    pub meta: Vec<u8>,
    pub events: Vec<u8>,
}

impl Tables {
    #[must_use]
    pub(crate) fn new(fingerprint: [u8; 32]) -> Self {
        Self {
            meta: encode(Table::Meta(META_VERSION)),
            events: encode(Table::Events(fingerprint)),
        }
    }

    #[must_use]
    pub(crate) fn specs(&self) -> Vec<TableSpec> {
        vec![
            TableSpec::flat(self.meta.clone()),
            TableSpec::flat(self.events.clone()),
        ]
    }
}

fn encode(table: Table) -> Vec<u8> {
    postcard::to_stdvec(&(NAMESPACE, table)).expect("a namespace and a tag serialize")
}

/// The table an id names, if it belongs to this backend.
#[must_use]
pub(crate) fn decode(id: &[u8]) -> Option<Table> {
    let (namespace, table) = postcard::from_bytes::<(&str, Table)>(id).ok()?;
    (namespace == NAMESPACE).then_some(table)
}

/// Whether this cache keeps `id`. `None` means the id is another subsystem's
/// (not ours to drop); `Some(false)` is a stale table of ours.
#[must_use]
pub(crate) fn keep(id: &[u8], fingerprint: [u8; 32]) -> Option<bool> {
    Some(match decode(id)? {
        Table::Meta(version) => version == META_VERSION,
        Table::Events(found) => found == fingerprint,
    })
}

/// `blake3` over the log's declarations in id order.
#[must_use]
pub(crate) fn fingerprint(registry: &Registry) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    for decl in registry.iter() {
        hasher.update(&decl.id.0.to_le_bytes());
        hasher.update(&decl.hash);
    }
    *hasher.finalize().as_bytes()
}

/// The key of one event body: `group_be8 || slot_be8`.
#[must_use]
pub(crate) fn event_key(group: LocGroupId, slot: GroupEventId) -> [u8; 16] {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&group.0.to_le_bytes());
    key[8..].copy_from_slice(&slot.0.to_le_bytes());
    key
}

/// A body's cache value: `body_schema_be2 || body`.
#[must_use]
pub(crate) fn event_value(schema: SchemaId, body: &[u8]) -> Vec<u8> {
    let mut value = Vec::with_capacity(2 + body.len());
    value.extend_from_slice(&schema.0.to_le_bytes());
    value.extend_from_slice(body);
    value
}

/// The inverse of [`event_value`].
#[must_use]
pub(crate) fn event_body(value: &[u8]) -> Option<(SchemaId, &[u8])> {
    let (schema, body) = value.split_at_checked(2)?;
    Some((SchemaId(u16::from_le_bytes([schema[0], schema[1]])), body))
}

/// The store's tables: gear views plus this backend's meta and events.
pub(crate) fn layout<R: RuntimeTypes>(tables: &Tables, fingerprint: [u8; 32]) -> Layout {
    let gear = gear_tables::<R>();
    let mut specs = gear_specs(&gear);
    specs.extend(tables.specs());
    Layout::new(specs, move |id| {
        if let Some(table) = GearTable::decode(id) {
            return gear.contains(&table);
        }
        if id == legacy_gear_table() {
            return false;
        }
        keep(id, fingerprint).unwrap_or(true)
    })
}

/// The cursor the cache claims, if it belongs to this log and this layout.
pub(crate) async fn read_applied(
    cache: &Storage<FsDisk>,
    tables: &Tables,
    identity: [u8; 16],
    fingerprint: [u8; 32],
) -> Cursor {
    let bytes = cache
        .get(&tables.meta, META_KEY)
        .await
        .expect("read cache meta");
    let Some(bytes) = bytes else {
        return Cursor::START;
    };
    let Ok(meta) = postcard::from_bytes::<Meta>(&bytes) else {
        log::warn!("journaled cache: unreadable meta; refilling from the journal");
        return Cursor::START;
    };
    if meta.log == identity && meta.fingerprint == fingerprint && meta.fold == FOLD_VERSION {
        meta.applied()
    } else {
        log::warn!(
            "journaled cache: materialization belongs to another log or layout; \
             refilling from the journal"
        );
        Cursor::START
    }
}

pub(crate) fn write_meta(
    session: &mut Session<FsDisk>,
    tables: &Tables,
    identity: [u8; 16],
    fingerprint: [u8; 32],
    applied: Cursor,
) -> Result<(), StoreError> {
    let meta = Meta {
        log: identity,
        applied: (applied.segment, applied.offset),
        fingerprint,
        fold: FOLD_VERSION,
    };
    let bytes = postcard::to_stdvec(&meta).expect("a meta record serializes");
    session.put(&tables.meta, META_KEY, &bytes, &[], &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_ids_round_trip_and_keep_their_own_kind() {
        let fingerprint = [7u8; 32];
        let tables = Tables::new(fingerprint);
        assert_eq!(decode(&tables.meta), Some(Table::Meta(META_VERSION)));
        assert_eq!(decode(&tables.events), Some(Table::Events(fingerprint)));
        assert_eq!(keep(&tables.events, fingerprint), Some(true));
        assert_eq!(keep(&tables.events, [8u8; 32]), Some(false));
        assert_eq!(keep(b"another/subsystem", fingerprint), None);
    }

    #[test]
    fn an_event_value_carries_its_schema() {
        let value = event_value(SchemaId(19), b"payload");
        assert_eq!(event_body(&value), Some((SchemaId(19), &b"payload"[..])));
        assert_eq!(event_body(&[0u8; 1]), None);
    }
}
