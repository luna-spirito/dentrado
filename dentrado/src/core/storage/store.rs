//! The disk lifecycle shared by every on-disk layer: the lazily-opened page
//! store, the startup table GC, and the corrupt-image quarantine.
//!
//! A store error is fatal. The only recovery is the quarantine of a corrupt
//! image (renamed aside, a fresh store opens in its place); a failed open
//! panics rather than degrading to RAM.
//!
//! The store opens lazily on the first touch: `make_storage` runs before the
//! core's runtime exists, and opening spawns the store's driver task — both
//! need runtime context. Concurrent first-touchers park on a oneshot while one
//! of them opens.

use std::{
    cell::RefCell,
    mem,
    path::{Path, PathBuf},
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use dentrado_cache::{Storage, StoreError, TableSpec, disk::FsDisk};
use futures::channel::oneshot;

/// Page-cache arena slots per core (1024 pages = 4 MiB).
const BLOCK_CACHE: usize = 1024;

/// A table-id predicate: does this layout keep the table?
pub(crate) type Keep = Box<dyn Fn(&[u8]) -> bool>;

/// Which tables a store opens with, and which existing ones its GC keeps.
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
            keep: Box::new(keep),
        }
    }
}

/// The lazily-opened store, shared via `Rc` so no `RefCell` borrow is held
/// across an `.await`.
///
/// The layout is configured once, after the journal has been opened and its
/// registry fingerprint is known (the events table is keyed by it), but before
/// the first [`get`](Store::get).
pub(crate) struct Store {
    file: PathBuf,
    layout: RefCell<LayoutState>,
    state: RefCell<State>,
}

enum LayoutState {
    Unset,
    Set(Layout),
    /// Moved into an open in progress; restored if that open is cancelled.
    Taken,
}

enum State {
    Closed,
    Opening(Vec<oneshot::Sender<()>>),
    Ready(Rc<Storage<FsDisk>>),
    /// Open failed. Every later touch panics — the process is expected to
    /// restart against a fresh store.
    Failed(String),
}

impl Store {
    pub(crate) fn new(file: PathBuf) -> Self {
        Self {
            file,
            layout: RefCell::new(LayoutState::Unset),
            state: RefCell::new(State::Closed),
        }
    }

    /// Fix the table set before the first open.
    ///
    /// The layout is deterministic (it follows from the journal's declarations),
    /// so replacing it before an open consumes it is harmless: a cancelled open
    /// restores the layout and the retry configures the same one again. Once an
    /// open consumes it, the store is fixed.
    pub(crate) fn configure(&self, layout: Layout) {
        let mut slot = self.layout.borrow_mut();
        match &*slot {
            LayoutState::Unset | LayoutState::Set(_) => *slot = LayoutState::Set(layout),
            LayoutState::Taken => {
                panic!("the page store's layout was configured after its first open")
            }
        }
    }

    /// The opened store. A failed open panics.
    pub(crate) async fn get(&self) -> Rc<Storage<FsDisk>> {
        loop {
            enum Step {
                Ready(Rc<Storage<FsDisk>>),
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
                    // The layout moves into the opener and is restored if this
                    // future is dropped mid-open; the state returns to `Closed`
                    // and the waiters retry instead of parking forever.
                    let layout = self.take_layout();
                    let mut guard = OpenGuard {
                        store: self,
                        layout: Some(layout),
                    };
                    let opened = self
                        .open(guard.layout.as_ref().expect("the guard owns the layout"))
                        .await;
                    let mut state = self.state.borrow_mut();
                    match opened {
                        Ok(store) => {
                            guard.layout = None;
                            let rc = Rc::new(store);
                            let waiters = take_waiters(&mut state, State::Ready(Rc::clone(&rc)));
                            drop(state);
                            wake(waiters);
                            return rc;
                        }
                        Err(e) => {
                            guard.layout = None;
                            let msg = format!("opening {} failed: {e:?}", self.file.display());
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

    fn take_layout(&self) -> Layout {
        match mem::replace(&mut *self.layout.borrow_mut(), LayoutState::Taken) {
            LayoutState::Set(layout) => layout,
            LayoutState::Unset => {
                panic!("the page store was opened before its layout was configured")
            }
            LayoutState::Taken => unreachable!("only one opener moves the layout out"),
        }
    }

    /// Open the store, then the startup table GC: a table the layout does not
    /// ask for (an older kind set, an older layout version, a stale
    /// materialization) is an orphan an older build left — dropped at rest,
    /// before the first view.
    async fn open(&self, layout: &Layout) -> Result<Storage<FsDisk>, StoreError> {
        let store = open_quarantining(&self.file, &layout.specs).await?;
        for name in store.tables() {
            if (layout.keep)(&name) {
                continue;
            }
            if let Err(e) = store.drop_table(&name).await {
                log::warn!("page store: GC of table {name:?} failed: {e:?}");
            }
        }
        Ok(store)
    }
}

/// Open the store, quarantining a corrupt image: the file is renamed aside
/// (kept for inspection) and a fresh store opens in its place. A rename
/// failure falls back to the plain open — its error is fatal.
async fn open_quarantining(
    file: &Path,
    specs: &[TableSpec],
) -> Result<Storage<FsDisk>, StoreError> {
    let path = file.display().to_string();
    match Storage::open(&FsDisk, &path, BLOCK_CACHE, specs).await {
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
            Storage::open(&FsDisk, &path, BLOCK_CACHE, specs).await
        }
        other => other,
    }
}

/// Cancellation guard for the opener: if the open future is dropped before
/// publishing `Ready`/`Failed`, the slot returns to `Closed`, the layout is
/// restored, and the waiters retry instead of parking forever.
struct OpenGuard<'a> {
    store: &'a Store,
    layout: Option<Layout>,
}

impl Drop for OpenGuard<'_> {
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
        if let Some(layout) = self.layout.take() {
            *self.store.layout.borrow_mut() = LayoutState::Set(layout);
        }
        wake(waiters);
    }
}

fn take_waiters(state: &mut State, to: State) -> Vec<oneshot::Sender<()>> {
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
