//! The backend handle: lazy open, the startup replay, and the operation gate.
//!
//! Opening is lazy because `make_storage` runs before the core's runtime
//! exists. A failed open panics on every later call: there is no silent
//! degradation to RAM.
//!
//! ## One type, two disks
//!
//! `CacheStorage<R>` is the one storage type the engine, the gears, and the
//! tests all name. The disk is a runtime choice (`ram()` or `on_disk(path)`),
//! erased at this boundary: [`State`] is the existential
//! `Σ D ∈ {MemDisk, FsDisk}. Ready<R, D>`. Keeping `D` in the type instead
//! would push it through `Core`, `GearCtx`, every gear signature, and every
//! view — the generic switch this type exists to remove. The dispatch costs
//! one `match` per logical storage operation, not per page.
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
    marker::PhantomData,
    path::{Path, PathBuf},
    rc::Rc,
    time::Instant,
};

use dentrado_cache::disk::{FsDisk, IsFilesystem, MemDisk};
use dentrado_journal::{Cursor, Journal};
use futures::lock::{Mutex, MutexGuard};

use super::{
    live::{JournalState, Ready},
    replay,
    tables::{self, MetaState, Tables},
};
use crate::{
    core::{
        api::{GearTable, RuntimeTypes},
        loc_ctx::{GroupDelta, StoredEvent},
        storage::{
            GroupDiff, Watermark,
            journal::schema,
            store::Store,
            view::{GearView, gear_key},
        },
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// The cache-backed layers 1–3 over either disk.
pub struct CacheStorage<R: RuntimeTypes> {
    /// The operation gate — async, not a thread lock; see the module docs.
    state: Mutex<State<R>>,
}

/// The backend's life: not opened yet, one of the two opened disks, or failed.
pub(super) enum State<R: RuntimeTypes> {
    Ram {
        store: Rc<Store<MemDisk>>,
        ready: Option<Ready<R, MemDisk>>,
    },
    Fs {
        store: Rc<Store<FsDisk>>,
        journal: PathBuf,
        ready: Option<Ready<R, FsDisk>>,
    },
    Failed(String),
}

/// Open an unopened `state` in place; a failed open is returned to the
/// caller, which decides whether to poison the handle.
async fn open_state<R: RuntimeTypes>(state: &mut State<R>) -> Result<(), String> {
    match state {
        State::Ram {
            store,
            ready: slot @ None,
        } => *slot = Some(open::<R, MemDisk>(store, None).await?),
        State::Fs {
            store,
            journal,
            ready: slot @ None,
        } => *slot = Some(open::<R, FsDisk>(store, Some(journal)).await?),
        State::Failed(e) => return Err(e.clone()),
        _ => {}
    }
    Ok(())
}

/// Run `$call` against whichever disk is open. Or-patterns cannot bind
/// differently-typed `Ready`s, so each arm repeats the call.
macro_rules! on_ready {
    ($state:expr, |$ready:ident| $call:expr) => {
        match &mut *$state {
            State::Ram {
                ready: Some($ready),
                ..
            } => $call,
            State::Fs {
                ready: Some($ready),
                ..
            } => $call,
            _ => unreachable!("ready() returns an opened state"),
        }
    };
}

impl<R: RuntimeTypes> CacheStorage<R> {
    /// The non-durable backend: a fresh memory disk, no journal.
    #[must_use]
    pub fn ram() -> Self {
        let store = Rc::new(Store::new(
            MemDisk::default(),
            "dentrado-memory".to_owned(),
            None,
        ));
        Self {
            state: Mutex::new(State::Ram { store, ready: None }),
        }
    }

    /// Disk mode. The journal directory is the cache path with its extension
    /// replaced by `journal`; both open on first use.
    #[must_use]
    pub fn on_disk(file: PathBuf) -> Self {
        let journal = file.with_extension("journal");
        let path = file.display().to_string();
        let store = Rc::new(Store::new(FsDisk, path, Some(file)));
        Self {
            state: Mutex::new(State::Fs {
                store,
                journal,
                ready: None,
            }),
        }
    }

    /// Take the operation gate, opening the backend on first use. A failed
    /// open panics on every later call.
    pub(super) async fn ready(&self) -> MutexGuard<'_, State<R>> {
        let mut state = self.state.lock().await;
        if let Err(e) = open_state(&mut state).await {
            log::error!("storage is unusable: {e}");
            *state = State::Failed(e.clone());
            panic!("storage is unusable: {e}");
        }
        state
    }

    /// The raw open, for the backend tests: unlike [`ready`](Self::ready), a
    /// failure is returned instead of poisoning the handle.
    #[cfg(test)]
    pub(super) async fn open(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        open_state(&mut state).await
    }

    // ── layer 1: localization, idempotent allocators ──────────────────────

    /// Intern a user identity, returning its existing local id when known.
    pub async fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ensure_user(uid).await)
    }

    /// Intern a sender; with a `uid` the user is interned too.
    pub async fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ensure_sender(pk, uid).await)
    }

    /// Intern a `(message type, group)` pair, returning its existing local id
    /// when known.
    pub async fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ensure_group(msg_type, group).await)
    }

    /// Intern content-addressed data; `data_id.hash` is verified against
    /// `R::Data::global_hash` before anything is written.
    ///
    /// # Errors
    ///
    /// [`DataVerifyError`] if the hash does not match the content.
    pub async fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ensure_data(data_id, content).await)
    }

    // ── layer 1: reverse lookups ──────────────────────────────────────────

    /// The identity behind a local user id.
    pub async fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().loc_user(lid).await)
    }

    /// The sender's local user, when it has one.
    pub async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s
            .ops()
            .loc_sender(sid)
            .await
            .and_then(|(_, u)| u))
    }

    /// The sender's public key.
    pub async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().loc_sender(sid).await.map(|(pk, _)| pk))
    }

    /// The local id content-addressed data is already interned under.
    pub async fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().data_loc(*data_id).await)
    }

    /// The content address and value behind a local data id.
    pub async fn fetch_data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().loc_data(did).await)
    }

    /// The global content hash of a value, resolving embedded loc ids through
    /// this backend's localization maps.
    ///
    /// # Errors
    ///
    /// [`GroupRouteError`] if a nested loc id cannot be resolved.
    pub async fn global_hash<V: GlobalHash>(&self, value: &V) -> Result<[u8; 32], GroupRouteError> {
        let mut state = self.ready().await;
        on_ready!(state, |s| value.global_hash(&s.ops()).await)
    }

    // ── layer 2: one append-only shard per group ──────────────────────────

    /// Add an observation to `group`'s shard, dedup'd by `(sender, tx_id)`.
    /// `None` means nothing changed (a stale copy); otherwise the shard delta.
    ///
    /// # Panics
    ///
    /// If the group already observed `(sender, tx_id)`: supersede is a
    /// `todo!()` until observations can be ranked by authority.
    pub async fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> Option<GroupDelta> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.store_event(group, ev).await)
    }

    /// Arbitrary access by slot within a group's shard.
    pub async fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().stored_event(group, slot).await)
    }

    /// Ids added/removed in `group` since `since`, plus the new tip.
    pub async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.ops().diff_group(group, since).await)
    }

    // ── layer 3: the gear's private view ──────────────────────────────────

    /// Open a view scoped to `gear`. Writes stage in the view; the runtime
    /// commits it after the gear run.
    pub async fn gear_view(&self, gear: &R::GearId) -> GearView {
        let table = GearTable::of(R::gear_kind(gear));
        let prefix = gear_key::<R>(gear).to_vec();
        let state = self.ready().await;
        match &*state {
            State::Ram {
                ready: Some(ready), ..
            } => GearView::ram(Rc::clone(&ready.cache), table, prefix),
            State::Fs {
                ready: Some(ready), ..
            } => GearView::fs(Rc::clone(&ready.cache), table, prefix),
            _ => unreachable!("ready() returns an opened state"),
        }
    }

    // ── durability ────────────────────────────────────────────────────────

    /// The log's durability point. Once it returns, every event appended so
    /// far survives a crash; called once per `post_events` batch before any
    /// reply.
    ///
    /// # Errors
    ///
    /// The journal's own I/O failure; a poisoned log refuses further writes.
    pub async fn commit_log(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        on_ready!(state, |s| s.commit_log().await)
    }

    /// The full commit point: after a successful `flush`, everything written
    /// before it survives a crash. A flush implies [`commit_log`](Self::commit_log),
    /// then persists the page store.
    ///
    /// # Errors
    ///
    /// The journal's I/O failure, or the page store's flush failure.
    pub async fn flush(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        on_ready!(state, |s| flush(s).await)
    }

    #[cfg(test)]
    pub(crate) async fn replayed(&self) -> usize {
        let state = self.ready().await;
        match &*state {
            State::Ram {
                ready: Some(ready), ..
            } => ready.replayed,
            State::Fs {
                ready: Some(ready), ..
            } => ready.replayed,
            _ => unreachable!("ready() returns an opened state"),
        }
    }
}

impl<R: RuntimeTypes> std::fmt::Debug for CacheStorage<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CacheStorage").finish_non_exhaustive()
    }
}

impl<R: RuntimeTypes> Default for CacheStorage<R> {
    fn default() -> Self {
        Self::ram()
    }
}

/// Open the page store and bring it to the journal's head, replaying the tail
/// past the image's applied cursor.
async fn open<R: RuntimeTypes, D: IsFilesystem + Default + Clone>(
    store: &Store<D>,
    journal_dir: Option<&Path>,
) -> Result<Ready<R, D>, String> {
    let fingerprint = tables::fingerprint::<R>();
    let journal = match journal_dir {
        Some(dir) => Some(open_journal::<R>(dir).await?),
        None => None,
    };

    let tables = Tables::new();
    store.configure(tables::layout::<R>(&tables));
    let mut cache = store.get().await;
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
        store.reset().await;
        cache = store.get().await;
        meta = MetaState::Fresh;
    }
    let valid = matches!(meta, MetaState::Valid(_));
    let from = meta.start();

    // Only the tail past the image's cursor is visited; a warm image visits
    // none.
    let replay_started = Instant::now();
    let ops = super::ops::Ops::<R, D>::new(&cache, &tables);
    let replayed = match &journal {
        Some(journal) => replay::replay::<R, D>(&journal.log, &journal.schemas, from, &ops)
            .await
            .map_err(|e| format!("replay: {e}"))?,
        None => 0,
    };
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
        _r: PhantomData,
    };

    log::info!(
        "storage: head {head}, {} records replayed from {from} in {elapsed:?}",
        ready.replayed,
    );

    Ok(ready)
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

/// Sync the log, then advance the applied cursor, then flush the pages.
async fn flush<R: RuntimeTypes, D: IsFilesystem>(ready: &mut Ready<R, D>) -> io::Result<()> {
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
