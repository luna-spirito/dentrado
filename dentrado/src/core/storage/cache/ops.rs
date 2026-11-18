//! Layer 1–2 operations over the page cache.
//!
//! Everything here is idempotent: applying a loc assignment twice is a no-op,
//! and an event whose `(sender, tx_id)` is already stored is either identical
//! (no-op) or a conflict (a loud `todo!()` until observations can be ranked by
//! authority). That makes replay safe to interrupt: a crash before the meta
//! cursor advances simply re-applies the same records.
//!
//! Each call is atomic on its own: the caller holds the backend's operation
//! gate (an async mutex; see [`super::storage`]), and the methods commit one
//! session per logical write. There is no read-your-writes problem within an
//! operation because a read never overlaps an uncommitted write — the gate
//! serializes the whole operation.

use std::marker::PhantomData;

use dentrado_cache::{Storage, disk::IsFilesystem};

use super::{codec, tables::Tables};
use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::StoredEvent,
        storage::{GroupDiff, Watermark},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GlobalResolver, GroupEventId, GroupRouteError,
        LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, NodeId, SenderPk, UserId,
    },
};

/// The allocators: one monotonic counter each, never a row count.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Counter {
    User,
    Sender,
    Group,
    Data,
}

impl Counter {
    const fn tag(self) -> u8 {
        match self {
            Self::User => 0,
            Self::Sender => 1,
            Self::Group => 2,
            Self::Data => 3,
        }
    }
}

/// What an event would do to its group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventOutcome {
    /// The key is new; the event belongs at this slot.
    New(GroupEventId),
    /// The same observation is already stored at this slot.
    Same(GroupEventId),
    /// The key is known with different bytes. Supersede is not implemented.
    Conflict(GroupEventId),
}

/// The read/write operations shared by the live path and replay.
pub(crate) struct Ops<'a, R: RuntimeTypes, D: IsFilesystem> {
    cache: &'a Storage<D>,
    tables: &'a Tables,
    _r: PhantomData<fn() -> R>,
}

impl<'a, R: RuntimeTypes, D: IsFilesystem> Ops<'a, R, D> {
    pub(crate) fn new(cache: &'a Storage<D>, tables: &'a Tables) -> Self {
        Self {
            cache,
            tables,
            _r: PhantomData,
        }
    }

    // ── raw access ────────────────────────────────────────────────────────

    async fn get(&self, table: &[u8], key: &[u8]) -> Option<Vec<u8>> {
        self.cache.get(table, key).await.expect("read the cache")
    }

    async fn put_many(&self, rows: &[(&[u8], Vec<u8>, Vec<u8>)]) {
        let mut session = self.cache.session();
        for (table, key, value) in rows {
            session
                .put(table, key, value, &[], &[])
                .expect("stage a cache record");
        }
        session.commit().expect("commit cache records");
    }

    // ── counters ──────────────────────────────────────────────────────────

    pub(crate) async fn next(&self, which: Counter) -> u64 {
        self.get(&self.tables.counters, &codec::counter(which.tag()))
            .await
            .and_then(|bytes| codec::parse_loc(&bytes))
            .unwrap_or(0)
    }

    // ── localization lookups ──────────────────────────────────────────────

    pub(crate) async fn user_loc(&self, uid: UserId) -> Option<LocUserId> {
        let bytes = self.get(&self.tables.users, &codec::user_fwd(uid)).await?;
        Some(LocUserId(codec::parse_loc(&bytes)?))
    }

    pub(crate) async fn loc_user(&self, loc: LocUserId) -> Option<UserId> {
        let bytes = self.get(&self.tables.users, &codec::user_rev(loc)).await?;
        codec::parse_user(&bytes)
    }

    pub(crate) async fn sender_loc(&self, pk: SenderPk) -> Option<LocSenderId> {
        let bytes = self
            .get(&self.tables.senders, &codec::sender_fwd(pk))
            .await?;
        Some(codec::parse_sender_fwd(&bytes)?.0)
    }

    pub(crate) async fn loc_sender(
        &self,
        loc: LocSenderId,
    ) -> Option<(SenderPk, Option<LocUserId>)> {
        let bytes = self
            .get(&self.tables.senders, &codec::sender_rev(loc))
            .await?;
        codec::parse_sender_rev(&bytes)
    }

    pub(crate) async fn group_loc(
        &self,
        msg_type: LocMsgTypeId,
        group: &[u8],
    ) -> Option<LocGroupId> {
        let bytes = self
            .get(&self.tables.groups, &codec::group_fwd(msg_type.0, group))
            .await?;
        Some(LocGroupId(codec::parse_loc(&bytes)?))
    }

    pub(crate) async fn data_loc(&self, data_id: DataId) -> Option<LocDataId> {
        let bytes = self
            .get(&self.tables.data, &codec::data_fwd(data_id))
            .await?;
        Some(LocDataId(codec::parse_loc(&bytes)?))
    }

    pub(crate) async fn loc_data(&self, loc: LocDataId) -> Option<(DataId, R::Data)> {
        let bytes = self.get(&self.tables.data, &codec::data_rev(loc)).await?;
        let data_id = DataId {
            timestamp: u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?),
            hash: bytes.get(4..36)?.try_into().ok()?,
        };
        let content = postcard::from_bytes(bytes.get(36..)?).expect("a cached data value decodes");
        Some((data_id, content))
    }

    // ── localization apply ────────────────────────────────────────────────

    pub(crate) async fn apply_user(&self, loc: LocUserId, uid: UserId) -> Result<(), String> {
        let fk = codec::user_fwd(uid);
        let rk = codec::user_rev(loc);
        let fwd = self.get(&self.tables.users, &fk).await;
        let rev = self.get(&self.tables.users, &rk).await;
        match (fwd.as_deref(), rev.as_deref()) {
            (None, None) => {}
            (Some(f), Some(r))
                if codec::parse_loc(f) == Some(loc.0) && codec::parse_user(r) == Some(uid) =>
            {
                return Ok(());
            }
            _ => {
                return Err(format!(
                    "user {} (loc {}) is already interned differently",
                    uid.id, loc.0
                ));
            }
        }
        let next = self.next(Counter::User).await;
        if loc.0 != next {
            return Err(format!(
                "user loc {} is out of order (next is {next})",
                loc.0
            ));
        }
        self.put_many(&[
            (&self.tables.users, fk, codec::loc_bytes(loc.0).to_vec()),
            (&self.tables.users, rk, codec::user_bytes(uid).to_vec()),
            (
                &self.tables.counters,
                codec::counter(Counter::User.tag()),
                codec::loc_bytes(loc.0 + 1).to_vec(),
            ),
        ])
        .await;
        Ok(())
    }

    pub(crate) async fn apply_sender(
        &self,
        loc: LocSenderId,
        pk: SenderPk,
        user: Option<LocUserId>,
    ) -> Result<(), String> {
        let fk = codec::sender_fwd(pk);
        let rk = codec::sender_rev(loc);
        let fwd_value = codec::sender_fwd_bytes(loc, user);
        let rev_value = codec::sender_rev_bytes(pk, user);
        let fwd = self.get(&self.tables.senders, &fk).await;
        let rev = self.get(&self.tables.senders, &rk).await;
        match (fwd.as_deref(), rev.as_deref()) {
            (None, None) => {}
            (Some(f), Some(r)) if f == fwd_value.as_slice() && r == rev_value.as_slice() => {
                return Ok(());
            }
            _ => {
                return Err("a sender is already interned differently".to_owned());
            }
        }
        if let Some(user) = user
            && self.loc_user(user).await.is_none()
        {
            return Err(format!(
                "sender names user loc {}, which was not applied",
                user.0
            ));
        }
        let next = self.next(Counter::Sender).await;
        if loc.0 != next {
            return Err(format!(
                "sender loc {} is out of order (next is {next})",
                loc.0
            ));
        }
        self.put_many(&[
            (&self.tables.senders, fk, fwd_value),
            (&self.tables.senders, rk, rev_value),
            (
                &self.tables.counters,
                codec::counter(Counter::Sender.tag()),
                codec::loc_bytes(loc.0 + 1).to_vec(),
            ),
        ])
        .await;
        Ok(())
    }

    pub(crate) async fn apply_group(
        &self,
        loc: LocGroupId,
        msg_type: LocMsgTypeId,
        group: &R::Group,
    ) -> Result<(), String> {
        let bytes = postcard::to_stdvec(group).expect("a group value serializes");
        let fk = codec::group_fwd(msg_type.0, &bytes);
        let rk = codec::group_rev(loc);
        let value = codec::group_bytes(msg_type.0, &bytes);
        let fwd = self.get(&self.tables.groups, &fk).await;
        let rev = self.get(&self.tables.groups, &rk).await;
        match (fwd.as_deref(), rev.as_deref()) {
            (None, None) => {}
            (Some(f), Some(r))
                if codec::parse_loc(f) == Some(loc.0)
                    && codec::parse_group(r)
                        .is_some_and(|(mt, g)| mt == msg_type.0 && g == bytes.as_slice()) =>
            {
                return Ok(());
            }
            _ => {
                return Err(format!(
                    "group (msg type {}) is already interned differently",
                    msg_type.0
                ));
            }
        }
        let next = self.next(Counter::Group).await;
        if loc.0 != next {
            return Err(format!(
                "group loc {} is out of order (next is {next})",
                loc.0
            ));
        }
        self.put_many(&[
            (&self.tables.groups, fk, codec::loc_bytes(loc.0).to_vec()),
            (&self.tables.groups, rk, value),
            (
                &self.tables.counters,
                codec::counter(Counter::Group.tag()),
                codec::loc_bytes(loc.0 + 1).to_vec(),
            ),
        ])
        .await;
        Ok(())
    }

    pub(crate) async fn verify_data(
        &self,
        data_id: DataId,
        content: &R::Data,
    ) -> Result<(), DataVerifyError> {
        match content.global_hash(self).await {
            Ok(hash) if hash == data_id.hash => Ok(()),
            Ok(computed_hash) => Err(DataVerifyError::HashMismatch {
                claimed: data_id,
                computed_hash,
            }),
            Err(e) => Err(DataVerifyError::UnresolvableId(e)),
        }
    }

    pub(crate) async fn apply_data(
        &self,
        loc: LocDataId,
        data_id: DataId,
        content: &R::Data,
    ) -> Result<(), String> {
        self.verify_data(data_id, content)
            .await
            .map_err(|e| e.to_string())?;
        self.put_data(loc, data_id, content).await
    }

    /// Write a data assignment whose content was already verified.
    pub(crate) async fn put_data(
        &self,
        loc: LocDataId,
        data_id: DataId,
        content: &R::Data,
    ) -> Result<(), String> {
        let fk = codec::data_fwd(data_id);
        let rk = codec::data_rev(loc);
        let mut value = Vec::with_capacity(36);
        value.extend_from_slice(&data_id.timestamp.to_le_bytes());
        value.extend_from_slice(&data_id.hash);
        value.extend_from_slice(&postcard::to_stdvec(content).expect("a data value serializes"));
        let fwd = self.get(&self.tables.data, &fk).await;
        let rev = self.get(&self.tables.data, &rk).await;
        match (fwd.as_deref(), rev.as_deref()) {
            (None, None) => {}
            (Some(f), Some(r))
                if codec::parse_loc(f) == Some(loc.0) && r.get(..36) == Some(&value[..36]) =>
            {
                return Ok(());
            }
            _ => {
                return Err(format!(
                    "data {:?} (loc {}) is already interned differently",
                    data_id, loc.0
                ));
            }
        }
        let next = self.next(Counter::Data).await;
        if loc.0 != next {
            return Err(format!(
                "data loc {} is out of order (next is {next})",
                loc.0
            ));
        }
        self.put_many(&[
            (&self.tables.data, fk, codec::loc_bytes(loc.0).to_vec()),
            (&self.tables.data, rk, value),
            (
                &self.tables.counters,
                codec::counter(Counter::Data.tag()),
                codec::loc_bytes(loc.0 + 1).to_vec(),
            ),
        ])
        .await;
        Ok(())
    }

    // ── event shards ──────────────────────────────────────────────────────

    pub(crate) async fn shard(&self, group: LocGroupId) -> (u64, u64) {
        self.get(&self.tables.shards, &codec::shard(group))
            .await
            .and_then(|bytes| codec::parse_shard(&bytes))
            .unwrap_or((0, 0))
    }

    /// What storing `value` under `(sender, tx_id)` would do. `value` is the
    /// encoded event (see [`codec::event_bytes`]).
    pub(crate) async fn event_outcome(
        &self,
        group: LocGroupId,
        sender: LocSenderId,
        tx_id: u32,
        value: &[u8],
    ) -> EventOutcome {
        let key = codec::event_key(group, sender.0, tx_id);
        let Some(slot) = self.get(&self.tables.event_keys, &key).await else {
            let (added, _) = self.shard(group).await;
            return EventOutcome::New(GroupEventId(added));
        };
        let slot = GroupEventId(codec::parse_loc(&slot).expect("a dedup slot"));
        match self
            .get(&self.tables.events, &codec::event(group, slot))
            .await
        {
            Some(stored) if stored == value => EventOutcome::Same(slot),
            _ => EventOutcome::Conflict(slot),
        }
    }

    /// Write a new event at `slot` and advance its shard's `added` counter.
    pub(crate) async fn put_event(&self, group: LocGroupId, slot: GroupEventId, value: &[u8]) {
        let parts = codec::parse_event(value).expect("an encoded event parses");
        let (added, removed) = self.shard(group).await;
        debug_assert_eq!(slot.0, added, "a new event takes the shard's next slot");
        self.put_many(&[
            (
                &self.tables.events,
                codec::event(group, slot),
                value.to_vec(),
            ),
            (
                &self.tables.event_keys,
                codec::event_key(group, parts.sender, parts.tx_id),
                codec::loc_bytes(slot.0).to_vec(),
            ),
            (
                &self.tables.shards,
                codec::shard(group).to_vec(),
                codec::shard_bytes(slot.0 + 1, removed),
            ),
        ])
        .await;
    }

    pub(crate) async fn stored_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        let value = self
            .get(&self.tables.events, &codec::event(group, slot))
            .await?;
        let parts = codec::parse_event(&value)?;
        Some(StoredEvent {
            sender: LocSenderId(parts.sender),
            tx_id: parts.tx_id,
            timestamp: parts.timestamp,
            source_node: NodeId(parts.source_node),
            body: postcard::from_bytes(parts.body).expect("a cached event body decodes"),
        })
    }

    pub(crate) async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let (added, removed) = self.shard(group).await;
        let a = since.added.min(added);
        let r = since.removed.min(removed);
        let mut removed_ids = Vec::new();
        for index in r..removed {
            if let Some(bytes) = self
                .get(&self.tables.removed, &codec::removed(group, index))
                .await
            {
                removed_ids.push(GroupEventId(
                    codec::parse_loc(&bytes).expect("a removed slot"),
                ));
            }
        }
        GroupDiff {
            added: (a..added).map(GroupEventId).collect(),
            removed: removed_ids,
            watermark: Watermark { added, removed },
        }
    }
}

impl<R: RuntimeTypes, D: IsFilesystem> GlobalResolver for Ops<'_, R, D> {
    async fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        match self.loc_user(lid).await {
            Some(user) => Ok(user),
            None => Err(GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: self.next(Counter::User).await as usize,
            }),
        }
    }

    async fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        match self.loc_data(did).await {
            Some((id, _)) => Ok(id),
            None => Err(GroupRouteError::DataIdOutOfBounds {
                idx: did.0,
                objects_len: self.next(Counter::Data).await as usize,
            }),
        }
    }
}
