//! The disk lifecycle: the lazily-opened store, the startup version GC,
//! the corrupt-image quarantine, and the poison boundary around every disk
//! op. See [`super`] for the storage contract this serves.

use std::{
    future::Future,
    mem,
    panic::AssertUnwindSafe,
    path::Path,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use dentrado_cache::{Storage, StoreError, core::val::VAL_PAYLOAD, disk::FsDisk};
use futures::{FutureExt, channel::oneshot};

use super::PagedStorage;
use crate::core::gear::IsRuntime;

/// The gear-cache table: the cache format version lives in the name — a new
/// format opens a new table, and the startup GC (below) drops every other
/// name the file carries.
pub(super) const CACHE_TABLE: &str = "cache:v1";
/// Page-cache arena slots per core (1024 pages = 4 MiB).
const BLOCK_CACHE: usize = 1024;

/// The lazily-opened store, shared via `Rc` so no `RefCell` borrow is held
/// across an `.await`.
pub(super) enum Disk {
    Closed,
    Opening(Vec<oneshot::Sender<()>>),
    Ready(Rc<Storage<FsDisk>>),
    /// Open failed, or a later op errored/panicked — RAM-only for the rest
    /// of the process.
    Failed,
}

impl<R: IsRuntime> PagedStorage<R> {
    /// Fail the disk permanently — see the module doc's poison boundary.
    pub(super) fn fail(&self) {
        *self.disk.borrow_mut() = Disk::Failed;
    }

    /// The opened store, or `None` in RAM-only mode (`ram()`, failed open,
    /// poisoned disk).
    pub(super) async fn ensure(&self) -> Option<Rc<Storage<FsDisk>>> {
        enum Step {
            Ready(Rc<Storage<FsDisk>>),
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
                let file = self.file.as_ref().expect("disk mode has a file").clone();
                let opened = self.open(&file).await;
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

    /// Open the store (see [`open_quarantining`]), then the version GC:
    /// this file holds only cache tables, so every name that is not the
    /// current version's is an orphan an older build left — dropped here,
    /// at rest, before the first cache op (and the drop marked dirty, so
    /// the next `flush` engraves the tombstone into the image).
    async fn open(&self, file: &Path) -> Result<Storage<FsDisk>, StoreError> {
        let store = open_quarantining(file).await?;
        for name in store.tables() {
            if name != CACHE_TABLE {
                // A GC drop failing is an inconvenience, not a boundary
                // trip: the orphan stays (unread, its pages paid for) until
                // a future start retries.
                match store.drop_table(&name).await {
                    Ok(()) => self.dirty.set(true),
                    Err(e) => log::warn!("page store: GC of table {name:?} failed: {e:?}"),
                }
            }
        }
        Ok(store)
    }

    /// The poison boundary around one disk op: any error, and any panic
    /// the store did not contain itself, fails the disk permanently — the
    /// backend degrades to RAM. The op's store handle (and any sibling's)
    /// keeps the store alive until its in-flight ops end; the handle here
    /// is dropped, never inspected again.
    pub(super) async fn boundary<T>(
        &self,
        file: &Path,
        op: impl Future<Output = Result<T, StoreError>>,
    ) -> Option<T> {
        match AssertUnwindSafe(op).catch_unwind().await {
            Ok(Ok(t)) => Some(t),
            Ok(Err(e)) => {
                log::warn!(
                    "page store: op on {} failed ({e:?}); degrading to RAM",
                    file.display()
                );
                self.fail();
                None
            }
            Err(_) => {
                log::error!(
                    "page store: op on {} panicked; degrading to RAM",
                    file.display()
                );
                self.fail();
                None
            }
        }
    }

    /// Pages a key's live entry occupies (chain order); empty when absent.
    pub(super) async fn old_pages(
        store: &Storage<FsDisk>,
        key: &[u8],
    ) -> Result<Vec<u64>, StoreError> {
        match store.get_cache(CACHE_TABLE, key).await? {
            Some((guard, cv)) => {
                drop(guard);
                Ok(cv.pages)
            }
            None => Ok(Vec::new()),
        }
    }

    /// Write `image` as a fresh value chain, freeing the pages it supersedes.
    pub(super) async fn publish(
        store: &Storage<FsDisk>,
        key: &[u8],
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
        store.put_cache(CACHE_TABLE, key, &mt, &drafts, &old).await
    }

    /// Tombstone the key and free its pages.
    pub(super) async fn erase(store: &Storage<FsDisk>, key: &[u8]) -> Result<(), StoreError> {
        let old = Self::old_pages(store, key).await?;
        store.put_cache(CACHE_TABLE, key, &[], &[], &old).await
    }
}

/// Open the store, quarantining a corrupt image: the file is renamed aside
/// (kept for inspection) and a fresh store opens in its place. A rename
/// failure falls back to the plain open — its error decides RAM-only mode.
async fn open_quarantining(file: &Path) -> Result<Storage<FsDisk>, StoreError> {
    let path = file.display().to_string();
    match Storage::open(&FsDisk, &path, BLOCK_CACHE, &[CACHE_TABLE]).await {
        Err(StoreError::CorruptImage) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let aside = file.with_file_name(format!("{}.corrupt-{stamp}", file.display()));
            match std::fs::rename(file, &aside) {
                Ok(()) => {
                    log::warn!(
                        "page store: {} was corrupt; quarantined as {}",
                        file.display(),
                        aside.display()
                    );
                }
                Err(e) => {
                    log::warn!(
                        "page store: {} was corrupt, quarantine rename failed: {e}",
                        file.display()
                    );
                }
            }
            Storage::open(&FsDisk, &path, BLOCK_CACHE, &[CACHE_TABLE]).await
        }
        other => other,
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
    store: &Storage<FsDisk>,
    drafts: Vec<dentrado_cache::DraftId>,
    e: StoreError,
) -> Result<(), StoreError> {
    for k in drafts {
        let _ = store.dealloc_draft_page(k).await;
    }
    Err(e)
}
