//! The backend handle: lazy open, the startup replay, and the operation gate.
//!
//! Opening is lazy because `make_storage` runs before the core's runtime
//! exists. A failed open panics on every later call: there is no silent
//! degradation to RAM.
//!
//! ## Why an async mutex on a `!Sync` type
//!
//! `CacheStorage` never leaves its core's thread, so a thread lock would be
//! the wrong tool. [`Mutex`] here is `futures::lock::Mutex`, an *async* one:
//! it parks the competing future, not the thread. Interleaving on one thread
//! is nevertheless real — every gear run is a spawned future, and rendering
//! the corpus does contend for this gate. What must not interleave is a
//! single logical operation: an allocation reads the shared counter, appends
//! to the journal, then commits the counter and the new mapping. Two
//! interleaved allocations would mint the same loc id, and the cache's
//! sessions have no read-your-writes, so the read and the commit cannot be
//! folded into one session. The gate spans them, which is also what makes an
//! operation's journal append and its cache write observable in that order to
//! replay.

use std::{
    io,
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};

use dentrado_cache::disk::{FsDisk, IsFilesystem, MemDisk};
use dentrado_journal::{Cursor, Journal};
use futures::lock::{Mutex, MutexGuard};

use super::{
    live::{JournalState, Ready},
    sink::CacheSink,
    tables::{self, MetaState, Tables},
};
use crate::core::{
    api::RuntimeTypes,
    storage::{
        journal::{replay, schema},
        store::Store,
    },
};

/// The non-durable backend: the same cache over memory.
pub type InMemoryStorage<R> = CacheStorage<R, MemDisk>;

/// The cache-backed layers 1–3, optionally journaled.
pub struct CacheStorage<R: RuntimeTypes, D: IsFilesystem> {
    store: Rc<Store<D>>,
    journal: Option<PathBuf>,
    /// The operation gate — async, not a thread lock; see the module docs.
    state: Mutex<State<R, D>>,
}

pub(super) struct State<R: RuntimeTypes, D: IsFilesystem> {
    pub(super) ready: Option<Ready<R, D>>,
    failed: Option<String>,
}

impl<R: RuntimeTypes> CacheStorage<R, FsDisk> {
    /// Disk mode. The journal directory is the cache path with its extension
    /// replaced by `journal`; both open on first use.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        let dir = file.with_extension("journal");
        let path = file.display().to_string();
        let store = Rc::new(Store::new(FsDisk, path, Some(file)));
        Self {
            store,
            journal: Some(dir),
            state: Mutex::new(State {
                ready: None,
                failed: None,
            }),
        }
    }
}

impl<R: RuntimeTypes> CacheStorage<R, MemDisk> {
    /// The non-durable backend: a fresh memory disk, no journal.
    #[must_use]
    pub fn new() -> Self {
        let store = Rc::new(Store::new(
            MemDisk::default(),
            "dentrado-memory".to_owned(),
            None,
        ));
        Self {
            store,
            journal: None,
            state: Mutex::new(State {
                ready: None,
                failed: None,
            }),
        }
    }
}

impl<R: RuntimeTypes> Default for CacheStorage<R, MemDisk> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: RuntimeTypes, D: IsFilesystem + Default + Clone> CacheStorage<R, D> {
    /// Take the operation gate, opening the backend on first use. A failed
    /// open panics on every later call.
    pub(super) async fn ready(&self) -> MutexGuard<'_, State<R, D>> {
        let mut state = self.state.lock().await;
        if state.ready.is_none() && state.failed.is_none() {
            match self.open().await {
                Ok(ready) => state.ready = Some(ready),
                Err(e) => state.failed = Some(e),
            }
        }
        if let Some(e) = &state.failed {
            log::error!("storage is unusable: {e}");
            panic!("storage is unusable: {e}");
        }
        state
    }

    /// The raw open, exposed to the module so the backend tests can assert on
    /// failures. Normal callers go through [`CacheStorage::ready`].
    pub(super) async fn open(&self) -> Result<Ready<R, D>, String> {
        let fingerprint = tables::fingerprint::<R>();
        let journal = match &self.journal {
            Some(dir) => Some(open_journal::<R>(dir).await?),
            None => None,
        };

        let tables = Tables::new();
        self.store.configure(tables::layout::<R>(&tables));
        let mut cache = self.store.get().await;
        let identity = journal.as_ref().map_or([0; 16], |j| j.identity);
        let head = journal.as_ref().map_or(Cursor::START, |j| j.log.head());
        let mut meta = tables::read_meta(&cache, &tables, identity, fingerprint).await;
        if meta == MetaState::Stale || meta.start() > head {
            if meta == MetaState::Stale {
                log::warn!("cache belongs to another log or runtime; refilling from the journal");
            } else {
                log::warn!("cache is applied past the journal head; refilling from the journal");
            }
            drop(cache);
            self.store.reset().await;
            cache = self.store.get().await;
            meta = MetaState::Fresh;
        }
        let valid = matches!(meta, MetaState::Valid(_));
        let from = meta.start();

        // Only the tail past the image's cursor is visited; a warm image
        // visits none.
        let replay_started = Instant::now();
        let mut sink = CacheSink {
            ops: super::ops::Ops::<R, D>::new(&cache, &tables),
            replayed: 0,
        };
        if let Some(journal) = &journal {
            replay::replay::<R, _>(&journal.log, &journal.schemas, from, &mut sink)
                .await
                .map_err(|e| format!("replay: {e}"))?;
        }
        let replayed = sink.replayed;
        drop(sink);
        let elapsed = replay_started.elapsed();

        if !valid || from != head {
            let mut session = cache.session();
            tables::write_meta(&mut session, &tables, identity, fingerprint, head)
                .map_err(|e| format!("stage cache meta: {e:?}"))?;
            session
                .commit()
                .map_err(|e| format!("commit cache meta: {e:?}"))?;
            if journal.is_some() {
                cache
                    .flush()
                    .await
                    .map_err(|e| format!("flush the materialized cache: {e:?}"))?;
            }
        }

        let ready = Ready {
            cache,
            tables,
            fingerprint,
            journal,
            replayed,
            _r: std::marker::PhantomData,
        };

        log::info!(
            "storage: head {head}, {} records replayed from {from} in {elapsed:?}",
            ready.replayed,
        );

        Ok(ready)
    }

    #[cfg(test)]
    pub(crate) async fn replayed(&self) -> usize {
        self.ready().await.ready.as_ref().expect("opened").replayed
    }
}

/// Open the log and intern this build's record schemas.
async fn open_journal<R: RuntimeTypes>(dir: &Path) -> Result<JournalState, String> {
    let mut log = Journal::open(dir)
        .await
        .map_err(|e| format!("open {}: {e}", dir.display()))?;
    if log.truncated() > 0 {
        log::warn!(
            "journal {}: repaired {} bytes of torn tail",
            dir.display(),
            log.truncated()
        );
    }
    let identity = log
        .identity()
        .await
        .map_err(|e| format!("identity of {}: {e}", dir.display()))?;
    let schemas = schema::Schemas::declare::<R>(&mut log)
        .await
        .map_err(|e| format!("declare schemas in {}: {e}", dir.display()))?;
    Ok(JournalState {
        log,
        schemas,
        identity,
    })
}

/// The durability half, kept here so the trait impl in [`super::backend`] stays
/// a list of delegations.
pub(super) async fn flush<R: RuntimeTypes, D: IsFilesystem>(
    ready: &mut Ready<R, D>,
) -> io::Result<()> {
    if ready.journal.is_none() {
        return Ok(());
    }
    ready.commit_log().await?;
    ready
        .cache
        .flush()
        .await
        .map_err(|e| io::Error::other(format!("flush the cache: {e:?}")))
}
