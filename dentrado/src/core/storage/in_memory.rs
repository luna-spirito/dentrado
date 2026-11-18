//! The RAM backend: a [`Fold`] plus a `BTreeMap` of gear views.
//!
//! Used as the test backend, as the `PagedStorage::ram()` dev mode, and as the
//! reference a disk backend is validated against. It holds no journal and does
//! no IO: every method does its work eagerly and returns an already-ready
//! future, so no `RefCell` borrow is ever held across an `.await`.
//!
//! Nothing here survives process death — not just the gear views, but events
//! and localization too. That is a property of the backend, not of any one
//! layer; the `Storage` contract treats all of it as restart-surviving state,
//! satisfied here in the (only) trivial sense RAM allows: live for the lifetime
//! of this object. The durable backend pairs the same [`Fold`] with a journal.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    future::{Future, ready},
};

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
        storage::{GroupDiff, IsStorage, RamView, Watermark, fold::Fold, view::gear_key},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// In-memory typed `Storage`.
pub struct InMemoryStorage<R: RuntimeTypes> {
    fold: RefCell<Fold<R, R::Body>>,
    /// The gear views: full (prefixed) key → flat value.
    gear_data: RefCell<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl<R: RuntimeTypes> Default for InMemoryStorage<R> {
    fn default() -> Self {
        Self {
            fold: RefCell::new(Fold::default()),
            gear_data: RefCell::new(BTreeMap::new()),
        }
    }
}

impl<R: RuntimeTypes> InMemoryStorage<R> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The backing map of every gear view — used by [`PagedStorage`]'s
    /// RAM mode.
    ///
    /// [`PagedStorage`]: crate::core::storage::PagedStorage
    pub(crate) fn gear_data(&self) -> &RefCell<BTreeMap<Vec<u8>, Vec<u8>>> {
        &self.gear_data
    }
}

impl<R: RuntimeTypes> IsStorage<R> for InMemoryStorage<R> {
    // ── localization: idempotent allocators ───────────────────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId> {
        ready(self.fold.borrow_mut().mk_loc_user(uid))
    }

    fn mk_loc_sender(
        &self,
        pk: SenderPk,
        uid: Option<UserId>,
    ) -> impl Future<Output = LocSenderId> {
        ready(self.fold.borrow_mut().mk_loc_sender(pk, uid))
    }

    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId> {
        ready(self.fold.borrow_mut().mk_loc_group(msg_type, group))
    }

    fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> impl Future<Output = Result<LocDataId, DataVerifyError>> {
        ready(self.fold.borrow_mut().mk_data(data_id, content))
    }

    // ── localization: reverse lookups ─────────────────────────────────────

    fn user_by_local(&self, lid: LocUserId) -> impl Future<Output = Option<UserId>> {
        ready(self.fold.borrow().user_by_local(lid))
    }

    fn sender_user(&self, sid: LocSenderId) -> impl Future<Output = Option<LocUserId>> {
        ready(self.fold.borrow().sender_user(sid))
    }

    fn sender_pk(&self, sid: LocSenderId) -> impl Future<Output = Option<SenderPk>> {
        ready(self.fold.borrow().sender_pk(sid))
    }

    fn find_data(&self, data_id: &DataId) -> impl Future<Output = Option<LocDataId>> {
        ready(self.fold.borrow().find_data(data_id))
    }

    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<(DataId, R::Data)>> {
        ready(self.fold.borrow().fetch_data(did))
    }

    fn global_hash<V: GlobalHash>(
        &self,
        value: &V,
    ) -> impl Future<Output = Result<[u8; 32], GroupRouteError>> {
        ready(value.global_hash(&*self.fold.borrow()))
    }

    // ── event log: one append-only shard per group ───────────────────────

    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<GroupDelta>> {
        ready(self.fold.borrow_mut().store_event(group, ev))
    }

    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>> {
        ready(self.fold.borrow().fetch_event(group, slot))
    }

    fn diff_group(&self, group: LocGroupId, since: Watermark) -> impl Future<Output = GroupDiff> {
        ready(self.fold.borrow().diff_group(group, since))
    }

    // ── the gear's private view ───────────────────────────────────────────

    type View<'a>
        = RamView<'a>
    where
        Self: 'a;

    fn gear_view(&self, gear: &R::GearId) -> impl Future<Output = Self::View<'_>> {
        ready(RamView::new(self.gear_data(), gear_key::<R>(gear).to_vec()))
    }

    // ── durability ────────────────────────────────────────────────────────

    fn flush(&self) -> impl Future<Output = std::io::Result<()>> {
        ready(Ok(()))
    }
}
