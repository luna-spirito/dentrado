//! The disk-backed [`IsStorage`]: localization and the event log stay in
//! RAM; the gear views spill into one [`dentrado_cache`] table.
//!
//! A view is a flat key-value overlay scoped to one gear (see
//! [`GearView`]). The disk view is one `Session` over the store: reads use
//! the session's old snapshot, writes are staged by the session, and the
//! runtime commits the whole batch after the gear run. There is no image
//! envelope and no chunking here — a gear that needs bigger values stores
//! several records itself.
//!
//! ## The poison boundary
//!
//! A store error is fatal: the view panics rather than silently degrading
//! to RAM. The process is expected to restart against a fresh store (the
//! cache is recomputable). A corrupt image is quarantined (renamed aside,
//! kept for inspection) and a fresh store opens in its place; a failed
//! open is a panic, never a silent RAM fallback.
//!
//! The store opens lazily on the first view: `make_storage` runs before the
//! core's runtime exists, and opening spawns the store's driver task — both
//! need runtime context. Concurrent first-touchers park on a oneshot while
//! one of them opens.

mod disk;

use std::{
    cell::{Cell, RefCell},
    future::Future,
    io,
    path::PathBuf,
    rc::Rc,
};

use dentrado_cache::{Storage, disk::FsDisk};

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
        storage::{
            GroupDiff, IsStorage, Watermark,
            in_memory::InMemoryStorage,
            view::{DiskView, GearView, RamView, gear_key},
        },
    },
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, GroupRouteError, LocDataId,
        LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};
use disk::Disk;

/// [`IsStorage`] whose gear views live in a [`dentrado_cache`] file.
/// `ram()` is the runtime toggle: no file, views in a `BTreeMap`.
pub struct PagedStorage<R: IsRuntime> {
    ram: InMemoryStorage<R>,
    file: Option<PathBuf>,
    disk: RefCell<Disk>,
    dirty: Cell<bool>,
}

impl<R: IsRuntime> Default for PagedStorage<R> {
    fn default() -> Self {
        Self::ram()
    }
}

impl<R: IsRuntime> PagedStorage<R> {
    #[must_use]
    pub fn ram() -> Self {
        Self {
            ram: InMemoryStorage::default(),
            file: None,
            disk: RefCell::new(Disk::Closed),
            dirty: Cell::new(false),
        }
    }

    /// Disk mode: gear views live (after [`IsStorage::flush`]) in one
    /// page-store file. The parent directory must already exist; the store
    /// opens lazily inside the core's runtime on the first view.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        Self {
            file: Some(file),
            ..Self::ram()
        }
    }

    /// The opened store, or a panic. A missing file means [`Self::ram`];
    /// disk mode always has one.
    pub(super) async fn ensure(&self) -> Rc<Storage<FsDisk>> {
        self.disk_open().await.expect("open the gear view store")
    }
}

impl<R: IsRuntime> GlobalResolver for PagedStorage<R> {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        self.ram.resolve_user(lid)
    }

    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        self.ram.resolve_data(did)
    }
}

/// The two representations a [`PagedStorage`] view can take. The disk
/// branch is a `Session` + gear prefix; the RAM branch is the same
/// contract over a `BTreeMap` (the `ram()` toggle).
pub enum PagedView<'a> {
    Disk(DiskView<'a>),
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

impl<R> IsStorage<R> for PagedStorage<R>
where
    R: IsRuntime,
{
    // ── Layers 1–2: delegated to RAM ───────────────────────────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId> {
        self.ram.mk_loc_user(uid)
    }

    fn mk_loc_sender(
        &self,
        pk: SenderPk,
        uid: Option<UserId>,
    ) -> impl Future<Output = LocSenderId> {
        self.ram.mk_loc_sender(pk, uid)
    }

    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId> {
        self.ram.mk_loc_group(msg_type, group)
    }

    fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> impl Future<Output = Result<LocDataId, DataVerifyError>> {
        self.ram.mk_data(data_id, content)
    }

    fn user_by_local(&self, lid: LocUserId) -> impl Future<Output = Option<UserId>> {
        self.ram.user_by_local(lid)
    }

    fn sender_user(&self, sid: LocSenderId) -> impl Future<Output = Option<LocUserId>> {
        self.ram.sender_user(sid)
    }

    fn sender_pk(&self, sid: LocSenderId) -> impl Future<Output = Option<SenderPk>> {
        self.ram.sender_pk(sid)
    }

    fn find_data(&self, data_id: &DataId) -> impl Future<Output = Option<LocDataId>> {
        self.ram.find_data(data_id)
    }

    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<(DataId, R::Data)>> {
        self.ram.fetch_data(did)
    }

    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<StoreResultSuccess>> {
        self.ram.store_event(group, ev)
    }

    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>> {
        self.ram.fetch_event(group, slot)
    }

    fn diff_group(&self, group: LocGroupId, since: Watermark) -> impl Future<Output = GroupDiff> {
        self.ram.diff_group(group, since)
    }

    // ── Layer 3: the gear's private view ──────────────────────────────────

    type View<'a>
        = PagedView<'a>
    where
        Self: 'a;

    fn gear_view(&self, gear: &R::GearId) -> impl Future<Output = Self::View<'_>> {
        let prefix = gear_key::<R>(gear).to_vec();
        async move {
            match &self.file {
                Some(_) => {
                    let store = self.ensure().await;
                    PagedView::Disk(DiskView::new(store, prefix, &self.dirty))
                }
                None => PagedView::Ram(RamView::new(self.ram.gear_data(), prefix)),
            }
        }
    }

    // ── Durability ─────────────────────────────────────────────────────────

    fn flush(&self) -> impl Future<Output = io::Result<()>> {
        async move {
            self.ram.flush().await?;
            if self.dirty.get() {
                let store = self.ensure().await;
                store.flush().await.expect("flush the gear view store");
                self.dirty.set(false);
            }
            Ok(())
        }
    }
}
