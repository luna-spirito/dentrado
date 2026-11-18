//! The disk lifecycle shared by every on-disk layer: the lazily-opened page
//! store, the startup table GC, and the corrupt-image quarantine.
//!
//! A store error is fatal. The only recovery is the quarantine of a corrupt
//! image (renamed aside, a fresh store opens in its place) or an explicit
//! [`Store::reset`] when the image belongs to another log or layout. A failed
//! open panics rather than degrading to RAM.
//!
//! The store opens lazily on the first touch: `make_storage` runs before the
//! core's runtime exists, and opening spawns the store's driver task — both
//! need runtime context. Concurrent first-touchers park on a oneshot while one
//! of them opens.
//!
//! The disk is a type parameter so the RAM backend can run the very same code
//! over [`MemDisk`](dentrado_cache::disk::MemDisk): it gets the same tables,
//! the same GC, and the same view semantics, just without a file to survive
//! the process.

use std::{
    cell::RefCell,
    io, mem,
    path::{Path, PathBuf},
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use dentrado_cache::{Storage, StoreError, TableSpec, disk::IsFilesystem};
use futures::channel::oneshot;

/// Page-cache arena slots per core (1024 pages = 4 MiB).
const BLOCK_CACHE: usize = 1024;

/// A table-id predicate: does this layout keep the table?
pub(crate) type Keep = Rc<dyn Fn(&[u8]) -> bool>;

/// Which tables a store opens with, and which existing ones its GC keeps.
#[derive(Clone)]
pub(crate) struct Layout {
    specs: Vec<TableSpec>,
    /// A live table's id. Anything else in the store is an orphan an older
    /// build left and is dropped at rest, before the first view.
    keep: Keep,
}

impl Layout {
    pub(crate) fn new(specs: Vec<TableSpec>, keep: impl Fn(&[u8]) -> bool + 'static) -> Self {
        Self {
            specs,
            keep: Rc::new(keep),
        }
    }
}

/// The lazily-opened store, shared via `Rc` so no `RefCell` borrow is held
/// across an `.await`.
///
/// The layout is configured once, before the first [`get`](Store::get): it
/// follows from the runtime's shapes and gear kinds, not from the log.
pub(crate) struct Store<D: IsFilesystem> {
    disk: RefCell<D>,
    path: String,
    /// The image file, when the disk has one. A store without an image is
    /// wiped by replacing the disk itself.
    image: Option<PathBuf>,
    layout: RefCell<Option<Layout>>,
    state: RefCell<State<D>>,
}

enum State<D: IsFilesystem> {
    Closed,
    Opening(Vec<oneshot::Sender<()>>),
    Ready(Rc<Storage<D>>),
    /// Open failed. Every later touch panics — the process is expected to
    /// restart against a fresh store.
    Failed(String),
}

impl<D: IsFilesystem + Default + Clone> Store<D> {
    pub(crate) fn new(disk: D, path: String, image: Option<PathBuf>) -> Self {
        Self {
            disk: RefCell::new(disk),
            path,
            image,
            layout: RefCell::new(None),
            state: RefCell::new(State::Closed),
        }
    }

    /// Fix the table set before the first open.
    ///
    /// The layout is deterministic, so replacing it before an open is
    /// harmless. Once the store is open the layout is fixed in practice:
    /// every reset reopens with the same one.
    pub(crate) fn configure(&self, layout: Layout) {
        *self.layout.borrow_mut() = Some(layout);
    }

    /// The opened store. A failed open panics.
    pub(crate) async fn get(&self) -> Rc<Storage<D>> {
        loop {
            enum Step<D: IsFilesystem> {
                Ready(Rc<Storage<D>>),
                Open,
                Wait(oneshot::Receiver<()>),
                Gone(String),
            }
            let step = {
                let mut state = self.state.borrow_mut();
                match &mut *state {
                    State::Ready(store) => Step::Ready(Rc::clone(store)),
                    State::Failed(e) => Step::Gone(e.clone()),
                    State::Opening(waiters) => {
                        let (tx, rx) = oneshot::channel();
                        waiters.push(tx);
                        Step::Wait(rx)
                    }
                    State::Closed => {
                        *state = State::Opening(Vec::new());
                        Step::Open
                    }
                }
            };
            match step {
                Step::Ready(store) => return store,
                Step::Gone(e) => panic!("page store is unusable: {e}"),
                Step::Wait(rx) => {
                    let _ = rx.await;
                    // The opener either published `Ready`, published
                    // `Failed`, or (if it was cancelled) left `Closed`.
                    // All three are handled by the next loop turn.
                }
                Step::Open => {
                    // The guard returns the state to `Closed` and wakes the
                    // waiters if this future is dropped mid-open.
                    let guard = OpenGuard { store: self };
                    let layout = self
                        .layout
                        .borrow()
                        .clone()
                        .expect("the page store was opened before its layout was configured");
                    let opened = self.open(&layout).await;
                    let mut state = self.state.borrow_mut();
                    match opened {
                        Ok(store) => {
                            guard.defuse();
                            let rc = Rc::new(store);
                            let waiters = take_waiters(&mut state, State::Ready(Rc::clone(&rc)));
                            drop(state);
                            wake(waiters);
                            return rc;
                        }
                        Err(e) => {
                            guard.defuse();
                            let msg = format!("opening {} failed: {e:?}", self.path);
                            let waiters = take_waiters(&mut state, State::Failed(msg.clone()));
                            drop(state);
                            wake(waiters);
                            panic!("{msg}");
                        }
                    }
                }
            }
        }
    }

    /// Wipe the image and get ready for a fresh open. The caller must have
    /// dropped every handle it holds: a live [`Storage`] would be writing
    /// into the image being discarded.
    pub(crate) async fn reset(&self) {
        let old = {
            let mut state = self.state.borrow_mut();
            match mem::replace(&mut *state, State::Closed) {
                State::Ready(store) => store,
                _ => panic!("reset of a page store that is not open"),
            }
        };
        assert_eq!(
            Rc::strong_count(&old),
            1,
            "reset of a page store that still has live handles"
        );
        drop(old);
        match &self.image {
            Some(path) => match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => panic!("wipe {}: {e}", path.display()),
            },
            None => *self.disk.borrow_mut() = D::default(),
        }
    }

    /// Open the store, then the startup table GC: a table the layout does not
    /// ask for (an older kind set, an older layout version, a stale
    /// materialization) is an orphan an older build left — dropped at rest,
    /// before the first view.
    async fn open(&self, layout: &Layout) -> Result<Storage<D>, StoreError> {
        let disk = self.disk.borrow().clone();
        let store = match &self.image {
            Some(path) => open_quarantining(&disk, path, &self.path, &layout.specs).await?,
            None => Storage::open(&disk, &self.path, BLOCK_CACHE, &layout.specs).await?,
        };
        for name in store.tables() {
            if (layout.keep)(&name) {
                continue;
            }
            let Some(table) = store.table(&name) else {
                continue;
            };
            if let Err(e) = store.drop_table(table).await {
                log::warn!("page store: GC of table {name:?} failed: {e:?}");
            }
        }
        Ok(store)
    }
}

/// Open the store, quarantining a corrupt image: the file is renamed aside
/// (kept for inspection) and a fresh store opens in its place. A rename
/// failure falls back to the plain open — its error is fatal.
async fn open_quarantining<D: IsFilesystem>(
    disk: &D,
    file: &Path,
    path: &str,
    specs: &[TableSpec],
) -> Result<Storage<D>, StoreError> {
    match Storage::open(disk, path, BLOCK_CACHE, specs).await {
        Err(StoreError::CorruptImage) => {
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
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
            Storage::open(disk, path, BLOCK_CACHE, specs).await
        }
        other => other,
    }
}

/// Cancellation guard for the opener: if the open future is dropped before
/// publishing `Ready`/`Failed`, the slot returns to `Closed` and the waiters
/// retry instead of parking forever.
struct OpenGuard<'a, D: IsFilesystem> {
    store: &'a Store<D>,
}

impl<D: IsFilesystem> OpenGuard<'_, D> {
    fn defuse(self) {
        mem::forget(self);
    }
}

impl<D: IsFilesystem> Drop for OpenGuard<'_, D> {
    fn drop(&mut self) {
        let waiters = {
            let mut state = self.store.state.borrow_mut();
            if !matches!(*state, State::Opening(_)) {
                return;
            }
            match mem::replace(&mut *state, State::Closed) {
                State::Opening(waiters) => waiters,
                _ => unreachable!(),
            }
        };
        wake(waiters);
    }
}

fn take_waiters<D: IsFilesystem>(state: &mut State<D>, to: State<D>) -> Vec<oneshot::Sender<()>> {
    match mem::replace(state, to) {
        State::Opening(waiters) => waiters,
        _ => Vec::new(),
    }
}

fn wake(waiters: Vec<oneshot::Sender<()>>) {
    for tx in waiters {
        let _ = tx.send(());
    }
}
