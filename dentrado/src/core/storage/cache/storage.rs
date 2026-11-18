//! The backend handle: lazy open, the public storage surface, and the commit
//! point.
//!
//! Opening is lazy because `make_storage` runs before the core's runtime
//! exists. A failed open panics on every later call: there is no silent
//! degradation to RAM. The slow path is guarded by [`Mutex`] only until the
//! handle is opened; every operation after that is a [`OnceCell`] read.
//!
//! ## One type, two disks
//!
//! `CacheStorage<R>` is the one storage type the engine, the gears, and the
//! tests all name. The disk is a runtime choice (`ram()` or `on_disk(path)`),
//! erased at this boundary.
//!
//! ## Concurrency
//!
//! Same-key allocations are coalesced by the sequencer's in-flight table;
//! different keys reserve ids synchronously and append independently. The
//! journal is the only shared writer, so its lock is held for the append and
//! the sync — never for a cache lookup. [`commit_log`](Self::commit_log)
//! writes the batch and the meta cursor in one cache session.

use std::{cell::OnceCell, io, marker::PhantomData, path::PathBuf, rc::Rc};

use dentrado_cache::{
    TableId,
    disk::{FsDisk, MemDisk},
};
use futures::lock::Mutex;

use super::{
    codec, events, live,
    live::{Disk, Opened},
    open,
};
use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::StoredEvent,
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

/// Run a cache-only operation against whichever disk the backend opened.
///
/// The two disks are distinct types, so the dispatch is a macro; the body is
/// written once and instantiated per disk.
macro_rules! on_cache {
    ($opened:expr, |$cache:ident| $body:expr) => {
        match &$opened.disk {
            Disk::Ram { cache } => {
                let $cache = cache;
                $body
            }
            Disk::Fs { cache, .. } => {
                let $cache = cache;
                $body
            }
        }
    };
}

impl<R: RuntimeTypes> Opened<R> {
    /// A delta-first lookup through whichever disk is open.
    async fn get(&self, table: TableId, key: &[u8]) -> Option<Vec<u8>> {
        on_cache!(self, |cache| self.seq.get(cache, table, key).await)
    }
}

/// The cache-backed layers 1–3 over either disk.
pub struct CacheStorage<R: RuntimeTypes> {
    backend: Backend,
    opened: OnceCell<Result<Rc<Opened<R>>, String>>,
    /// Held only while the first caller opens the backend.
    opening: Mutex<()>,
    /// Serializes `commit_log`: the journal writer and the meta session.
    committing: Mutex<()>,
}

/// The unopened backend choice.
enum Backend {
    Ram {
        store: Rc<Store<MemDisk>>,
    },
    Fs {
        store: Rc<Store<FsDisk>>,
        journal: PathBuf,
    },
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
            backend: Backend::Ram { store },
            opened: OnceCell::new(),
            opening: Mutex::new(()),
            committing: Mutex::new(()),
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
            backend: Backend::Fs { store, journal },
            opened: OnceCell::new(),
            opening: Mutex::new(()),
            committing: Mutex::new(()),
        }
    }

    async fn ensure_open(&self) -> Result<Rc<Opened<R>>, String> {
        if let Some(result) = self.opened.get() {
            return result.clone();
        }
        let _guard = self.opening.lock().await;
        if let Some(result) = self.opened.get() {
            return result.clone();
        }
        let result = match &self.backend {
            Backend::Ram { store } => open::materialize::<R, MemDisk>(store, None).await.map(|m| {
                Rc::new(Opened {
                    disk: Disk::Ram { cache: m.cache },
                    tables: m.tables,
                    seq: m.seq,
                    replayed: m.replayed,
                    _r: PhantomData,
                })
            }),
            Backend::Fs { store, journal } => open::materialize::<R, FsDisk>(store, Some(journal))
                .await
                .map(|m| {
                    let log = m.journal.expect("the fs backend opens a journal");
                    Rc::new(Opened {
                        disk: Disk::Fs {
                            cache: m.cache,
                            identity: log.identity,
                            journal: Rc::new(Mutex::new(log)),
                            fingerprint: m.fingerprint,
                        },
                        tables: m.tables,
                        seq: m.seq,
                        replayed: m.replayed,
                        _r: PhantomData,
                    })
                }),
        };
        self.opened.set(result.clone()).ok();
        result
    }

    /// The opened handle. A failed open panics on every later call.
    async fn opened(&self) -> Rc<Opened<R>> {
        match self.ensure_open().await {
            Ok(opened) => opened,
            Err(e) => {
                log::error!("storage is unusable: {e}");
                panic!("storage is unusable: {e}");
            }
        }
    }

    /// The raw open, for the backend tests: unlike every operation, a failure
    /// is returned instead of poisoning the handle.
    #[cfg(test)]
    pub(super) async fn open(&self) -> Result<(), String> {
        self.ensure_open().await.map(|_| ())
    }

    // ── layer 1: localization, idempotent allocators ──────────────────────

    /// Intern a user identity, returning its existing local id when known.
    pub async fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        self.opened().await.user_flight(uid).await
    }

    /// Intern a sender; with a `uid` the user is interned too.
    pub async fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        self.opened().await.sender_flight(pk, uid).await
    }

    /// Intern a `(message type, group)` pair, returning its existing local id
    /// when known.
    ///
    /// # Panics
    ///
    /// If `group` cannot be encoded as postcard.
    pub async fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        let bytes = postcard::to_stdvec(&group).expect("a group value serializes");
        self.opened().await.group_flight(msg_type, bytes).await
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
        self.opened().await.data_flight(data_id, content).await
    }

    // ── layer 1: reverse lookups ──────────────────────────────────────────

    /// The identity behind a local user id.
    pub async fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        let opened = self.opened().await;
        let bytes = opened
            .get(opened.tables.users_rev, &codec::user_rev(lid))
            .await?;
        codec::parse_user(&bytes)
    }

    /// The sender's local user, when it has one.
    pub async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.sender_rev(sid).await.and_then(|(_, user)| user)
    }

    /// The sender's public key.
    pub async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.sender_rev(sid).await.map(|(pk, _)| pk)
    }

    async fn sender_rev(&self, sid: LocSenderId) -> Option<(SenderPk, Option<LocUserId>)> {
        let opened = self.opened().await;
        let bytes = opened
            .get(opened.tables.senders_rev, &codec::sender_rev(sid))
            .await?;
        codec::parse_sender_rev(&bytes)
    }

    /// The local id content-addressed data is already interned under.
    pub async fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        let opened = self.opened().await;
        let bytes = opened
            .get(opened.tables.data_fwd, &codec::data_fwd(*data_id))
            .await?;
        Some(LocDataId(codec::parse_loc(&bytes)?))
    }

    /// The content address and value behind a local data id.
    ///
    /// # Panics
    ///
    /// If the stored row does not hold a postcard-encoded `R::Data`; the cache
    /// only stores values this backend wrote.
    pub async fn fetch_data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        let opened = self.opened().await;
        let bytes = opened
            .get(opened.tables.data_rev, &codec::data_rev(did))
            .await?;
        let (data_id, content) = codec::parse_data_value(&bytes)?;
        let content = postcard::from_bytes(content).expect("a cached data value decodes");
        Some((data_id, content))
    }

    /// The global content hash of a value, resolving embedded loc ids through
    /// this backend's localization maps.
    ///
    /// # Errors
    ///
    /// [`GroupRouteError`] if a nested loc id cannot be resolved.
    pub async fn global_hash<V: GlobalHash>(&self, value: &V) -> Result<[u8; 32], GroupRouteError> {
        let opened = self.opened().await;
        on_cache!(opened, |cache| {
            let resolver = live::Resolver {
                cache,
                tables: &opened.tables,
                seq: &opened.seq,
            };
            value.global_hash(&resolver).await
        })
    }

    // ── layer 2: one append-only shard per group ──────────────────────────

    /// Add an observation to `group`'s shard, dedup'd by `(sender, tx_id)`.
    /// `None` means nothing changed (a stale copy); otherwise the shard delta.
    /// The event body is not readable until the next `commit_log`.
    ///
    /// # Panics
    ///
    /// If the group already observed `(sender, tx_id)` with different bytes:
    /// supersede is a `todo!()` until observations can be ranked by authority.
    pub async fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> Option<crate::core::loc_ctx::GroupDelta> {
        let body = schema::encode(&ev.body).expect("an event body serializes");
        let value =
            codec::event_bytes(ev.sender.0, ev.tx_id, ev.timestamp, ev.source_node.0, &body);
        let key = (group, ev.sender, ev.tx_id);
        let (flight, owner) = self.opened().await.event_flight(key, ev, value.clone());
        let result = flight.await;
        if result.value != value {
            todo!("supersede: rank by (authoritative, timestamp, source_node)")
        }
        if owner { result.delta } else { None }
    }

    /// Arbitrary access by slot within a group's shard. Only committed events
    /// are visible.
    pub async fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        let opened = self.opened().await;
        on_cache!(opened, |cache| {
            events::stored_event::<R, _>(cache, &opened.tables, group, slot).await
        })
    }

    /// Ids added/removed in `group` since `since`, plus the new tip.
    pub async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let opened = self.opened().await;
        on_cache!(opened, |cache| {
            opened
                .seq
                .diff_group(cache, &opened.tables, group, since)
                .await
        })
    }

    // ── layer 3: the gear's private view ──────────────────────────────────

    /// Open a view scoped to `gear`. Writes stage in the view; the runtime
    /// commits it after the gear run.
    pub async fn gear_view(&self, gear: &R::GearId) -> GearView {
        let table = self.opened().await.tables.gear(R::gear_kind(gear));
        let prefix = gear_key::<R>(gear).to_vec();
        let opened = self.opened().await;
        match &opened.disk {
            Disk::Ram { cache } => GearView::ram(Rc::clone(cache), table, prefix),
            Disk::Fs { cache, .. } => GearView::fs(Rc::clone(cache), table, prefix),
        }
    }

    // ── durability ────────────────────────────────────────────────────────

    /// The log's durability point and the cache's applied watermark. Once it
    /// returns, every event appended so far survives a crash; called once per
    /// `post_events` batch before any reply.
    ///
    /// # Errors
    ///
    /// The journal's own I/O failure; a poisoned log refuses further writes.
    pub async fn commit_log(&self) -> io::Result<()> {
        let opened = self.opened().await;
        let _gate = self.committing.lock().await;
        match &opened.disk {
            Disk::Ram { cache } => {
                let commit = opened.seq.begin_commit(None);
                live::commit(cache, &opened.tables, &opened.seq, None, commit)
            }
            Disk::Fs {
                cache,
                journal,
                identity,
                fingerprint,
            } => {
                let (head, commit) = {
                    let mut journal = journal.lock().await;
                    journal.log.sync().await.map_err(io::Error::other)?;
                    let head = journal.log.head();
                    (head, opened.seq.begin_commit(Some(head)))
                };
                live::commit(
                    cache,
                    &opened.tables,
                    &opened.seq,
                    Some((*identity, *fingerprint, head)),
                    commit,
                )
            }
        }
    }

    /// The full commit point: after a successful `flush`, everything written
    /// before it survives a crash. A flush implies [`commit_log`](Self::commit_log),
    /// then persists the page store.
    ///
    /// # Errors
    ///
    /// The journal's I/O failure, or the page store's flush failure.
    pub async fn flush(&self) -> io::Result<()> {
        self.commit_log().await?;
        let opened = self.opened().await;
        on_cache!(opened, |cache| cache.flush().await)
            .map_err(|e| io::Error::other(format!("flush the cache: {e:?}")))
    }

    #[cfg(test)]
    pub(crate) async fn replayed(&self) -> usize {
        self.opened().await.replayed
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
