//! Gear-cache persistence over [`dentrado_storage`] pages.
//!
//! [`PagedStorage`] wraps [`InMemoryStorage`]: localization, the event log,
//! and the dormant page substrate stay in RAM and are delegated verbatim;
//! only the gear cache (`get_cache`/`put_cache`) spills into value pages
//! through [`CacheSer::ser_image`], one B-tree entry per gear:
//!
//! * key — `blake3(serde_json(gear_id))`, 32 bytes;
//! * value — the image split into 4080-byte payload chunks, one draft page
//!   each; the entry's `mt` carries the total image length (8-byte LE), the
//!   only metadata needed to strip the zero padding on read;
//! * deletion — a tombstone entry (empty `mt`, no pages).
//!
//! The image is hashed, and an unchanged image skips the rewrite — the
//! engine puts the cache after *every* gear run, so without this an idle
//! gear would churn pages and disk forever. Reads seed the same map, so a
//! post-restart run of an unchanged gear never touches the store at all.
//! `flush` is dirty-gated the same way (an idle round still rewrites the
//! superblock and syncs).
//!
//! Every store error is logged and falls back to RAM: gear caches are
//! recomputable from the event log, so the backend degrades to
//! [`InMemoryStorage`] semantics rather than failing the core.
//!
//! The store opens lazily on the first cache op: `make_storage` runs before
//! the core's runtime exists, and opening spawns the store's driver task —
//! both need runtime context. Concurrent first-touchers park on a oneshot
//! while one of them opens; a failed open is terminal for the process
//! (RAM-only mode) — a cache backend must not brick the core by retrying a
//! broken disk on every op.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    io, mem,
    path::PathBuf,
    rc::Rc,
};

use dentrado_storage::{
    Store, StoreError,
    core::val::{VAL_PAYLOAD, val_split},
    disk::FsDisk,
};
use futures::channel::oneshot;

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
        storage::{
            AlignedPage, CacheSer, GroupDiff, PageHandle, PageId, Storage,
            in_memory::InMemoryStorage,
        },
    },
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, GroupRouteError, LocDataId,
        LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};
/// The gear-cache table (the only one this backend uses).
const TABLE: usize = 0;
/// Page-cache arena slots per core (1024 pages = 4 MiB).
const BLOCK_CACHE: usize = 1024;

/// The lazily-opened store, shared via `Rc` so no `RefCell` borrow is held
/// across an `.await`.
enum Disk {
    Closed,
    Opening(Vec<oneshot::Sender<()>>),
    Ready(Rc<Store<FsDisk>>),
    /// Open failed — RAM-only for the rest of the process.
    Failed,
}

/// [`Storage`] that persists gear caches into dentrado-storage pages.
/// `ram()` behaves exactly like [`InMemoryStorage`] — the toggle.
pub struct PagedStorage<R: IsRuntime> {
    ram: InMemoryStorage<R>,
    file: Option<PathBuf>,
    disk: RefCell<Disk>,
    dirty: Cell<bool>,
    /// Gear key → hash of the last disk-bound image. A matching hash makes
    /// `put_cache` a no-op; a `remove` means "erase on disk".
    seen: RefCell<HashMap<[u8; 32], [u8; 32]>>,
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
            seen: RefCell::new(HashMap::new()),
        }
    }

    /// Disk mode: cache images live (after [`Storage::flush`]) in one
    /// page-store file. The parent directory must already exist; the store
    /// opens lazily inside the core's runtime on the first cache op.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        Self {
            file: Some(file),
            ..Self::ram()
        }
    }

    fn key(gear: &R::GearId) -> Option<[u8; 32]>
    where
        R::GearId: serde::Serialize,
    {
        let json = serde_json::to_vec(gear).ok()?;
        Some(*blake3::hash(&json).as_bytes())
    }

    fn remember(&self, key: [u8; 32], image: &[u8]) {
        self.seen
            .borrow_mut()
            .insert(key, *blake3::hash(image).as_bytes());
    }

    /// The opened store, or `None` in RAM-only mode (`ram()`, failed open).
    async fn ensure(&self) -> Option<Rc<Store<FsDisk>>> {
        enum Step {
            Ready(Rc<Store<FsDisk>>),
            Open,
            Wait(oneshot::Receiver<()>),
            Gone,
        }
        let step = {
            let mut disk = self.disk.borrow_mut();
            match &mut *disk {
                Disk::Ready(s) => Step::Ready(Rc::clone(s)),
                Disk::Failed => Step::Gone,
                Disk::Opening(waiters) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    Step::Wait(rx)
                }
                Disk::Closed => {
                    *disk = Disk::Opening(Vec::new());
                    Step::Open
                }
            }
        };
        match step {
            Step::Ready(s) => Some(s),
            Step::Gone => None,
            Step::Wait(rx) => {
                let _ = rx.await;
                match &*self.disk.borrow() {
                    Disk::Ready(s) => Some(Rc::clone(s)),
                    _ => None,
                }
            }
            Step::Open => {
                let file = self.file.as_ref().expect("disk mode has a file");
                let opened =
                    Store::open(&FsDisk, &file.display().to_string(), BLOCK_CACHE, TABLE + 1).await;
                let mut disk = self.disk.borrow_mut();
                match opened {
                    Ok(store) => {
                        let rc = Rc::new(store);
                        let waiters = take_waiters(&mut disk, Disk::Ready(Rc::clone(&rc)));
                        drop(disk);
                        wake(waiters);
                        Some(rc)
                    }
                    Err(e) => {
                        log::warn!(
                            "page store: opening {} failed ({e:?}); gear caches stay RAM-only",
                            file.display()
                        );
                        let waiters = take_waiters(&mut disk, Disk::Failed);
                        drop(disk);
                        wake(waiters);
                        None
                    }
                }
            }
        }
    }

    /// Pages a key's live entry occupies (chain order); empty when absent.
    async fn old_pages(store: &Store<FsDisk>, key: &[u8; 32]) -> Result<Vec<u64>, StoreError> {
        match store.get_cache(TABLE, key).await? {
            Some((guard, cv)) => {
                drop(guard);
                Ok(cv.pages)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Write `image` as a fresh value chain, freeing the pages it supersedes.
    async fn publish(
        &self,
        store: &Store<FsDisk>,
        key: &[u8; 32],
        image: &[u8],
    ) -> Result<(), StoreError> {
        let old = Self::old_pages(store, key).await?;
        let mut drafts = Vec::new();
        for chunk in image.chunks(VAL_PAYLOAD) {
            let k = match store.alloc_draft_page().await {
                Ok(k) => k,
                Err(e) => return undraft(store, drafts, e).await,
            };
            drafts.push(k);
            let chunk = chunk.to_vec();
            if let Err(e) = store
                .write_draft_page(k, move |_| (chunk.clone(), ()))
                .await
            {
                return undraft(store, drafts, e).await;
            }
        }
        let mt = u64::try_from(image.len()).unwrap_or(u64::MAX).to_le_bytes();
        store.put_cache(TABLE, key, &mt, &drafts, &old).await
    }

    /// Tombstone the key and free its pages.
    async fn erase(&self, store: &Store<FsDisk>, key: &[u8; 32]) -> Result<(), StoreError> {
        let old = Self::old_pages(store, key).await?;
        store.put_cache(TABLE, key, &[], &[], &old).await
    }
}

fn take_waiters(disk: &mut Disk, to: Disk) -> Vec<oneshot::Sender<()>> {
    match mem::replace(disk, to) {
        Disk::Opening(waiters) => waiters,
        _ => Vec::new(),
    }
}

fn wake(waiters: Vec<oneshot::Sender<()>>) {
    for tx in waiters {
        let _ = tx.send(());
    }
}

/// Return allocated drafts to the pool, then surface the original error.
async fn undraft(
    store: &Store<FsDisk>,
    drafts: Vec<dentrado_storage::DraftId>,
    e: StoreError,
) -> Result<(), StoreError> {
    for k in drafts {
        let _ = store.dealloc_draft_page(k).await;
    }
    Err(e)
}

impl<R: IsRuntime> GlobalResolver for PagedStorage<R> {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        self.ram.resolve_user(lid)
    }

    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        self.ram.resolve_data(did)
    }
}

impl<R> Storage<R> for PagedStorage<R>
where
    R: IsRuntime,
    R::GearId: serde::Serialize,
    R::GearCache<(usize, usize)>: CacheSer,
{
    type Watermark = (usize, usize);

    // ── Layers 1–2 + pages: delegated to RAM ───────────────────────────────

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

    fn diff_group(
        &self,
        group: LocGroupId,
        since: Self::Watermark,
    ) -> impl Future<Output = GroupDiff<Self::Watermark>> {
        self.ram.diff_group(group, since)
    }

    fn write_page(&self, data: AlignedPage, refs: &[PageId]) -> impl Future<Output = PageHandle> {
        self.ram.write_page(data, refs)
    }

    fn read_page(&self, id: PageId) -> impl Future<Output = AlignedPage> {
        self.ram.read_page(id)
    }

    fn drop_page_handle(&self, handle: PageHandle) -> impl Future<Output = ()> {
        self.ram.drop_page_handle(handle)
    }

    // ── Layer 3: gear cache → pages ────────────────────────────────────────

    fn get_cache(
        &self,
        gear: &R::GearId,
    ) -> impl Future<Output = Option<R::GearCache<Self::Watermark>>> {
        async move {
            let Some(file) = &self.file else {
                return self.ram.get_cache(gear).await;
            };
            let Some(key) = Self::key(gear) else {
                return self.ram.get_cache(gear).await;
            };
            let Some(store) = self.ensure().await else {
                return self.ram.get_cache(gear).await;
            };
            let entry = match store.get_cache(TABLE, &key).await {
                Ok(entry) => entry,
                Err(e) => {
                    log::warn!("page store: read {}: {e:?}", file.display());
                    return self.ram.get_cache(gear).await;
                }
            };
            let Some((guard, cv)) = entry else {
                return self.ram.get_cache(gear).await;
            };
            // Tombstones carry no `mt`; a foreign length can only be
            // corruption — both mean "absent".
            let Ok(mt) = <[u8; 8]>::try_from(cv.mt.as_slice()) else {
                return None;
            };
            let len = u64::from_le_bytes(mt) as usize;
            if len > cv.pages.len() * VAL_PAYLOAD {
                return None;
            }
            let mut image = Vec::with_capacity(len);
            for p in &cv.pages {
                match store
                    .read_page_guarded(&guard, *p, |pg| val_split(pg).1.to_vec())
                    .await
                {
                    Ok(chunk) => image.extend_from_slice(&chunk),
                    Err(e) => {
                        log::warn!("page store: chain read {}: {e:?}", file.display());
                        return None; // half an image is worse than a cold start
                    }
                }
            }
            drop(guard);
            image.truncate(len);
            let mut cache = R::make_cache(gear);
            if !cache.de_image(&image) {
                return None;
            }
            self.remember(key, &image);
            Some(cache)
        }
    }

    fn put_cache(
        &self,
        gear: R::GearId,
        cache: R::GearCache<Self::Watermark>,
    ) -> impl Future<Output = ()> {
        async move {
            let Some(file) = &self.file else {
                return self.ram.put_cache(gear, cache).await;
            };
            let Some(key) = Self::key(&gear) else {
                return self.ram.put_cache(gear, cache).await;
            };
            match cache.ser_image() {
                Some(image) => {
                    let hash = *blake3::hash(&image).as_bytes();
                    if self.seen.borrow().get(&key) == Some(&hash) {
                        return; // unchanged since the last disk-bound image
                    }
                    let Some(store) = self.ensure().await else {
                        return self.ram.put_cache(gear, cache).await;
                    };
                    match self.publish(&store, &key, &image).await {
                        Ok(()) => {
                            self.dirty.set(true);
                            self.seen.borrow_mut().insert(key, hash);
                        }
                        Err(e) => {
                            log::warn!("page store: write {}: {e:?}", file.display());
                            self.ram.put_cache(gear, cache).await;
                        }
                    }
                }
                // A non-persisting cache erases only what this process (or a
                // same-shape predecessor) stored; a stale entry left by older
                // code is unread (`de_image` fails) and dies at the next
                // overwrite.
                None => {
                    if self.seen.borrow_mut().remove(&key).is_some() {
                        if let Some(store) = self.ensure().await {
                            match self.erase(&store, &key).await {
                                Ok(()) => {
                                    self.dirty.set(true);
                                }
                                Err(e) => {
                                    log::warn!("page store: erase {}: {e:?}", file.display());
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ── Durability ─────────────────────────────────────────────────────────

    fn flush(&self) -> impl Future<Output = io::Result<()>> {
        async move {
            self.ram.flush().await?;
            if !self.dirty.get() {
                return Ok(());
            }
            let Some(store) = self.ensure().await else {
                return Ok(());
            };
            store
                .flush()
                .await
                .map_err(|e| io::Error::other(format!("page store flush: {e:?}")))?;
            self.dirty.set(false);
            Ok(())
        }
    }
}
