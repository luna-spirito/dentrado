//! The tables this backend materializes, and the image's checkpoint.
//!
//! Each table has a stable name (its durable identity) and a [`TableId`]
//! issued by the open page store. The names below are the whole fixed set:
//! localization forward/reverse maps, one event table, the event dedup index,
//! per-group shard watermarks, and the cache meta. Gear views add one table
//! per gear kind.
//!
//! [`Meta`] is the checkpoint: the log identity, the runtime fingerprint, the
//! journal cursor whose records are in the image, and the allocator counters.
//! A mismatch wipes the image and refills it from the journal.

use std::collections::HashSet;

use dentrado_cache::{Session, Storage, StoreError, TableId, TableSpec, disk::IsFilesystem};
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

/// The cache layout version. Bump whenever a record encoding below changes in
/// a way replay could not reconstruct.
pub(crate) const CACHE_VERSION: u32 = 3;

const META: &[u8] = b"dentrado/meta";
const USERS_FWD: &[u8] = b"dentrado/users/fwd";
const USERS_REV: &[u8] = b"dentrado/users/rev";
const SENDERS_FWD: &[u8] = b"dentrado/senders/fwd";
const SENDERS_REV: &[u8] = b"dentrado/senders/rev";
const GROUPS_FWD: &[u8] = b"dentrado/groups/fwd";
const DATA_FWD: &[u8] = b"dentrado/data/fwd";
const DATA_REV: &[u8] = b"dentrado/data/rev";
const EVENTS: &[u8] = b"dentrado/events";
const EVENT_KEYS: &[u8] = b"dentrado/event-keys";
const SHARDS: &[u8] = b"dentrado/shards";

/// The fixed table names, in declaration order.
const FIXED: [&[u8]; 11] = [
    META,
    USERS_FWD,
    USERS_REV,
    SENDERS_FWD,
    SENDERS_REV,
    GROUPS_FWD,
    DATA_FWD,
    DATA_REV,
    EVENTS,
    EVENT_KEYS,
    SHARDS,
];

/// This backend's handles, resolved once per open.
#[derive(Clone)]
pub(crate) struct Tables {
    pub meta: TableId,
    pub users_fwd: TableId,
    pub users_rev: TableId,
    pub senders_fwd: TableId,
    pub senders_rev: TableId,
    pub groups_fwd: TableId,
    pub data_fwd: TableId,
    pub data_rev: TableId,
    pub events: TableId,
    pub event_keys: TableId,
    pub shards: TableId,
    gears: Vec<(&'static str, TableId)>,
}

impl Tables {
    /// Resolve every name in the configured layout to its handle.
    pub(crate) fn resolve<R: RuntimeTypes, D: IsFilesystem>(cache: &Storage<D>) -> Self {
        let get = |name: &[u8]| {
            cache
                .table(name)
                .unwrap_or_else(|| panic!("table {name:?} is not in the store layout"))
        };
        Self {
            meta: get(META),
            users_fwd: get(USERS_FWD),
            users_rev: get(USERS_REV),
            senders_fwd: get(SENDERS_FWD),
            senders_rev: get(SENDERS_REV),
            groups_fwd: get(GROUPS_FWD),
            data_fwd: get(DATA_FWD),
            data_rev: get(DATA_REV),
            events: get(EVENTS),
            event_keys: get(EVENT_KEYS),
            shards: get(SHARDS),
            gears: R::gear_kinds()
                .iter()
                .map(|&kind| (kind, get(&GearTable::of(kind).encode())))
                .collect(),
        }
    }

    /// The gear kind's table.
    pub(crate) fn gear(&self, kind: &str) -> TableId {
        self.gears
            .iter()
            .find_map(|&(name, table)| (name == kind).then_some(table))
            .unwrap_or_else(|| panic!("gear kind {kind:?} has no table"))
    }
}

/// The store's tables: gear views plus this backend's fixed set.
pub(crate) fn layout<R: RuntimeTypes>() -> Layout {
    let gear = gear_tables::<R>();
    let mut specs = gear_specs(&gear);
    let mut expected: HashSet<Vec<u8>> = specs.iter().map(|s| s.id.clone()).collect();
    for name in FIXED {
        specs.push(TableSpec::flat(name.to_vec()));
        expected.insert(name.to_vec());
    }
    Layout::new(specs, move |id| {
        if expected.contains(id) {
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

/// The pre-refactor ids (`postcard(("dentrado", ...))`). The startup GC drops
/// them.
fn is_legacy_backend(id: &[u8]) -> bool {
    id.len() >= 9 && id[0] == 8 && &id[1..9] == b"dentrado"
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
    /// Next ids for users, senders, groups, data.
    next: [u64; 4],
}

impl Meta {
    pub(crate) fn applied(&self) -> Cursor {
        Cursor::new(self.applied.0, self.applied.1)
    }

    pub(crate) fn next(&self) -> [u64; 4] {
        self.next
    }
}

/// What an existing image claims.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MetaState {
    /// No meta record: a fresh image.
    Fresh,
    /// The image belongs to this log and this runtime, applied through the
    /// given cursor.
    Valid(Meta),
    /// The image belongs to another log or runtime. It must be wiped.
    Stale,
}

impl MetaState {
    /// The cursor to start from, after the caller has wiped a stale image.
    pub(crate) fn start(self) -> Cursor {
        match self {
            Self::Valid(meta) => meta.applied(),
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
    let bytes = cache.get(tables.meta, &[]).await.expect("read cache meta");
    let Some(bytes) = bytes else {
        return MetaState::Fresh;
    };
    let Ok(meta) = postcard::from_bytes::<Meta>(&bytes) else {
        log::warn!("cache meta is unreadable; refilling from the journal");
        return MetaState::Stale;
    };
    if meta.version == CACHE_VERSION && meta.log == log && meta.fingerprint == fingerprint {
        MetaState::Valid(meta)
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
    next: [u64; 4],
) -> Result<(), StoreError> {
    let meta = Meta {
        version: CACHE_VERSION,
        log,
        fingerprint,
        applied: (applied.segment, applied.offset),
        next,
    };
    let bytes = postcard::to_stdvec(&meta).expect("a meta record serializes");
    session.put(tables.meta, &[], &bytes, &[], &[])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_old_backend_ids_are_recognized_as_legacy() {
        let old = postcard::to_stdvec(&("dentrado", 0u8)).unwrap();
        assert!(is_legacy_backend(&old));
        assert!(!is_legacy_backend(META));
    }
}
