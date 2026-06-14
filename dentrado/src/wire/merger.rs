use std::cell::RefCell;

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{EventContext, StoreResultSuccess, StoredEvent},
    },
    types::{GlobalCoreId, LocDataId, LocSenderId, LocUserId, Localizable, NodeId},
    wire::format::{MergeError, WireEventBody, WireLocCtx},
};

pub(crate) struct WireLocCtxMerger<'a, R: IsRuntime, Target: EventContext<R>> {
    source: &'a WireLocCtx<R>,
    inner: RefCell<MergerInner<'a, Target>>,
}

struct MergerInner<'a, Target> {
    target: &'a Target,
    user_map: Vec<Option<LocUserId>>,
    sender_map: Vec<Option<LocSenderId>>,
    data_map: Vec<Option<LocDataId>>,
}

impl<'a, R: IsRuntime, Target: EventContext<R>> WireLocCtxMerger<'a, R, Target> {
    pub(crate) fn new(source: &'a WireLocCtx<R>, target: &'a Target) -> Self {
        Self {
            source,
            inner: RefCell::new(MergerInner {
                target,
                user_map: vec![None; source.users.len()],
                sender_map: vec![None; source.senders.len()],
                data_map: vec![None; source.data.len()],
            }),
        }
    }

    pub(crate) fn remap<L: Localizable + Clone>(&self, obj: L) -> Result<L, MergeError> {
        let max_data = self.source.data.len();
        let result = obj.localize(
            &mut |lid| self.remap_user(lid),
            &mut |sid| self.remap_sender(sid),
            &mut |did| self.remap_data(did, max_data),
        )?;
        Ok(result.unwrap_or(obj))
    }

    pub(crate) fn import_new_event(
        &self,
        event: &WireEventBody<R::Group, R::Body>,
        global_core_id: GlobalCoreId,
        timestamp: u32,
        source_node: NodeId,
    ) -> Result<Option<StoreResultSuccess>, MergeError> {
        let sender = self.remap_sender(event.sender)?;
        let group = self.remap_value(&event.group)?;
        let body = self.remap_value(&event.body)?;

        let inner = self.inner.borrow_mut();
        let group_id = inner.target.mk_loc_group(event.msg_type, group);
        Ok(inner.target.store_event(StoredEvent {
            group: group_id,
            sender,
            global_core_id,
            tx_id: event.tx_id,
            timestamp,
            source_node,
            body,
        }))
    }

    fn remap_user(&self, lid: LocUserId) -> Result<LocUserId, MergeError> {
        let idx = lid.0 as usize;

        if idx >= self.source.users.len() {
            return Err(MergeError::UserOutOfBounds {
                idx: lid.0,
                len: self.source.users.len(),
            });
        }

        let mut inner = self.inner.borrow_mut();

        if let Some(mapped) = inner.user_map[idx] {
            return Ok(mapped);
        }

        let uid = self.source.users[idx];

        let local_id = inner.target.mk_loc_user(uid);
        inner.user_map[idx] = Some(local_id);
        Ok(local_id)
    }

    fn remap_sender(&self, sid: LocSenderId) -> Result<LocSenderId, MergeError> {
        let idx = sid.0 as usize;

        if idx >= self.source.senders.len() {
            return Err(MergeError::SenderOutOfBounds {
                idx: sid.0,
                len: self.source.senders.len(),
            });
        }

        let inner = self.inner.borrow();

        if let Some(mapped) = inner.sender_map[idx] {
            return Ok(mapped);
        }

        let (pk, user_idx) = &self.source.senders[idx];
        let user_idx_val = *user_idx as usize;

        drop(inner);
        self.remap_user(LocUserId(user_idx_val as u64))?;

        if user_idx_val >= self.source.users.len() {
            return Err(MergeError::SenderUserOutOfBounds {
                sender_idx: sid.0,
                user_idx: *user_idx,
                users_len: self.source.users.len(),
            });
        }
        let uid = self.source.users[user_idx_val];

        let mut inner = self.inner.borrow_mut();
        let local_id = inner.target.mk_loc_sender(*pk, Some(uid));
        inner.sender_map[idx] = Some(local_id);
        Ok(local_id)
    }

    fn remap_data(&self, did: LocDataId, max_allowed: usize) -> Result<LocDataId, MergeError> {
        let idx = did.0 as usize;

        if idx >= self.source.data.len() {
            return Err(MergeError::DataOutOfBounds {
                idx: did.0,
                len: self.source.data.len(),
            });
        }

        if idx >= max_allowed {
            return Err(MergeError::DataForwardReference {
                idx: did.0,
                max_allowed: max_allowed as u64,
            });
        }

        {
            let inner = self.inner.borrow();
            if let Some(mapped) = inner.data_map[idx] {
                return Ok(mapped);
            }
        }

        let (data_id, content) = &self.source.data[idx];

        {
            let inner = self.inner.borrow();
            if let Some(existing) = inner.target.loc_ctx().find_data_by_data_id(data_id) {
                drop(inner);
                self.inner.borrow_mut().data_map[idx] = Some(existing);
                return Ok(existing);
            }
        }

        let next_max = idx; // data[i]'s content may only reference data[0..i)
        let localized = content
            .localize(
                &mut |lid| self.remap_user(lid),
                &mut |sid| self.remap_sender(sid),
                &mut |d| self.remap_data(d, next_max),
            )?
            .unwrap_or_else(|| content.clone());

        let new_did = self
            .inner
            .borrow_mut()
            .target
            .mk_data(*data_id, localized)
            .map_err(MergeError::DataVerify)?;

        self.inner.borrow_mut().data_map[idx] = Some(new_did);
        Ok(new_did)
    }

    fn remap_value<V: Localizable + Clone>(&self, value: &V) -> Result<V, MergeError> {
        let max_data = self.source.data.len();
        let result = value.localize(
            &mut |lid| self.remap_user(lid),
            &mut |sid| self.remap_sender(sid),
            &mut |did| self.remap_data(did, max_data),
        )?;
        Ok(result.unwrap_or_else(|| value.clone()))
    }
}
