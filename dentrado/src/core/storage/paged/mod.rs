//! The storage facade: RAM or journaled.
//!
//! Disk mode is entirely [`JournalStorage`]: layers 1–2 live in the journal,
//! the fold, and the body cache; gear views live in the same page store. RAM
//! mode is [`InMemoryStorage`], the non-durable reference backend.
//!
//! A view is a flat key-value overlay scoped to one gear (see [`GearView`]).
//! The disk view is one `Session` over the store: reads use the session's old
//! snapshot, writes are staged by the session, and the runtime commits the
//! whole batch after the gear run.
//!
//! ## The poison boundary
//!
//! A store or journal error is fatal: the backend panics rather than silently
//! degrading to RAM. The process is expected to restart against a fresh cache
//! (the cache is recomputable; the journal is not). A corrupt cache image is
//! quarantined (renamed aside, kept for inspection) and a fresh store opens in
//! its place; a failed open is a panic, never a silent RAM fallback.

use std::{io, path::PathBuf};

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
        storage::{
            GroupDiff, IsStorage, Watermark,
            in_memory::InMemoryStorage,
            journal::JournalStorage,
            view::{DiskView, GearView, RamView},
        },
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// [`IsStorage`] whose durability is chosen at construction.
pub struct PagedStorage<R: RuntimeTypes> {
    backend: Backend<R>,
}

enum Backend<R: RuntimeTypes> {
    /// Boxed: a storage is constructed once per core, never per call, and the
    /// two backends differ in size enough to make an inline enum wasteful.
    Ram(Box<InMemoryStorage<R>>),
    Journal(Box<JournalStorage<R>>),
}

impl<R: RuntimeTypes> Default for PagedStorage<R> {
    fn default() -> Self {
        Self::ram()
    }
}

impl<R: RuntimeTypes> PagedStorage<R> {
    /// Non-durable mode: every layer in RAM.
    #[must_use]
    pub fn ram() -> Self {
        Self {
            backend: Backend::Ram(Box::default()),
        }
    }

    /// Disk mode: `<file>.journal/` is the source of truth and `<file>` is the
    /// recomputable cache for event bodies and gear views. The parent
    /// directory must already exist; both open lazily inside the core's
    /// runtime on the first storage call.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        Self {
            backend: Backend::Journal(Box::new(JournalStorage::new(file))),
        }
    }
}

/// The two representations a [`PagedStorage`] view can take. The disk
/// branch is a `Session` + gear prefix; the RAM branch is the same
/// contract over a `BTreeMap` (the `ram()` toggle).
pub enum PagedView<'a> {
    Disk(DiskView),
    Ram(RamView<'a>),
}

impl GearView for PagedView<'_> {
    async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        match self {
            Self::Disk(view) => view.get(key).await,
            Self::Ram(view) => view.get(key).await,
        }
    }

    async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        match self {
            Self::Disk(view) => view.scan(prefix).await,
            Self::Ram(view) => view.scan(prefix).await,
        }
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        match self {
            Self::Disk(view) => view.put(key, value),
            Self::Ram(view) => view.put(key, value),
        }
    }

    fn del(&mut self, key: &[u8]) {
        match self {
            Self::Disk(view) => view.del(key),
            Self::Ram(view) => view.del(key),
        }
    }

    fn commit(&mut self) {
        match self {
            Self::Disk(view) => view.commit(),
            Self::Ram(view) => view.commit(),
        }
    }
}

impl<R: RuntimeTypes> IsStorage<R> for PagedStorage<R> {
    // ── localization ──────────────────────────────────────────────────────

    async fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        match &self.backend {
            Backend::Ram(storage) => storage.mk_loc_user(uid).await,
            Backend::Journal(storage) => storage.mk_loc_user(uid).await,
        }
    }

    async fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        match &self.backend {
            Backend::Ram(storage) => storage.mk_loc_sender(pk, uid).await,
            Backend::Journal(storage) => storage.mk_loc_sender(pk, uid).await,
        }
    }

    async fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        match &self.backend {
            Backend::Ram(storage) => storage.mk_loc_group(msg_type, group).await,
            Backend::Journal(storage) => storage.mk_loc_group(msg_type, group).await,
        }
    }

    async fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        match &self.backend {
            Backend::Ram(storage) => storage.mk_data(data_id, content).await,
            Backend::Journal(storage) => storage.mk_data(data_id, content).await,
        }
    }

    async fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        match &self.backend {
            Backend::Ram(storage) => storage.user_by_local(lid).await,
            Backend::Journal(storage) => storage.user_by_local(lid).await,
        }
    }

    async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        match &self.backend {
            Backend::Ram(storage) => storage.sender_user(sid).await,
            Backend::Journal(storage) => storage.sender_user(sid).await,
        }
    }

    async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        match &self.backend {
            Backend::Ram(storage) => storage.sender_pk(sid).await,
            Backend::Journal(storage) => storage.sender_pk(sid).await,
        }
    }

    async fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        match &self.backend {
            Backend::Ram(storage) => storage.find_data(data_id).await,
            Backend::Journal(storage) => storage.find_data(data_id).await,
        }
    }

    async fn fetch_data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        match &self.backend {
            Backend::Ram(storage) => storage.fetch_data(did).await,
            Backend::Journal(storage) => storage.fetch_data(did).await,
        }
    }

    async fn global_hash<V: GlobalHash>(&self, value: &V) -> Result<[u8; 32], GroupRouteError> {
        match &self.backend {
            Backend::Ram(storage) => storage.global_hash(value).await,
            Backend::Journal(storage) => storage.global_hash(value).await,
        }
    }

    // ── event log ─────────────────────────────────────────────────────────

    async fn store_event(&self, group: LocGroupId, ev: StoredEvent<R::Body>) -> Option<GroupDelta> {
        match &self.backend {
            Backend::Ram(storage) => storage.store_event(group, ev).await,
            Backend::Journal(storage) => storage.store_event(group, ev).await,
        }
    }

    async fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        match &self.backend {
            Backend::Ram(storage) => storage.fetch_event(group, slot).await,
            Backend::Journal(storage) => storage.fetch_event(group, slot).await,
        }
    }

    async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        match &self.backend {
            Backend::Ram(storage) => storage.diff_group(group, since).await,
            Backend::Journal(storage) => storage.diff_group(group, since).await,
        }
    }

    // ── layer 3: the gear's private view ──────────────────────────────────

    type View<'a>
        = PagedView<'a>
    where
        Self: 'a;

    async fn gear_view(&self, gear: &R::GearId) -> Self::View<'_> {
        match &self.backend {
            Backend::Ram(storage) => PagedView::Ram(storage.gear_view(gear).await),
            Backend::Journal(storage) => PagedView::Disk(storage.gear_view(gear).await),
        }
    }

    // ── durability ────────────────────────────────────────────────────────

    async fn sync_log(&self) -> io::Result<()> {
        match &self.backend {
            Backend::Ram(_) => Ok(()),
            Backend::Journal(storage) => storage.sync_log().await,
        }
    }

    async fn flush(&self) -> io::Result<()> {
        match &self.backend {
            Backend::Ram(storage) => storage.flush().await,
            Backend::Journal(storage) => storage.flush().await,
        }
    }
}
