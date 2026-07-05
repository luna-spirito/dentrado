use std::collections::HashMap;

use crate::{
    core::{gear::IsRuntime, loc_ctx::LocCtx},
    types::{LocDataId, LocSenderId, LocUserId, Localizable, Remapper, SenderPk, UserId},
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

pub struct WireLocCtxBuilder<'a, R: IsRuntime>(BuilderInner<'a, R>);

pub struct BuilderInner<'a, R: IsRuntime> {
    ctx: &'a LocCtx<R>,
    users: Vec<UserId>,
    senders: Vec<(SenderPk, u32)>,
    objects: Vec<(crate::types::DataId, R::Data)>,

    user_to_wire: HashMap<u64, u32>,
    sender_to_wire: HashMap<u64, u32>,
    data_to_wire: HashMap<u64, u32>,
}

impl<'a, R: IsRuntime> WireLocCtxBuilder<'a, R> {
    #[must_use]
    pub fn new(ctx: &'a LocCtx<R>) -> Self {
        WireLocCtxBuilder(BuilderInner {
            ctx,
            users: Vec::new(),
            senders: Vec::new(),
            objects: Vec::new(),
            user_to_wire: HashMap::new(),
            sender_to_wire: HashMap::new(),
            data_to_wire: HashMap::new(),
        })
    }
    #[must_use]
    pub fn remap<L: Localizable>(&mut self, l: L) -> Result<L, BuildError> {
        l.localize(&mut self.0)
    }
    #[must_use]
    pub fn build(self) -> WireLocCtx<R> {
        WireLocCtx {
            users: self.0.users,
            senders: self.0.senders,
            data: self.0.objects,
        }
    }
}

impl<R: IsRuntime> Remapper for BuilderInner<'_, R> {
    type Err = BuildError;
    fn remap_user(&mut self, lid: LocUserId) -> Result<LocUserId, BuildError> {
        if let Some(&wire_idx) = self.user_to_wire.get(&lid.0) {
            return Ok(LocUserId(u64::from(wire_idx)));
        }

        let uid = self
            .ctx
            .user_by_local(lid)
            .ok_or(BuildError::UserNotFound { lid })?;

        let wire_idx = self.users.len() as u32;
        self.users.push(uid);

        self.user_to_wire.insert(lid.0, wire_idx);
        Ok(LocUserId(u64::from(wire_idx)))
    }

    fn remap_data(&mut self, did: LocDataId) -> Result<LocDataId, BuildError> {
        if let Some(&wire_idx) = self.data_to_wire.get(&did.0) {
            return Ok(LocDataId(u64::from(wire_idx)));
        }

        let (data_id, content) = self
            .ctx
            .get_data(did, Clone::clone)
            .ok_or(BuildError::DataNotFound { did })?;

        let localized = content.localize(self)?;

        let wire_idx = self.objects.len() as u32;
        self.objects.push((data_id, localized));
        self.data_to_wire.insert(did.0, wire_idx);

        Ok(LocDataId(u64::from(wire_idx)))
    }

    fn remap_sender(&mut self, sid: crate::types::LocSenderId) -> Result<LocSenderId, BuildError> {
        if let Some(&wire_idx) = self.sender_to_wire.get(&sid.0) {
            return Ok(LocSenderId(u64::from(wire_idx)));
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

        let wire_idx = self.senders.len() as u32;
        self.senders.push((pk, user_wire_idx));
        self.sender_to_wire.insert(sid.0, wire_idx);

        Ok(LocSenderId(u64::from(wire_idx)))
    }
}
