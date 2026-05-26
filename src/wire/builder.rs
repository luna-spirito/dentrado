use std::{cell::RefCell, collections::HashMap};

use crate::{
    core::{gear::Runtime, loc_ctx::LocCtx},
    types::{LocDataId, LocSenderId, LocUserId, Localizable, SenderPk, UserId},
    wire::format::WireLocCtx,
};

#[derive(Debug)]
pub enum BuildError {
    DataNotFound { did: LocDataId },
    UserNotFound { lid: LocUserId },
    SenderNotFound { sid: crate::types::LocSenderId },
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DataNotFound { did } => write!(f, "data {did:?} not found in LocCtx"),
            Self::UserNotFound { lid } => write!(f, "user {lid:?} not found in LocCtx"),
            Self::SenderNotFound { sid } => write!(f, "sender {sid:?} not found"),
        }
    }
}

impl std::error::Error for BuildError {}

pub struct WireLocCtxBuilder<'a, R: Runtime> {
    ctx: &'a LocCtx<R>,
    inner: RefCell<BuilderInner<R>>,
}

struct BuilderInner<R: Runtime> {
    users: Vec<UserId>,
    senders: Vec<(SenderPk, u32)>,
    objects: Vec<(crate::types::DataId, R::Data)>,

    user_to_wire: HashMap<u64, u32>,
    sender_to_wire: HashMap<u64, u32>,
    data_to_wire: HashMap<u64, u32>,
}

impl<'a, R: Runtime> WireLocCtxBuilder<'a, R> {
    #[must_use]
    pub fn new(ctx: &'a LocCtx<R>) -> Self {
        Self {
            ctx,
            inner: RefCell::new(BuilderInner {
                users: Vec::new(),
                senders: Vec::new(),
                objects: Vec::new(),
                user_to_wire: HashMap::new(),
                sender_to_wire: HashMap::new(),
                data_to_wire: HashMap::new(),
            }),
        }
    }

    pub fn remap<L: Localizable + Clone>(&self, obj: L) -> Result<L, BuildError> {
        let result = obj.localize(
            &mut |lid| self.remap_user(lid),
            &mut |sid| self.remap_sender(sid),
            &mut |did| self.remap_data(did),
        )?;
        Ok(result.unwrap_or(obj))
    }

    #[must_use]
    pub fn build(self) -> WireLocCtx<R> {
        let inner = self.inner.into_inner();
        WireLocCtx {
            users: inner.users,
            senders: inner.senders,
            data: inner.objects,
        }
    }

    fn remap_user(&self, lid: LocUserId) -> Result<LocUserId, BuildError> {
        let mut inner = self.inner.borrow_mut();

        if let Some(&wire_idx) = inner.user_to_wire.get(&lid.0) {
            return Ok(LocUserId(u64::from(wire_idx)));
        }

        let uid = self
            .ctx
            .user_by_local(lid)
            .ok_or(BuildError::UserNotFound { lid })?;

        let wire_idx = inner.users.len() as u32;
        inner.users.push(uid);

        inner.user_to_wire.insert(lid.0, wire_idx);
        Ok(LocUserId(u64::from(wire_idx)))
    }

    fn remap_data(&self, did: LocDataId) -> Result<LocDataId, BuildError> {
        {
            let inner = self.inner.borrow();
            if let Some(&wire_idx) = inner.data_to_wire.get(&did.0) {
                return Ok(LocDataId(u64::from(wire_idx)));
            }
        }

        let (data_id, content) = self
            .ctx
            .get_data(did)
            .ok_or(BuildError::DataNotFound { did })?;

        let localized = content
            .localize(
                &mut |lid| self.remap_user(lid),
                &mut |sid| self.remap_sender(sid),
                &mut |d| self.remap_data(d),
            )?
            .unwrap_or_else(|| content.clone());

        let wire_idx = {
            let mut inner = self.inner.borrow_mut();
            let wire_idx = inner.objects.len() as u32;
            inner.objects.push((*data_id, localized));
            inner.data_to_wire.insert(did.0, wire_idx);
            wire_idx
        };

        Ok(LocDataId(u64::from(wire_idx)))
    }

    fn remap_sender(&self, sid: crate::types::LocSenderId) -> Result<LocSenderId, BuildError> {
        {
            let inner = self.inner.borrow();
            if let Some(&wire_idx) = inner.sender_to_wire.get(&sid.0) {
                return Ok(LocSenderId(u64::from(wire_idx)));
            }
        }

        let lid = self
            .ctx
            .sender_user(sid)
            .ok_or(BuildError::SenderNotFound { sid })?;
        let user_wire_idx = self.remap_user(lid)?.0 as u32;

        let pk = self
            .ctx
            .sender_pk(sid)
            .ok_or(BuildError::SenderNotFound { sid })?;

        let wire_idx = {
            let mut inner = self.inner.borrow_mut();
            let wire_idx = inner.senders.len() as u32;
            inner.senders.push((pk, user_wire_idx));
            inner.sender_to_wire.insert(sid.0, wire_idx);
            wire_idx
        };

        Ok(LocSenderId(u64::from(wire_idx)))
    }
}
