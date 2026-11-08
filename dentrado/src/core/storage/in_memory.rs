//! Reference typed in-memory [`Storage`] implementation.
//!
//! Behaviorally identical to the structures it replaces in `loc_ctx.rs` /
//! `core_ctx.rs`: the localization `HashMap`s, the per-group event shards
//! ([`EventGroup`]: bodies + `(sender, tx_id)` dedup + changelog), and the
//! per-gear view — just lifted behind the trait. Used as the test backend and
//! the baseline a disk backend is validated against.
//!
//! Like any RAM-only backend, NOTHING here survives process death — not just
//! the gear views, but events and localization too. That is a property of
//! the backend, not of any one layer; the `Storage` contract treats all of it
//! as restart-surviving state, satisfied here in the (only) trivial sense RAM
//! allows: live for the lifetime of this object.
//!
//! All methods do their work eagerly (synchronously) and return a `ready`
//! future — there is no IO to await — so no `RefCell` borrow is ever held
//! across an `.await`. Typed end-to-end: zero serialization.

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap},
    future::ready,
};

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{EventGroup, StoreResultSuccess, StoredEvent},
        storage::{GroupDiff, IsStorage, RamView, Watermark, view::gear_key},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GlobalResolver, GroupEventId, GroupRouteError,
        LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

struct Inner<R: RuntimeTypes> {
    // localization
    pk_to_sender: HashMap<SenderPk, LocSenderId>,
    sender_to_pk: HashMap<LocSenderId, SenderPk>,
    sender_to_user: HashMap<LocSenderId, LocUserId>,
    user_id_to_local: HashMap<UserId, LocUserId>,
    local_to_user_id: HashMap<LocUserId, UserId>,
    // event log: one append-only shard per group (bodies + dedup + changelog).
    events_by_group: HashMap<LocGroupId, EventGroup<R::Body>>,
    // content-addressed data
    data_by_id: Vec<(DataId, R::Data)>,
    data_id_to_local: HashMap<DataId, LocDataId>,
    // group localization
    group_by_key: HashMap<(LocMsgTypeId, R::Group), LocGroupId>,
}

impl<R: RuntimeTypes> Inner<R> {
    fn new() -> Self {
        Self {
            pk_to_sender: HashMap::new(),
            sender_to_pk: HashMap::new(),
            sender_to_user: HashMap::new(),
            user_id_to_local: HashMap::new(),
            local_to_user_id: HashMap::new(),
            events_by_group: HashMap::new(),
            data_by_id: Vec::new(),
            data_id_to_local: HashMap::new(),
            group_by_key: HashMap::new(),
        }
    }
}

/// In-memory typed `Storage`.
pub struct InMemoryStorage<R: RuntimeTypes> {
    inner: RefCell<Inner<R>>,
    /// The gear views: full (prefixed) key → flat value.
    gear_data: RefCell<BTreeMap<Vec<u8>, Vec<u8>>>,
}

impl<R: RuntimeTypes> Default for InMemoryStorage<R> {
    fn default() -> Self {
        Self {
            inner: RefCell::new(Inner::new()),
            gear_data: RefCell::new(BTreeMap::new()),
        }
    }
}

impl<R: RuntimeTypes> InMemoryStorage<R> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The backing map of every gear view — used by [`PagedStorage`]'s
    /// RAM mode.
    ///
    /// [`PagedStorage`]: crate::core::storage::PagedStorage
    pub(crate) fn gear_data(&self) -> &RefCell<BTreeMap<Vec<u8>, Vec<u8>>> {
        &self.gear_data
    }
}

// mk_data verifies content hashes by resolving embedded LocDataId refs against
// the store, so the backend must be a GlobalResolver over its own state.
impl<R: RuntimeTypes> GlobalResolver for InMemoryStorage<R> {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        let inner = self.inner.borrow();
        inner.local_to_user_id.get(&lid).copied().ok_or({
            GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: inner.user_id_to_local.len(),
            }
        })
    }

    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        let inner = self.inner.borrow();
        inner
            .data_by_id
            .get(did.0 as usize)
            .map(|(id, _)| *id)
            .ok_or({
                GroupRouteError::DataIdOutOfBounds {
                    idx: did.0,
                    objects_len: inner.data_by_id.len(),
                }
            })
    }
}

impl<R: RuntimeTypes> IsStorage<R> for InMemoryStorage<R> {
    // ── localization: idempotent allocators ───────────────────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId> {
        let mut inner = self.inner.borrow_mut();
        if let Some(&luid) = inner.user_id_to_local.get(&uid) {
            return ready(luid);
        }
        let luid = LocUserId(inner.user_id_to_local.len() as u64);
        inner.user_id_to_local.insert(uid, luid);
        inner.local_to_user_id.insert(luid, uid);
        ready(luid)
    }

    fn mk_loc_sender(
        &self,
        pk: SenderPk,
        uid: Option<UserId>,
    ) -> impl Future<Output = LocSenderId> {
        let mut inner = self.inner.borrow_mut();
        if let Some(uid_val) = uid {
            // inline mk_loc_user to keep this method a single borrow scope
            if !inner.user_id_to_local.contains_key(&uid_val) {
                let luid = LocUserId(inner.user_id_to_local.len() as u64);
                inner.user_id_to_local.insert(uid_val, luid);
                inner.local_to_user_id.insert(luid, uid_val);
            }
        }
        if let Some(&existing) = inner.pk_to_sender.get(&pk) {
            return ready(existing);
        }
        let sid = LocSenderId(inner.pk_to_sender.len() as u64);
        inner.pk_to_sender.insert(pk, sid);
        inner.sender_to_pk.insert(sid, pk);
        if let Some(uid_val) = uid {
            let lid = inner.user_id_to_local[&uid_val];
            inner.sender_to_user.insert(sid, lid);
        }
        ready(sid)
    }

    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId> {
        let mut inner = self.inner.borrow_mut();
        let key = (msg_type, group);
        if let Some(&gid) = inner.group_by_key.get(&key) {
            return ready(gid);
        }
        let gid = LocGroupId(inner.group_by_key.len() as u64);
        inner.group_by_key.insert(key, gid);
        ready(gid)
    }

    fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> impl Future<Output = Result<LocDataId, DataVerifyError>> {
        // 1. Idempotent fast path (shared borrow, then dropped).
        if let Some(&existing) = self.inner.borrow().data_id_to_local.get(&data_id) {
            return ready(Ok(existing));
        }
        // 2. Verify content hash. `global_hash` resolves embedded LocDataId refs
        //    via `GlobalResolver`, which takes its own shared borrow — so no
        //    borrow may be held across this call.
        match content.global_hash(self) {
            Ok(hash) if hash == data_id.hash => {}
            Ok(computed_hash) => {
                return ready(Err(DataVerifyError::HashMismatch {
                    claimed: data_id,
                    computed_hash,
                }));
            }
            Err(e) => return ready(Err(DataVerifyError::UnresolvableId(e))),
        }
        // 3. Record (mutable borrow). Single-threaded + eager ⇒ nothing
        //    mutated `data_id_to_local` between steps 1 and 3.
        let mut inner = self.inner.borrow_mut();
        let did = LocDataId(inner.data_by_id.len() as u64);
        inner.data_id_to_local.insert(data_id, did);
        inner.data_by_id.push((data_id, content));
        ready(Ok(did))
    }

    // ── localization: reverse lookups ─────────────────────────────────────

    fn user_by_local(&self, lid: LocUserId) -> impl Future<Output = Option<UserId>> {
        ready(self.inner.borrow().local_to_user_id.get(&lid).copied())
    }

    fn sender_user(&self, sid: LocSenderId) -> impl Future<Output = Option<LocUserId>> {
        ready(self.inner.borrow().sender_to_user.get(&sid).copied())
    }

    fn sender_pk(&self, sid: LocSenderId) -> impl Future<Output = Option<SenderPk>> {
        ready(self.inner.borrow().sender_to_pk.get(&sid).copied())
    }

    fn find_data(&self, data_id: &DataId) -> impl Future<Output = Option<LocDataId>> {
        ready(self.inner.borrow().data_id_to_local.get(data_id).copied())
    }

    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<(DataId, R::Data)>> {
        ready(
            self.inner
                .borrow()
                .data_by_id
                .get(did.0 as usize)
                .map(|(id, c)| (*id, c.clone())),
        )
    }

    // ── event log: one append-only shard per group ───────────────────────

    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<StoreResultSuccess>> {
        let mut inner = self.inner.borrow_mut();
        let g = inner.events_by_group.entry(group).or_default();
        let key = (ev.sender, ev.tx_id);
        let new_key = (ev.timestamp, ev.source_node);
        if let Some(&old_slot) = g.dedup.get(&key) {
            let old_ev = &g.bodies[old_slot as usize];
            // Same freshness tiebreak as `LocCtx::store_event`: earliest
            // `(timestamp, source_node)` wins; an at-least-as-early stored
            // observation makes the incoming one stale.
            if (old_ev.timestamp, old_ev.source_node) <= new_key {
                return ready(None);
            }
            let new_slot = g.bodies.len() as u64;
            g.bodies.push(ev);
            g.dedup.insert(key, new_slot);
            g.added.push(GroupEventId(new_slot));
            g.removed.push(GroupEventId(old_slot));
            return ready(Some(StoreResultSuccess {
                old: Some(GroupEventId(old_slot)),
                new: GroupEventId(new_slot),
            }));
        }
        let new_slot = g.bodies.len() as u64;
        g.bodies.push(ev);
        g.dedup.insert(key, new_slot);
        g.added.push(GroupEventId(new_slot));
        ready(Some(StoreResultSuccess {
            old: None,
            new: GroupEventId(new_slot),
        }))
    }

    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>> {
        ready(
            self.inner
                .borrow()
                .events_by_group
                .get(&group)
                .and_then(|g| g.bodies.get(slot.0 as usize))
                .cloned(),
        )
    }

    fn diff_group(&self, group: LocGroupId, since: Watermark) -> impl Future<Output = GroupDiff> {
        let inner = self.inner.borrow();
        let Some(g) = inner.events_by_group.get(&group) else {
            return ready(GroupDiff {
                added: Vec::new(),
                removed: Vec::new(),
                watermark: since,
            });
        };
        let a = (since.0 as usize).min(g.added.len());
        let r = (since.1 as usize).min(g.removed.len());
        ready(GroupDiff {
            added: g.added[a..].to_vec(),
            removed: g.removed[r..].to_vec(),
            watermark: Watermark(g.added.len() as u64, g.removed.len() as u64),
        })
    }

    // ── the gear's private view ───────────────────────────────────────────

    type View<'a>
        = RamView<'a>
    where
        Self: 'a;

    fn gear_view(&self, gear: &R::GearId) -> impl Future<Output = Self::View<'_>> {
        ready(RamView::new(self.gear_data(), gear_key::<R>(gear).to_vec()))
    }

    // ── durability ────────────────────────────────────────────────────────

    fn flush(&self) -> impl Future<Output = std::io::Result<()>> {
        ready(Ok(()))
    }
}
