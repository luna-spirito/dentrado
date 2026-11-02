//! Gear-cache persistence over [`dentrado_cache`].
//!
//! [`PagedStorage`] wraps [`InMemoryStorage`]: localization and the event log
//! stay in RAM and are delegated verbatim; only the gear cache
//! (`get_cache`/`put_cache`) spills into the store's named table — one entry
//! per gear:
//!
//! * table — `cache:v<N>`, the cache format version in the name (a format
//!   change opens a new table; the opener's startup GC drops every other
//!   name this file carries — an old version's orphans);
//! * key — `postcard(gear_id)`;
//! * value — the [`CacheSer`] image split into 4080-byte payload chunks, one
//!   draft page each; the entry's `mt` carries the total image length (8-byte
//!   LE), the only metadata needed to strip the zero padding on read;
//! * deletion — a tombstone entry (empty `mt`, no pages).
//!
//! The image is hashed, and an unchanged image skips the rewrite — the
//! engine puts the cache after *every* gear run, so without this an idle
//! gear would churn pages and disk forever. Reads seed the same map, so a
//! post-restart run of an unchanged gear never touches the store at all.
//! `flush` is dirty-gated the same way (an idle round still rewrites the
//! superblock and syncs).
//!
//! ## The poison boundary
//!
//! The store is a component the core cannot trust: any error, and any panic
//! the store does not already contain itself, fails the disk **permanently**
//! — the backend degrades to [`InMemoryStorage`] semantics for the rest of
//! the process rather than failing the core. Gear caches are recomputable
//! from the event log, so the degradation is silent to correctness (an idle
//! gear re-warms on its next run). A corrupt image at open is quarantined
//! (the file renamed aside, kept for inspection) and a fresh store opens in
//! its place — corruption must neither crashloop the process nor be
//! silently rewritten. The disk lifecycle ([`Disk`], open, quarantine, the
//! boundary) is [`disk`]'s; this module is the [`IsStorage`] contract.
//!
//! The store opens lazily on the first cache op: `make_storage` runs before
//! the core's runtime exists, and opening spawns the store's driver task —
//! both need runtime context. Concurrent first-touchers park on a oneshot
//! while one of them opens; a failed open is terminal for the process
//! (RAM-only mode) — a cache backend must not brick the core by retrying a
//! broken disk on every op.

mod disk;

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    future::Future,
    io,
    path::PathBuf,
};

use dentrado_cache::core::val::{VAL_PAYLOAD, val_split};

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
        storage::{CacheSer, GroupDiff, IsStorage, Watermark, in_memory::InMemoryStorage},
    },
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, GroupRouteError, LocDataId,
        LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};
use disk::{CACHE_TABLE, Disk};

/// [`Storage`] that persists gear caches into a dentrado-cache table.
/// `ram()` behaves exactly like [`InMemoryStorage`] — the toggle.
pub struct PagedStorage<R: IsRuntime> {
    ram: InMemoryStorage<R>,
    file: Option<PathBuf>,
    disk: RefCell<Disk>,
    dirty: Cell<bool>,
    /// Gear key → hash of the last disk-bound image. A matching hash makes
    /// `put_cache` a no-op; a `remove` means "erase on disk".
    seen: RefCell<HashMap<Vec<u8>, [u8; 32]>>,
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

    /// Disk mode: cache images live (after [`IsStorage::flush`]) in one
    /// page-store file. The parent directory must already exist; the store
    /// opens lazily inside the core's runtime on the first cache op.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        Self {
            file: Some(file),
            ..Self::ram()
        }
    }

    fn key(gear: &R::GearId) -> Option<Vec<u8>>
    where
        R::GearId: serde::Serialize,
    {
        postcard::to_stdvec(gear).ok()
    }

    fn remember(&self, key: &[u8], image: &[u8]) {
        self.seen
            .borrow_mut()
            .insert(key.to_vec(), *blake3::hash(image).as_bytes());
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

impl<R> IsStorage<R> for PagedStorage<R>
where
    R: IsRuntime,
    R::GearId: serde::Serialize,
    R::GearCache: CacheSer,
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

    // ── Layer 3: gear cache → pages ────────────────────────────────────────

    fn get_cache(&self, gear: &R::GearId) -> impl Future<Output = Option<R::GearCache>> {
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
            let read = async {
                let Some((guard, cv)) = store.get_cache(CACHE_TABLE, &key).await? else {
                    return Ok(None);
                };
                // Tombstones carry no `mt`; a foreign length can only be
                // corruption — both mean "absent".
                let Ok(mt) = <[u8; 8]>::try_from(cv.mt.as_slice()) else {
                    return Ok(None);
                };
                let len = u64::from_le_bytes(mt) as usize;
                if len > cv.pages.len() * VAL_PAYLOAD {
                    return Ok(None);
                }
                let mut image = Vec::with_capacity(len);
                for p in &cv.pages {
                    match store
                        .read_page_guarded(&guard, *p, |pg| val_split(pg).1.to_vec())
                        .await
                    {
                        Ok(chunk) => image.extend_from_slice(&chunk),
                        Err(e) => {
                            // half an image is worse than a cold start
                            return Err(e);
                        }
                    }
                }
                drop(guard);
                image.truncate(len);
                Ok(Some(image))
            };
            // A read failure degrades but needs no tombstone: nothing was
            // written, the entry on disk stays whatever it was.
            let Some(image) = self.boundary(file, read).await? else {
                return self.ram.get_cache(gear).await;
            };
            let mut cache = R::make_cache(gear);
            if !cache.de_image(&image) {
                return None;
            }
            self.remember(&key, &image);
            Some(cache)
        }
    }

    fn put_cache(&self, gear: R::GearId, cache: R::GearCache) -> impl Future<Output = ()> {
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
                    if self
                        .boundary(file, Self::publish(&store, &key, &image))
                        .await
                        .is_some()
                    {
                        self.dirty.set(true);
                        self.seen.borrow_mut().insert(key, hash);
                    }
                }
                // A non-persisting cache erases only what this process (or a
                // same-shape predecessor) stored; a stale entry left by older
                // code is unread (`de_image` fails) and dies at the next
                // overwrite.
                None => {
                    if self.seen.borrow_mut().remove(&key).is_some() {
                        if let Some(store) = self.ensure().await {
                            if self
                                .boundary(file, Self::erase(&store, &key))
                                .await
                                .is_some()
                            {
                                self.dirty.set(true);
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
