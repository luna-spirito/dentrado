use std::collections::HashMap;

use crate::{
    core::gear::IsRuntime,
    types::{
        AnyLocEventId, DataId, DataVerifyError, GlobalCoreId, GlobalResolver, GroupRouteError,
        LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

#[derive(Clone, Debug)]
pub struct StoredEvent<B> {
    pub group: LocGroupId,
    pub sender: LocSenderId,
    pub global_core_id: GlobalCoreId,
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
/// body by its `AnyLocEventId` = `(this group, slot)`. Superseded bodies stay in
/// `bodies` (still fetchable); "removal" is purely a `removed` entry, never a
/// deletion.
#[derive(Debug)]
pub(crate) struct EventGroup<B> {
    pub(crate) bodies: Vec<StoredEvent<B>>,
    /// `(sender, tx_id)` -> slot of the currently-live version.
    dedup: HashMap<(LocSenderId, u32), u32>,
    pub(crate) added: Vec<AnyLocEventId>,
    pub(crate) removed: Vec<AnyLocEventId>,
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
        eid: AnyLocEventId,
        f: impl Fn(&StoredEvent<R::Body>) -> F,
    ) -> Option<F> {
        self.events_by_group
            .get(&eid.0)?
            .bodies
            .get(eid.1 as usize)
            .map(f)
    }

    /// `(added, removed)` appended to `group`'s changelog since `since`, mapped
    /// through `f`. `None` if the group has no shard yet.
    pub(crate) fn query_events<F>(
        &self,
        group: LocGroupId,
        since: (usize, usize),
        f: impl Fn(&[AnyLocEventId], &[AnyLocEventId]) -> F,
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
    pub old: Option<AnyLocEventId>,
    pub new: AnyLocEventId,
}

/// Read-only access to a core's localised event/data/sender store.
///
/// Implemented both for `LocCtx` (which owns the data directly) and for `Core`
/// (which reaches the same data through a short-lived `inner` borrow). This is
/// what lets the Fadeno VM read events/data uniformly whether it's running in
/// construction mode (against a bare `&LocCtx`) or in gear-step mode (against a
/// `&Core`, where a long-lived borrow is impossible because `secondary_get`
/// takes `inner` mutably mid-step).
///
/// All methods return owned values (no borrows from `&self`) so that the trait
/// is object-safe and usable as `&dyn EventStore<R>` even behind a `RefCell`.
pub trait EventStore<R: IsRuntime> {
    fn stored_event(&self, eid: AnyLocEventId) -> Option<StoredEvent<R::Body>>;
    fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId>;
    fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk>;
    fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)>;
}

impl<R: IsRuntime> EventStore<R> for LocCtx<R> {
    fn stored_event(&self, eid: AnyLocEventId) -> Option<StoredEvent<R::Body>> {
        self.events_by_group
            .get(&eid.0)?
            .bodies
            .get(eid.1 as usize)
            .cloned()
    }

    fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.sender_to_user.get(&sid).copied()
    }

    fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.sender_to_pk.get(&sid).copied()
    }

    fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.data_by_id.get(did.0 as usize).cloned()
    }
}

pub trait EventContext<R: IsRuntime> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId;
    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId;
    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId;
    fn store_event(&mut self, event: StoredEvent<R::Body>) -> Option<StoreResultSuccess>;
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

    fn store_event(&mut self, ev: StoredEvent<R::Body>) -> Option<StoreResultSuccess> {
        let group = ev.group;
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
            let new_slot = g.bodies.len() as u32;
            g.bodies.push(ev);
            g.dedup.insert(key, new_slot);
            g.added.push(AnyLocEventId(group, new_slot));
            g.removed.push(AnyLocEventId(group, old_slot));
            return Some(StoreResultSuccess {
                old: Some(AnyLocEventId(group, old_slot)),
                new: AnyLocEventId(group, new_slot),
            });
        }

        // Fresh identity.
        let new_slot = g.bodies.len() as u32;
        g.bodies.push(ev);
        g.dedup.insert(key, new_slot);
        g.added.push(AnyLocEventId(group, new_slot));
        Some(StoreResultSuccess {
            old: None,
            new: AnyLocEventId(group, new_slot),
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
