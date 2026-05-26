use std::collections::HashMap;

use crate::{
    core::gear::Runtime,
    types::{
        AnyLocEventId, DataId, DataVerifyError, GlobalCoreId, GlobalResolver, GroupRouteError,
        LocDataId, LocGroupId, LocMsgTypeId, LocSenderEventId, LocSenderId, LocUserId, SenderPk,
        UserId,
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

#[derive(Debug)]
pub struct LocCtx<R: Runtime> {
    pk_to_sender: HashMap<SenderPk, LocSenderId>,
    sender_to_pk: HashMap<LocSenderId, SenderPk>,
    sender_to_user: HashMap<LocSenderId, LocUserId>,

    user_id_to_local: HashMap<UserId, LocUserId>,
    local_to_user_id: HashMap<LocUserId, UserId>,

    events: Vec<StoredEvent<R::Body>>,
    sender_tx_index: HashMap<LocSenderEventId, AnyLocEventId>,

    data_by_id: Vec<(DataId, R::Data)>,
    data_id_to_local: HashMap<DataId, LocDataId>,

    group_by_key: HashMap<(LocMsgTypeId, R::Group), LocGroupId>,
    group_by_id: HashMap<LocGroupId, (LocMsgTypeId, R::Group)>,
}

impl<R: Runtime> LocCtx<R> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pk_to_sender: HashMap::new(),
            sender_to_pk: HashMap::new(),
            sender_to_user: HashMap::new(),
            user_id_to_local: HashMap::new(),
            local_to_user_id: HashMap::new(),
            events: Vec::new(),
            sender_tx_index: HashMap::new(),
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
    pub(crate) fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.sender_to_pk.get(&sid).copied()
    }

    #[must_use]
    pub fn get_stored_event(&self, eid: AnyLocEventId) -> Option<&StoredEvent<R::Body>> {
        self.events.get(eid.0 as usize)
    }

    pub(crate) fn find_event_by_sender_tx(&self, id: LocSenderEventId) -> Option<AnyLocEventId> {
        self.sender_tx_index.get(&id).copied()
    }

    #[must_use]
    pub fn get_data(&self, did: LocDataId) -> Option<&(DataId, R::Data)> {
        self.data_by_id.get(did.0 as usize)
    }

    #[must_use]
    pub(crate) fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId> {
        self.data_id_to_local.get(data_id).copied()
    }
}

impl<R: Runtime> GlobalResolver for LocCtx<R> {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        self.local_to_user_id.get(&lid).copied().ok_or({
            GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: self.user_id_to_local.len(),
            }
        })
    }

    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        self.data_by_id
            .get(did.0 as usize)
            .map(|(id, _)| *id)
            .ok_or({
                GroupRouteError::DataIdOutOfBounds {
                    idx: did.0,
                    objects_len: self.data_by_id.len(),
                }
            })
    }
}

impl<R: Runtime> LocCtx<R> {
    #[must_use]
    pub(crate) fn group_msg_type(&self, gid: LocGroupId) -> Option<LocMsgTypeId> {
        self.group_by_id.get(&gid).map(|(mt, _)| *mt)
    }

    #[must_use]
    pub(crate) fn group_value(&self, gid: LocGroupId) -> Option<&R::Group> {
        self.group_by_id.get(&gid).map(|(_, v)| v)
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

impl<R: Runtime> Default for LocCtx<R> {
    fn default() -> Self {
        Self::new()
    }
}

pub struct StoreResultSuccess {
    pub old: Option<AnyLocEventId>,
    pub new: AnyLocEventId,
}

pub trait EventContext<R: Runtime> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId;
    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId;
    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId;
    fn store_event(&mut self, event: StoredEvent<R::Body>) -> Option<StoreResultSuccess>;
    fn mk_data(&mut self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError>;

    fn loc_ctx(&self) -> &LocCtx<R>;
}

impl<R: Runtime> EventContext<R> for LocCtx<R> {
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
        let eid = AnyLocEventId(self.events.len() as u64);
        let sender_key = LocSenderEventId(ev.sender, ev.global_core_id, ev.tx_id);

        let old = if let Some(existing_eid) = self.find_event_by_sender_tx(sender_key) {
            let existing_ev = self
                .get_stored_event(existing_eid)
                .expect("sender_tx_index points to valid event");
            let old_key = (existing_ev.source_node, existing_ev.timestamp);
            let new_key = (ev.source_node, ev.timestamp);

            if old_key <= new_key {
                return None; // Event isn't earlier, skip it
            }

            Some(existing_eid)
        } else {
            None
        };
        self.sender_tx_index.insert(
            LocSenderEventId(ev.sender, ev.global_core_id, ev.tx_id),
            eid,
        );
        self.events.push(ev);
        Some(StoreResultSuccess { old, new: eid })
    }

    fn loc_ctx(&self) -> &LocCtx<R> {
        self
    }

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
}
