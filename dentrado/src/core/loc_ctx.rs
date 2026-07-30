use std::collections::HashMap;

use crate::{
    core::gear::IsRuntime,
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, GroupRouteError, LocDataId,
        LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

#[derive(Clone, Debug)]
pub struct StoredEvent<B> {
    pub sender: LocSenderId,
    pub tx_id: u32,
    pub timestamp: u32,
    pub source_node: crate::types::NodeId,
    pub body: B,
}

/// Per-group event shard. Owns, for one group: its event bodies (a flat,
/// append-only `Vec`), its dedup index, and its `added`/`removed` changelog.
///
/// The dedup key is group-scoped — `(sender, tx_id)` — rather than the former
/// `(sender, global_core_id, tx_id)`: `global_core_id` is implied by the group
/// (one group maps to exactly one core), so it is constant within a shard and
/// carries no information. The consequence is that supersede is **always
/// intra-group by construction** — a cross-group "supersede" cannot arise, so a
/// `removed` entry never references a foreign shard.
///
/// `added`/`removed` mirror the previous per-group changelog; each references a
/// body by its `LocEventId` (a slot within this shard). Superseded bodies stay in
/// `bodies` (still fetchable); "removal" is purely a `removed` entry, never a
/// deletion.
#[derive(Debug)]
pub(crate) struct EventGroup<B> {
    pub(crate) bodies: Vec<StoredEvent<B>>,
    /// `(sender, tx_id)` -> slot of the currently-live version.
    pub(crate) dedup: HashMap<(LocSenderId, u32), u64>,
    pub(crate) added: Vec<GroupEventId>,
    pub(crate) removed: Vec<GroupEventId>,
}

// Hand-written (not derived) so it does NOT impose `B: Default` (which
// `#[derive(Default)]` would — `R::Body` is not `Default`). `Vec`/`HashMap`
// default without any bound on `B`.
impl<B> Default for EventGroup<B> {
    fn default() -> Self {
        Self {
            bodies: Vec::new(),
            dedup: HashMap::new(),
            added: Vec::new(),
            removed: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct LocCtx<R: IsRuntime> {
    pk_to_sender: HashMap<SenderPk, LocSenderId>,
    sender_to_pk: HashMap<LocSenderId, SenderPk>,
    sender_to_user: HashMap<LocSenderId, LocUserId>,

    user_id_to_local: HashMap<UserId, LocUserId>,
    local_to_user_id: HashMap<LocUserId, UserId>,

    /// Sharded event storage: one [`EventGroup`] per group. Bodies + dedup +
    /// changelog live together here (previously bodies+dedup were a global
    /// `Vec`/`HashMap` in `LocCtx` and the changelog lived separately in
    /// `CoreLocCtx`); `store_event` is now atomic over all three.
    events_by_group: HashMap<LocGroupId, EventGroup<R::Body>>,

    data_by_id: Vec<(DataId, R::Data)>,
    data_id_to_local: HashMap<DataId, LocDataId>,

    group_by_key: HashMap<(LocMsgTypeId, R::Group), LocGroupId>,
    group_by_id: HashMap<LocGroupId, (LocMsgTypeId, R::Group)>,
}

impl<R: IsRuntime> LocCtx<R> {
    #[must_use]
    pub fn new() -> Self {
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
            group_by_id: HashMap::new(),
        }
    }

    #[must_use]
    pub(crate) fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.sender_to_user.get(&sid).copied()
    }

    #[must_use]
    pub(crate) fn user_by_local(&self, lid: LocUserId) -> Option<UserId> {
        self.local_to_user_id.get(&lid).copied()
    }

    #[must_use]
    pub fn all_users(&self) -> Vec<(LocUserId, UserId)> {
        self.local_to_user_id
            .iter()
            .map(|(&lid, &uid)| (lid, uid))
            .collect()
    }

    #[must_use]
    pub(crate) fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.sender_to_pk.get(&sid).copied()
    }

    #[must_use]
    /// Panics if `Fn` accesses `Core`.
    pub fn get_stored_event<F>(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
        f: impl Fn(&StoredEvent<R::Body>) -> F,
    ) -> Option<F> {
        self.events_by_group
            .get(&group)?
            .bodies
            .get(slot.0 as usize)
            .map(f)
    }

    /// `(added, removed)` appended to `group`'s changelog since `since`, mapped
    /// through `f`. `None` if the group has no shard yet.
    pub(crate) fn query_events<F>(
        &self,
        group: LocGroupId,
        since: (usize, usize),
        f: impl Fn(&[GroupEventId], &[GroupEventId]) -> F,
    ) -> Option<F> {
        self.events_by_group
            .get(&group)
            .map(|eg| f(&eg.added[since.0..], &eg.removed[since.1..]))
    }

    #[must_use]
    pub fn get_data<F>(&self, did: LocDataId, f: impl Fn(&(DataId, R::Data)) -> F) -> Option<F> {
        self.data_by_id.get(did.0 as usize).map(f)
    }

    #[must_use]
    pub(crate) fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId> {
        self.data_id_to_local.get(data_id).copied()
    }

    #[must_use]
    pub(crate) fn find_group(
        &self,
        msg_type: LocMsgTypeId,
        group: &R::Group,
    ) -> Option<LocGroupId> {
        self.group_by_key.get(&(msg_type, group.clone())).copied()
    }
}

impl<R: IsRuntime> Default for LocCtx<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: IsRuntime> GlobalResolver for LocCtx<R> {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        let s = self;
        s.local_to_user_id.get(&lid).copied().ok_or({
            GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: s.user_id_to_local.len(),
            }
        })
    }

    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        let s = self;
        s.data_by_id.get(did.0 as usize).map(|(id, _)| *id).ok_or({
            GroupRouteError::DataIdOutOfBounds {
                idx: did.0,
                objects_len: s.data_by_id.len(),
            }
        })
    }
}

pub struct StoreResultSuccess {
    pub old: Option<GroupEventId>,
    pub new: GroupEventId,
}

/// Read-only access to a localised event/data/sender store, scoped to a single
/// group.
///
/// The only implementor is [`GroupStore`] — a thin view that pairs a backing
/// [`GroupEventSource`] with the one group a gear is running for. Because the
/// group is bound when the view is built, `stored_event` takes only the slot;
/// everything below this trait (`sg_ord_map`, `state_graph`) is completely
/// `LocGroupId`-free.
///
/// All methods return owned values (no borrows from `&self`) so the trait is
/// object-safe and usable as `&dyn EventStore<R>` even behind a `RefCell`.
pub trait EventStore<R: IsRuntime> {
    fn stored_event(&self, slot: GroupEventId) -> Option<StoredEvent<R::Body>>;
    fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId>;
    fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk>;
    fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)>;
}

/// Backing store for a [`GroupStore`]: group-scoped event/data/sender reads.
///
/// Implemented directly by `LocCtx` (which owns the data) and by `Core` (which
/// re-borrows its `RefCell`-guarded `LocCtx` per call) — see `core_ctx.rs`.
pub(crate) trait GroupEventSource<R: IsRuntime> {
    fn stored_event_in(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>>;
    fn sender_user_in(&self, sid: LocSenderId) -> Option<LocUserId>;
    fn sender_pk_in(&self, sid: LocSenderId) -> Option<SenderPk>;
    fn data_in(&self, did: LocDataId) -> Option<(DataId, R::Data)>;
}

/// A group-bound read view over a localised store. Carries the group so that
/// [`EventStore::stored_event`] needs only the slot — the group is a property
/// of the gear (one gear = one group), never of the event id. Construct this
/// once at the gear-run boundary; everything passed it downstream stays
/// group-agnostic.
pub struct GroupStore<'a, R: IsRuntime> {
    src: &'a dyn GroupEventSource<R>,
    group: LocGroupId,
}

impl<'a, R: IsRuntime> GroupStore<'a, R> {
    #[must_use]
    pub(crate) fn new<S: GroupEventSource<R>>(src: &'a S, group: LocGroupId) -> Self {
        Self { src, group }
    }
}

impl<R: IsRuntime> GroupEventSource<R> for LocCtx<R> {
    fn stored_event_in(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        self.events_by_group
            .get(&group)?
            .bodies
            .get(slot.0 as usize)
            .cloned()
    }

    fn sender_user_in(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.sender_to_user.get(&sid).copied()
    }

    fn sender_pk_in(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.sender_to_pk.get(&sid).copied()
    }

    fn data_in(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.data_by_id.get(did.0 as usize).cloned()
    }
}

impl<R: IsRuntime> EventStore<R> for GroupStore<'_, R> {
    fn stored_event(&self, slot: GroupEventId) -> Option<StoredEvent<R::Body>> {
        self.src.stored_event_in(self.group, slot)
    }

    fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.src.sender_user_in(sid)
    }

    fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.src.sender_pk_in(sid)
    }

    fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.src.data_in(did)
    }
}

pub trait EventContext<R: IsRuntime> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId;
    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId;
    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId;
    fn store_event(
        &mut self,
        group: LocGroupId,
        event: StoredEvent<R::Body>,
    ) -> Option<StoreResultSuccess>;
    fn mk_data(&mut self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError>;
    fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId>;
}

impl<R: IsRuntime> EventContext<R> for LocCtx<R> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId {
        if let Some(&luid) = self.user_id_to_local.get(&uid) {
            return luid;
        }
        let luid = LocUserId(self.user_id_to_local.len() as u64);
        self.user_id_to_local.insert(uid, luid);
        self.local_to_user_id.insert(luid, uid);
        luid
    }

    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        if let Some(uid_val) = uid {
            self.mk_loc_user(uid_val);
        }

        if let Some(&existing_sid) = self.pk_to_sender.get(&pk) {
            return existing_sid;
        }

        let sid = LocSenderId(self.pk_to_sender.len() as u64);
        self.pk_to_sender.insert(pk, sid);
        self.sender_to_pk.insert(sid, pk);

        if let Some(uid_val) = uid {
            let lid = self.user_id_to_local[&uid_val];
            self.sender_to_user.insert(sid, lid);
        }

        sid
    }

    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        let key = (msg_type, group);
        if let Some(&gid) = self.group_by_key.get(&key) {
            return gid;
        }
        let gid = LocGroupId(self.group_by_key.len() as u64);
        let (mt, gv) = key.clone();
        self.group_by_key.insert(key, gid);
        self.group_by_id.insert(gid, (mt, gv));
        gid
    }

    fn store_event(
        &mut self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> Option<StoreResultSuccess> {
        let g = self.events_by_group.entry(group).or_default();
        let key = (ev.sender, ev.tx_id);
        let new_key = (ev.timestamp, ev.source_node);

        // Supersede detection: same `(sender, tx_id)` already stored. The
        // freshness tiebreak is `(timestamp, source_node)`, earliest-wins (a
        // stored observation that is at-least-as-early makes the incoming one
        // stale). Same logic as before — only the key lost `global_core_id`
        // (now implied by the group).
        if let Some(&old_slot) = g.dedup.get(&key) {
            let old_ev = &g.bodies[old_slot as usize];
            let old_key = (old_ev.timestamp, old_ev.source_node);

            // (eid, ev.timestamp, ev.source_node) is unique in a network where source_node's are never duplicated.
            // TODO: Make sure that "source_node is unique" is always the case.
            if old_key <= new_key {
                return None; // Event isn't earlier, skip it
            }

            // Supersede: append the fresh body, repoint dedup at it, and record
            // the old slot removed (its body stays in `bodies`).
            let new_slot = g.bodies.len() as u64;
            g.bodies.push(ev);
            g.dedup.insert(key, new_slot);
            g.added.push(GroupEventId(new_slot));
            g.removed.push(GroupEventId(old_slot));
            return Some(StoreResultSuccess {
                old: Some(GroupEventId(old_slot)),
                new: GroupEventId(new_slot),
            });
        }

        // Fresh identity.
        let new_slot = g.bodies.len() as u64;
        g.bodies.push(ev);
        g.dedup.insert(key, new_slot);
        g.added.push(GroupEventId(new_slot));
        Some(StoreResultSuccess {
            old: None,
            new: GroupEventId(new_slot),
        })
    }

    // fn loc_ctx(&self) -> &LocCtx<R> {
    //     self
    // }

    fn mk_data(&mut self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError> {
        if let Some(&existing) = self.data_id_to_local.get(&data_id) {
            return Ok(existing);
        }
        let computed_hash =
            R::hash_data(&content, self).map_err(DataVerifyError::UnresolvableId)?;
        if computed_hash != data_id.hash {
            return Err(DataVerifyError::HashMismatch {
                claimed: data_id,
                computed_hash,
            });
        }
        let did = LocDataId(self.data_by_id.len() as u64);
        self.data_id_to_local.insert(data_id, did);
        self.data_by_id.push((data_id, content));
        Ok(did)
    }

    fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId> {
        self.data_id_to_local.get(data_id).copied()
    }
}
