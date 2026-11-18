//! The journaled layers 1–2: records dentrado declares, and the fold replay
//! rebuilds from them.
//!
//! The journal itself is domain-blind; everything it needs to know about a
//! `LocUserId`, a group or an event is here, written as a record with a shape.
//! Schema ids are log-local and assigned on first use — see [`schema::Schemas`].
//!
//! # What lives where
//!
//! - The **journal** is the source of truth: loc assignments and events, in
//!   append order, durable at [`sync_log`](JournalStorage::sync_log).
//! - The **fold** is the in-RAM layer 1–2 state. It holds envelopes, dedup and
//!   the changelog, not bodies: replay reads the body bytes without decoding
//!   them.
//! - The **cache** holds the bodies (`cache::Tables::events`) and the cursor it
//!   is materialized through (`cache::Tables::meta`). A cold start scans the
//!   journal for the fold and copies only the tail after `meta.applied`.
//!
//! Open is lazy: `PagedStorage::on_disk` runs before the core's runtime exists,
//! so the journal and the page store open on the first storage call, under the
//! same `Store` oneshot protocol as the gear views.
//!
//! # Errors are fatal
//!
//! A journal that cannot be opened, an interior record that cannot be applied,
//! or a cache body that contradicts the fold is a process-level failure. The
//! journal is the source of truth; serving from a partial one would be worse
//! than restarting.

pub mod record;
pub mod schema;
pub(crate) mod fold;
pub(crate) mod cache;

use std::{io, path::PathBuf, rc::Rc, time::Instant};

use dentrado_cache::{Storage, StoreError, disk::FsDisk};
use dentrado_journal::{Cursor, Frame, Journal, SchemaId};
use futures::lock::{Mutex, MutexGuard};

use super::{
    GroupDiff, IsStorage, Watermark,
    fold::Fold,
    store::Store,
    view::{DiskView, gear_key},
};
use crate::{
    core::{
        api::{GearTable, RuntimeTypes},
        loc_ctx::{GroupDelta, StoredEvent},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// The durable layer 1–2 backend: journal + fold + body cache.
pub(crate) struct JournalStorage<R: RuntimeTypes> {
    dir: PathBuf,
    store: Rc<Store>,
    state: Mutex<State<R>>,
}

struct State<R: RuntimeTypes> {
    ready: Option<Ready<R>>,
    failed: Option<String>,
}

struct Ready<R: RuntimeTypes> {
    journal: Journal,
    fold: Fold<R, ()>,
    cache: Rc<Storage<FsDisk>>,
    tables: cache::Tables,
    schemas: schema::Schemas,
    identity: [u8; 16],
    fingerprint: [u8; 32],
}

impl<R: RuntimeTypes> JournalStorage<R> {
    /// Disk mode. The journal directory is the page-store path with its
    /// extension replaced by `journal`; both open on first use.
    #[must_use]
    pub(crate) fn new(file: PathBuf) -> Self {
        Self {
            dir: file.with_extension("journal"),
            store: Rc::new(Store::new(file)),
            state: Mutex::new(State {
                ready: None,
                failed: None,
            }),
        }
    }

    /// The opened state, opening on first use. A failed open panics on every
    /// later call: there is no silent degradation to RAM.
    async fn ready(&self) -> MutexGuard<'_, State<R>> {
        let mut state = self.state.lock().await;
        if state.ready.is_none() && state.failed.is_none() {
            match self.open().await {
                Ok(ready) => state.ready = Some(ready),
                Err(e) => state.failed = Some(e),
            }
        }
        if let Some(e) = &state.failed {
            log::error!("journal storage {} is unusable: {e}", self.dir.display());
            panic!("journal storage is unusable: {e}");
        }
        state
    }

    async fn open(&self) -> Result<Ready<R>, String> {
        let mut journal = Journal::open(&self.dir)
            .await
            .map_err(|e| format!("open {}: {e}", self.dir.display()))?;
        if journal.truncated() > 0 {
            log::warn!(
                "journal {}: repaired {} bytes of torn tail",
                self.dir.display(),
                journal.truncated()
            );
        }
        let identity = journal
            .identity()
            .await
            .map_err(|e| format!("identity of {}: {e}", self.dir.display()))?;
        let schemas = schema::Schemas::declare::<R>(&mut journal)
            .await
            .map_err(|e| format!("declare schemas in {}: {e}", self.dir.display()))?;
        let fingerprint = cache::fingerprint(journal.decls());
        let tables = cache::Tables::new(fingerprint);
        self.store
            .configure(cache::layout::<R>(&tables, fingerprint));
        let cache = self.store.get().await;

        let applied = cache::read_applied(&cache, &tables, identity, fingerprint).await;
        let head = journal.head();
        let from = if applied > head {
            log::warn!(
                "journal {}: cache applied {applied} is past the head {head}; refilling",
                self.dir.display()
            );
            Cursor::START
        } else {
            applied
        };

        let replay_started = Instant::now();
        let mut fold = Fold::<R, ()>::new();
        let mut session = cache.session();
        let replayed = fold::replay(
            &journal,
            &schemas,
            Cursor::START,
            &mut fold,
            |at, ev, delta| {
                if at < from {
                    return;
                }
                let Some(slot) = delta.added else {
                    return;
                };
                let key = cache::event_key(LocGroupId(ev.group), slot);
                let value = cache::event_value(SchemaId(ev.body_schema), ev.body);
                session
                    .put(&tables.events, &key, &value, &[], &[])
                    .expect("stage an event body");
            },
        )
        .await
        .map_err(|e| format!("replay {}: {e}", self.dir.display()))?;
        let replay = replay_started.elapsed();

        cache::write_meta(&mut session, &tables, identity, fingerprint, replayed)
            .map_err(|e| format!("stage cache meta: {e:?}"))?;
        session
            .commit()
            .map_err(|e| format!("commit the materialized cache: {e:?}"))?;
        cache
            .flush()
            .await
            .map_err(|e| format!("flush the materialized cache: {e:?}"))?;

        log::info!(
            "journal {}: {} segments, head {head}, applied {applied}, refilled from {from} \
             in {replay:?}; fold: {}",
            self.dir.display(),
            journal.segments(),
            fold.size()
        );

        Ok(Ready {
            journal,
            fold,
            cache,
            tables,
            schemas,
            identity,
            fingerprint,
        })
    }
}

impl<R: RuntimeTypes> Ready<R> {
    async fn append(&mut self, schema: SchemaId, bytes: &[u8]) {
        self.journal
            .append(&[Frame::new(schema, bytes)])
            .await
            .expect("append to the journal");
    }

    async fn ensure_user(&mut self, uid: UserId) {
        if self.fold.loc_user(uid).is_some() {
            return;
        }
        let loc = self.fold.mk_loc_user(uid);
        let record = record::LocUser {
            loc: loc.0,
            id: uid.id,
            identity_server_pk: uid.identity_server_pk.0,
        };
        let bytes = schema::encode(&record).expect("a loc-user record serializes");
        self.append(self.schemas.user, &bytes).await;
    }

    fn commit_meta(&mut self) -> Result<(), StoreError> {
        let head = self.journal.head();
        let mut session = self.cache.session();
        cache::write_meta(
            &mut session,
            &self.tables,
            self.identity,
            self.fingerprint,
            head,
        )?;
        session.commit()
    }
}

impl<R: RuntimeTypes> IsStorage<R> for JournalStorage<R> {
    // ── localization: idempotent allocators ───────────────────────────────

    async fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        ready.ensure_user(uid).await;
        ready
            .fold
            .loc_user(uid)
            .expect("the user was just interned")
    }

    async fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        if let Some(uid) = uid {
            ready.ensure_user(uid).await;
        }
        if let Some(loc) = ready.fold.loc_sender(pk) {
            return loc;
        }
        let loc = ready.fold.mk_loc_sender(pk, uid);
        let user = uid
            .and_then(|uid| ready.fold.loc_user(uid))
            .map(|loc| loc.0);
        let record = record::LocSender {
            loc: loc.0,
            pk: pk.0,
            user,
        };
        let bytes = schema::encode(&record).expect("a loc-sender record serializes");
        ready.append(ready.schemas.sender, &bytes).await;
        loc
    }

    async fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        if let Some(loc) = ready.fold.loc_group(msg_type, &group) {
            return loc;
        }
        let bytes = schema::encode(&group).expect("a group value serializes");
        let loc = ready.fold.mk_loc_group(msg_type, group);
        let record = record::LocGroup {
            loc: loc.0,
            msg_type: msg_type.0,
            group_schema: ready.schemas.group_value.0,
            group: bytes,
        };
        let bytes = schema::encode(&record).expect("a loc-group record serializes");
        ready.append(ready.schemas.group, &bytes).await;
        loc
    }

    async fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        if let Some(loc) = ready.fold.find_data(&data_id) {
            return Ok(loc);
        }
        let bytes = schema::encode(&content).expect("a data value serializes");
        let loc = ready.fold.mk_data(data_id, content)?;
        let record = record::LocData {
            loc: loc.0,
            timestamp: data_id.timestamp,
            hash: data_id.hash,
            content_schema: ready.schemas.data_value.0,
            content: bytes,
        };
        let bytes = schema::encode(&record).expect("a loc-data record serializes");
        ready.append(ready.schemas.data, &bytes).await;
        Ok(loc)
    }

    // ── localization: reverse lookups ─────────────────────────────────────

    async fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        let state = self.ready().await;
        state
            .ready
            .as_ref()
            .expect("opened")
            .fold
            .user_by_local(lid)
    }

    async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        let state = self.ready().await;
        state.ready.as_ref().expect("opened").fold.sender_user(sid)
    }

    async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        let state = self.ready().await;
        state.ready.as_ref().expect("opened").fold.sender_pk(sid)
    }

    async fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        let state = self.ready().await;
        state
            .ready
            .as_ref()
            .expect("opened")
            .fold
            .find_data(data_id)
    }

    async fn fetch_data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        let state = self.ready().await;
        state.ready.as_ref().expect("opened").fold.fetch_data(did)
    }

    async fn global_hash<V: GlobalHash>(&self, value: &V) -> Result<[u8; 32], GroupRouteError> {
        let state = self.ready().await;
        let ready = state.ready.as_ref().expect("opened");
        value.global_hash(&ready.fold)
    }

    // ── event log: one append-only shard per group ───────────────────────

    async fn store_event(&self, group: LocGroupId, ev: StoredEvent<R::Body>) -> Option<GroupDelta> {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        let StoredEvent {
            sender,
            tx_id,
            timestamp,
            source_node,
            body,
        } = ev;
        let body = schema::encode(&body).expect("an event body serializes");
        // Fold first: an observation the fold refuses (a conflicting one is a
        // `todo!()` today) must not reach the journal, where replay would hit
        // it again on every start.
        let delta = ready.fold.store_event(
            group,
            StoredEvent {
                sender,
                tx_id,
                timestamp,
                source_node,
                body: (),
            },
        )?;
        let Some(slot) = delta.added else {
            // A removal-only delta has no new body to journal.
            return Some(delta);
        };
        let envelope = record::Event {
            group: group.0,
            sender: sender.0,
            tx_id,
            timestamp,
            source_node: source_node.0,
            body_schema: ready.schemas.body_value.0,
            body: &body,
        };
        ready.append(ready.schemas.event, &envelope.encode()).await;
        let key = cache::event_key(group, slot);
        let value = cache::event_value(ready.schemas.body_value, &body);
        let mut session = ready.cache.session();
        session
            .put(&ready.tables.events, &key, &value, &[], &[])
            .expect("stage an event body");
        session.commit().expect("commit an event body");
        Some(delta)
    }

    async fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        let state = self.ready().await;
        let ready = state.ready.as_ref().expect("opened");
        let envelope = ready.fold.fetch_event(group, slot)?;
        let key = cache::event_key(group, slot);
        let value = ready
            .cache
            .get(&ready.tables.events, &key)
            .await
            .expect("read an event body");
        let Some(value) = value else {
            panic!("event {group:?}/{slot:?} is live but absent from the cache");
        };
        let (schema, body) = cache::event_body(&value).expect("a cached body has a schema");
        assert_eq!(
            schema, ready.schemas.body_value,
            "a cached body was written with another layout"
        );
        let body = postcard::from_bytes(body).expect("decode an event body");
        Some(StoredEvent {
            sender: envelope.sender,
            tx_id: envelope.tx_id,
            timestamp: envelope.timestamp,
            source_node: envelope.source_node,
            body,
        })
    }

    async fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let state = self.ready().await;
        state
            .ready
            .as_ref()
            .expect("opened")
            .fold
            .diff_group(group, since)
    }

    // ── layer 3: the gear's private view ──────────────────────────────────

    type View<'a>
        = DiskView
    where
        Self: 'a;

    async fn gear_view(&self, gear: &R::GearId) -> Self::View<'_> {
        let table = GearTable::of(R::gear_kind(gear));
        let prefix = gear_key::<R>(gear).to_vec();
        let cache = {
            let state = self.ready().await;
            Rc::clone(&state.ready.as_ref().expect("opened").cache)
        };
        DiskView::new(cache, table, prefix)
    }

    // ── durability ────────────────────────────────────────────────────────

    async fn sync_log(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        ready.journal.sync().await.map_err(io::Error::other)?;
        ready.commit_meta().map_err(io::Error::other)
    }

    async fn flush(&self) -> io::Result<()> {
        let mut state = self.ready().await;
        let ready = state.ready.as_mut().expect("opened");
        ready.journal.sync().await.map_err(io::Error::other)?;
        ready.commit_meta().map_err(io::Error::other)?;
        ready
            .cache
            .flush()
            .await
            .map_err(|e| io::Error::other(format!("flush the journaled cache: {e:?}")))
    }
}
