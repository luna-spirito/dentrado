//! The tables the backend materializes layers 1–3 into.
//!
//! The page cache is the complete materialization of everything below a
//! gear's private view: localization, the per-group event shards, content
//! data, and the gear views themselves. The journal stays the source of
//! truth; a cache image is a checkpoint that lets a restart replay only the
//! journal tail.
//!
//! Table ids are two bytes (`magic, kind`). They are not versioned per table:
//! [`Meta`] records the cache layout, the log identity, and the runtime's
//! value shapes, and any mismatch wipes the image and refills it from the
//! journal. A table id therefore only names a *kind* of data, never a
//! particular generation of it.
//!
//! The keys and values live in [`super::codec`].

use dentrado_cache::{Session, Storage, StoreError, TableSpec, disk::IsFilesystem};
use dentrado_journal::Cursor;
use serde::{Deserialize, Serialize};

use crate::{
    core::{
        api::{GearTable, RuntimeTypes},
        storage::{
            store::Layout,
            view::{gear_specs, gear_tables, legacy_gear_table},
        },
    },
    types::Shape,
};

/// The first byte of every table id this module owns. Chosen to be invalid as
/// a postcard string length, so [`GearTable::decode`] never mistakes one of
/// our ids for a gear table (and vice versa).
const MAGIC: u8 = 0xD5;

/// The cache layout version. Bump whenever a record encoding below changes in
/// a way replay could not reconstruct.
pub(crate) const CACHE_VERSION: u32 = 2;

/// One table this backend owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Table {
    Meta,
    /// Every localization record: users, senders, groups, data, counters.
    Loc,
    /// Event slots and removed-slot rows.
    Events,
    /// The `(group, sender, tx_id)` dedup index.
    EventKeys,
    /// Per-group shard counters.
    Shards,
}

impl Table {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 5] = [
        Self::Meta,
        Self::Loc,
        Self::Events,
        Self::EventKeys,
        Self::Shards,
    ];

    const fn tag(self) -> u8 {
        match self {
            Self::Meta => 0,
            Self::Loc => 1,
            Self::Events => 2,
            Self::EventKeys => 3,
            Self::Shards => 4,
        }
    }

    const fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::Meta,
            1 => Self::Loc,
            2 => Self::Events,
            3 => Self::EventKeys,
            4 => Self::Shards,
            _ => return None,
        })
    }

    pub(crate) fn id(self) -> Vec<u8> {
        vec![MAGIC, self.tag()]
    }

    /// The table an id names, if it is ours.
    pub(crate) fn decode(id: &[u8]) -> Option<Self> {
        match id {
            [MAGIC, tag] => Self::from_tag(*tag),
            _ => None,
        }
    }
}

/// The pre-refactor ids (`postcard(("dentrado", ...))`). They name the same
/// data under a different layout; the startup GC drops them.
fn is_legacy_backend(id: &[u8]) -> bool {
    id.len() >= 9 && id[0] == 8 && &id[1..9] == b"dentrado"
}

/// This backend's table ids, resolved once per store.
#[derive(Debug, Clone)]
pub(crate) struct Tables {
    pub meta: Vec<u8>,
    pub loc: Vec<u8>,
    pub events: Vec<u8>,
    pub event_keys: Vec<u8>,
    pub shards: Vec<u8>,
}

impl Tables {
    pub(crate) fn new() -> Self {
        Self {
            meta: Table::Meta.id(),
            loc: Table::Loc.id(),
            events: Table::Events.id(),
            event_keys: Table::EventKeys.id(),
            shards: Table::Shards.id(),
        }
    }

    fn specs(&self) -> Vec<TableSpec> {
        [
            &self.meta,
            &self.loc,
            &self.events,
            &self.event_keys,
            &self.shards,
        ]
        .map(|id| TableSpec::flat(id.clone()))
        .to_vec()
    }
}

/// The store's tables: gear views plus this backend's own.
pub(crate) fn layout<R: RuntimeTypes>(tables: &Tables) -> Layout {
    let gear = gear_tables::<R>();
    let mut specs = gear_specs(&gear);
    specs.extend(tables.specs());
    Layout::new(specs, move |id| {
        if Table::decode(id).is_some() {
            return true;
        }
        if let Some(table) = GearTable::decode(id) {
            return gear.contains(&table);
        }
        if id == legacy_gear_table() || is_legacy_backend(id) {
            return false;
        }
        true
    })
}

/// `blake3` over every runtime layout the cache stores. A change here makes
/// the image unreadable, not merely stale: keys stay, values do not.
pub(crate) fn fingerprint<R: RuntimeTypes>() -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&CACHE_VERSION.to_le_bytes());
    hasher.update(R::Group::shape().as_bytes());
    hasher.update(R::Data::shape().as_bytes());
    hasher.update(R::Body::shape().as_bytes());
    *hasher.finalize().as_bytes()
}

/// What the image has applied, and to which log and runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Meta {
    version: u32,
    log: [u8; 16],
    fingerprint: [u8; 32],
    applied: (u64, u64),
}

impl Meta {
    fn applied(&self) -> Cursor {
        Cursor::new(self.applied.0, self.applied.1)
    }
}

/// What an existing image claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaState {
    /// No meta record: a fresh image.
    Fresh,
    /// The image belongs to this log and this runtime, applied through the
    /// given cursor.
    Valid(Cursor),
    /// The image belongs to another log or runtime. It must be wiped.
    Stale,
}

impl MetaState {
    /// The cursor to start from, after the caller has wiped a stale image.
    pub(crate) fn start(self) -> Cursor {
        match self {
            Self::Valid(at) => at,
            Self::Fresh | Self::Stale => Cursor::START,
        }
    }
}

pub(crate) async fn read_meta<D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    log: [u8; 16],
    fingerprint: [u8; 32],
) -> MetaState {
    let bytes = cache.get(&tables.meta, &[]).await.expect("read cache meta");
    let Some(bytes) = bytes else {
        return MetaState::Fresh;
    };
    let Ok(meta) = postcard::from_bytes::<Meta>(&bytes) else {
        log::warn!("cache meta is unreadable; refilling from the journal");
        return MetaState::Stale;
    };
    if meta.version == CACHE_VERSION && meta.log == log && meta.fingerprint == fingerprint {
        MetaState::Valid(meta.applied())
    } else {
        log::warn!(
            "cache belongs to another log or runtime (version {}, log {:?}); \
             refilling from the journal",
            meta.version,
            meta.log
        );
        MetaState::Stale
    }
}

pub(crate) fn write_meta<D: IsFilesystem>(
    session: &mut Session<D>,
    tables: &Tables,
    log: [u8; 16],
    fingerprint: [u8; 32],
    applied: Cursor,
) -> Result<(), StoreError> {
    let meta = Meta {
        version: CACHE_VERSION,
        log,
        fingerprint,
        applied: (applied.segment, applied.offset),
    };
    let bytes = postcard::to_stdvec(&meta).expect("a meta record serializes");
    session.put(&tables.meta, &[], &bytes, &[], &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_ids_round_trip_and_do_not_look_like_gear_tables() {
        for table in Table::ALL {
            assert_eq!(Table::decode(&table.id()), Some(table));
            assert!(GearTable::decode(&table.id()).is_none(), "{table:?}");
        }
        assert_eq!(Table::decode(b"dentrado"), None);
    }

    #[test]
    fn the_old_backend_ids_are_recognized_as_legacy() {
        let old = postcard::to_stdvec(&("dentrado", 0u8)).unwrap();
        assert!(is_legacy_backend(&old));
        assert!(!is_legacy_backend(&Table::Meta.id()));
    }
}
