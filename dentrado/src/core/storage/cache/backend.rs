//! `IsStorage` for the cache backend: a list of delegations into [`Ops`],
//! the live path, and the durability helpers.
//!
//! [`Ops`]: super::ops::Ops

use std::io;

use dentrado_cache::disk::IsFilesystem;

use super::{
    live::Ready,
    storage::{self, CacheStorage},
};
use crate::{
    core::{
        api::{GearTable, RuntimeTypes},
        loc_ctx::{GroupDelta, StoredEvent},
        storage::{
            GroupDiff, IsStorage, Watermark,
            view::{DiskView, gear_key},
        },
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

impl<R: RuntimeTypes, D: IsFilesystem + Default + Clone> IsStorage<R> for CacheStorage<R, D> {
    // ── localization: idempotent allocators ──────────────────────────────

    async fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        let mut state = self.ready().await;
        state.ready.as_mut().expect("opened").ensure_user(uid).await
    }

    async fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        let mut state = self.ready().await;
        state
            .ready
            .as_mut()
            .expect("opened")
            .ensure_sender(pk, uid)
            .await
    }

    async fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        let mut state = self.ready().await;
        state
            .ready
            .as_mut()
            .expect("opened")
            .ensure_group(msg_type, group)
            .await
    }

    async fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        let mut state = self.ready().await;
        state
            .ready
            .as_mut()
            .expect("opened")
            .ensure_data(data_id, content)
            .await
    }

    // ── localization: reverse lookups ────────────────────────────────────

    async fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        let state = self.ready().await;
        ready(&state).ops().loc_user(lid).await
    }

    async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        let state = self.ready().await;
        ready(&state)
            .ops()
            .loc_sender(sid)
            .await
            .and_then(|(_, u)| u)
    }

    async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        let state = self.ready().await;
        ready(&state).ops().loc_sender(sid).await.map(|(pk, _)| pk)
    }

    async fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        let state = self.ready().await;
        ready(&state).ops().data_loc(*data_id).await
    }

    async fn fetch_data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        let state = self.ready().await;
        ready(&state).ops().loc_data(did).await
    }

    async fn global_hash<V: GlobalHash>(&self, value: &V) -> Result<[u8; 32], GroupRouteError> {
        let state = self.ready().await;
        value.global_hash(&ready(&state).ops()).await
    }

    // ── event log: one append-only shard per group ───────────────────────

    async fn store_event(&self, group: LocGroupId, ev: StoredEvent<R::Body>) -> Option<GroupDelta> {
        let mut state = self.ready().await;
        state
            .ready
            .as_mut()
            .expect("opened")
            .store_event(group, ev)
            .await
    }

    async fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        let state = self.ready().await;
        ready(&state).ops().stored_event(group, slot).await
    }

    async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let state = self.ready().await;
        ready(&state).ops().diff_group(group, since).await
    }

    // ── layer 3: the gear's private view ─────────────────────────────────

    type View<'a>
        = DiskView<D>
    where
        Self: 'a;

    async fn gear_view(&self, gear: &R::GearId) -> Self::View<'_> {
        let table = GearTable::of(R::gear_kind(gear));
        let prefix = gear_key::<R>(gear).to_vec();
        let state = self.ready().await;
        DiskView::new(std::rc::Rc::clone(&ready(&state).cache), table, prefix)
    }

    // ── durability ───────────────────────────────────────────────────────

    async fn commit_log(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        state.ready.as_mut().expect("opened").commit_log().await
    }

    async fn flush(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        storage::flush(state.ready.as_mut().expect("opened")).await
    }
}

/// The opened state of a guard that [`CacheStorage::ready`] just returned.
fn ready<R: RuntimeTypes, D: IsFilesystem>(state: &storage::State<R, D>) -> &Ready<R, D> {
    state.ready.as_ref().expect("opened")
}
