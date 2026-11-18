//! The event table: committed bodies, the dedup index, and replay.
//!
//! Live events never enter the delta. They remain in the sequencer's pending
//! set until `commit_log` writes them here, so [`fetch_event`] and
//! [`diff_group`](super::CacheStorage::diff_group) only ever see durable
//! events.

use dentrado_cache::{Storage, disk::IsFilesystem};

use super::{codec, fold, tables::Tables};
use crate::{
    core::{api::RuntimeTypes, loc_ctx::StoredEvent},
    types::{GroupEventId, LocGroupId, LocSenderId, NodeId},
};

pub(crate) async fn stored_event<R: RuntimeTypes, D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    group: LocGroupId,
    slot: GroupEventId,
) -> Option<StoredEvent<R::Body>> {
    let value = cache
        .get(tables.events, &codec::event(group, slot))
        .await
        .expect("read the cache")?;
    let parts = codec::parse_event(&value)?;
    Some(StoredEvent {
        sender: LocSenderId(parts.sender),
        tx_id: parts.tx_id,
        timestamp: parts.timestamp,
        source_node: NodeId(parts.source_node),
        body: postcard::from_bytes(parts.body).expect("a cached event body decodes"),
    })
}

/// Replay one event: the first observation at the shard's tip, an identical
/// duplicate ignored, a conflicting duplicate a loud `todo!()`.
pub(crate) async fn replay<D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    group: LocGroupId,
    sender: LocSenderId,
    tx_id: u32,
    value: &[u8],
) -> Result<(), String> {
    let key = codec::event_key(group, sender, tx_id);
    if let Some(slot) = cache
        .get(tables.event_keys, &key)
        .await
        .expect("read the cache")
    {
        let slot = GroupEventId(codec::parse_loc(&slot).expect("a dedup slot"));
        let stored = cache
            .get(tables.events, &codec::event(group, slot))
            .await
            .expect("read the cache");
        if stored.as_deref() == Some(value) {
            return Ok(());
        }
        todo!("supersede: rank by (authoritative, timestamp, source_node)");
    }

    let (added, removed) = cache
        .get(tables.shards, &codec::shard(group))
        .await
        .expect("read the cache")
        .and_then(|bytes| codec::parse_shard(&bytes))
        .unwrap_or((0, 0));
    let slot = GroupEventId(added);
    let rows = fold::event_rows(tables, group, slot, sender, tx_id, value);

    let mut session = cache.session();
    for row in rows {
        session
            .put(row.table, &row.key, &row.value, &[], &[])
            .expect("stage an event");
    }
    session
        .put(
            tables.shards,
            &codec::shard(group),
            &codec::shard_bytes(added + 1, removed),
            &[],
            &[],
        )
        .expect("stage a shard");
    session
        .commit()
        .map_err(|e| format!("commit an event: {e:?}"))
}
