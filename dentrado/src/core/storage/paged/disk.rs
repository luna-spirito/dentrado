//! The disk lifecycle: the lazily-opened store, the startup table GC, the
//! corrupt-image quarantine. See [`super`] for the storage contract this
//! serves.
//!
//! A store error is fatal. The only recovery is the quarantine of a
//! corrupt image (renamed aside, a fresh store opens in its place); a
//! failed open panics rather than degrading to RAM.

use std::{
    future::Future,
    mem,
    path::Path,
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use dentrado_cache::{Storage, StoreError, disk::FsDisk};
use futures::channel::oneshot;

use super::PagedStorage;
use crate::core::{
    api::RuntimeTypes,
    storage::view::{gear_spec, gear_table},
};

/// Page-cache arena slots per core (1024 pages = 4 MiB).
const BLOCK_CACHE: usize = 1024;

/// The lazily-opened store, shared via `Rc` so no `RefCell` borrow is held
/// across an `.await`.
pub(super) enum Disk {
    Closed,
    Opening(Vec<oneshot::Sender<()>>),
    Ready(Rc<Storage<FsDisk>>),
    /// Open failed. Every later touch panics — the process is expected to
    /// restart against a fresh store.
    Failed(String),
}

impl<R: RuntimeTypes> PagedStorage<R> {
    /// The opened store, or `None` in RAM mode (`ram()`). A failed open
    /// panics.
    pub(super) fn disk_open(&self) -> impl Future<Output = Option<Rc<Storage<FsDisk>>>> {
        async move {
            let file = self.file.as_deref()?;
            loop {
                enum Step {
                    Ready(Rc<Storage<FsDisk>>),
                    Open,
                    Wait(oneshot::Receiver<()>),
                    Gone(String),
                }
                let step = {
                    let mut disk = self.disk.borrow_mut();
                    match &mut *disk {
                        Disk::Ready(s) => Step::Ready(Rc::clone(s)),
                        Disk::Failed(e) => Step::Gone(e.clone()),
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
                    Step::Ready(s) => return Some(s),
                    Step::Gone(e) => panic!("gear view store is unusable: {e}"),
                    Step::Wait(rx) => {
                        let _ = rx.await;
                        // The opener either published `Ready`, published
                        // `Failed`, or (if it was cancelled) left `Closed`.
                        // All three are handled by the next loop turn.
                    }
                    Step::Open => {
                        // If this future is dropped mid-open, hand the
                        // `Opening` slot back to `Closed` and wake the
                        // waiters so one of them retries.
                        let _guard = OpenGuard(self);
                        let opened = self.open(file).await;
                        let mut disk = self.disk.borrow_mut();
                        match opened {
                            Ok(store) => {
                                let rc = Rc::new(store);
                                let waiters = take_waiters(&mut disk, Disk::Ready(Rc::clone(&rc)));
                                drop(disk);
                                wake(waiters);
                                return Some(rc);
                            }
                            Err(e) => {
                                let msg = format!("opening {} failed: {e:?}", file.display());
                                let waiters = take_waiters(&mut disk, Disk::Failed(msg.clone()));
                                drop(disk);
                                wake(waiters);
                                panic!("{msg}");
                            }
                        }
                    }
                }
            }
        }
    }

    /// Open the store, then the startup table GC: this file holds only
    /// gear-view tables, so every identity that is not the current one is
    /// an orphan an older build left — dropped at rest, before the first
    /// view (and the drop marked dirty, so the next `flush` engraves it
    /// into the image).
    async fn open(&self, file: &Path) -> Result<Storage<FsDisk>, StoreError> {
        let store = open_quarantining(file).await?;
        let id = gear_table();
        for name in store.tables() {
            if name != id {
                match store.drop_table(&name).await {
                    Ok(()) => self.dirty.set(true),
                    Err(e) => log::warn!("page store: GC of table {name:?} failed: {e:?}"),
                }
            }
        }
        Ok(store)
    }
}

/// Open the store, quarantining a corrupt image: the file is renamed aside
/// (kept for inspection) and a fresh store opens in its place. A rename
/// failure falls back to the plain open — its error is fatal.
async fn open_quarantining(file: &Path) -> Result<Storage<FsDisk>, StoreError> {
    let path = file.display().to_string();
    match Storage::open(&FsDisk, &path, BLOCK_CACHE, &[gear_spec()]).await {
        Err(StoreError::CorruptImage) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            let aside = file.with_file_name(format!("{name}.corrupt-{stamp}"));
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
            Storage::open(&FsDisk, &path, BLOCK_CACHE, &[gear_spec()]).await
        }
        other => other,
    }
}

/// Cancellation guard for the opener: if the open future is dropped before
/// publishing `Ready`/`Failed`, the slot returns to `Closed` and the
/// waiters retry instead of parking forever.
struct OpenGuard<'a, R: RuntimeTypes>(&'a PagedStorage<R>);

impl<R: RuntimeTypes> Drop for OpenGuard<'_, R> {
    fn drop(&mut self) {
        let waiters = {
            let mut disk = self.0.disk.borrow_mut();
            if !matches!(*disk, Disk::Opening(_)) {
                return;
            }
            match mem::replace(&mut *disk, Disk::Closed) {
                Disk::Opening(waiters) => waiters,
                _ => unreachable!(),
            }
        };
        wake(waiters);
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
