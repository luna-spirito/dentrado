use crate::{
    core::gear::IsRuntime,
    types::{
        DataId, LocDataId, LocMsgTypeId, LocSenderId, LocUserId, Localizable, SenderPk, UserId,
    },
};

#[derive(Clone, Debug)]
pub struct WireEventBody<K, B> {
    pub sender: LocSenderId,

    pub tx_id: u32,

    pub msg_type: LocMsgTypeId,

    pub group: K,

    pub body: B,
}

impl<K: Localizable + Clone, B: Localizable + Clone> Localizable for WireEventBody<K, B> {
    fn localize<U, S, D, E>(
        self,
        remap_user: &mut U,
        remap_sender: &mut S,
        remap_data: &mut D,
    ) -> Result<Self, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        let new_sender = remap_sender(self.sender)?;
        let new_group =
            self.group
                .localize(&mut *remap_user, &mut *remap_sender, &mut *remap_data)?;
        let new_body = self.body.localize(remap_user, remap_sender, remap_data)?;

        Ok(WireEventBody {
            sender: new_sender,
            tx_id: self.tx_id,
            msg_type: self.msg_type,
            group: new_group,
            body: new_body,
        })
    }
}

#[derive(Debug)]
pub struct WireLocCtx<R: IsRuntime> {
    pub users: Vec<UserId>,

    pub senders: Vec<(SenderPk, u32)>,

    pub data: Vec<(DataId, R::Data)>,
}

impl<R: IsRuntime> Clone for WireLocCtx<R>
where
    R::Data: Clone,
{
    fn clone(&self) -> Self {
        Self {
            users: self.users.clone(),
            senders: self.senders.clone(),
            data: self.data.clone(),
        }
    }
}

impl<R: IsRuntime> Default for WireLocCtx<R> {
    fn default() -> Self {
        Self {
            users: Vec::new(),
            senders: Vec::new(),
            data: Vec::new(),
        }
    }
}

impl<R: IsRuntime> crate::types::GlobalResolver for WireLocCtx<R> {
    fn resolve_user(
        &self,
        lid: crate::types::LocUserId,
    ) -> Result<crate::types::UserId, crate::types::GroupRouteError> {
        self.users.get(lid.0 as usize).copied().ok_or({
            crate::types::GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: self.users.len(),
            }
        })
    }

    fn resolve_data(
        &self,
        did: crate::types::LocDataId,
    ) -> Result<crate::types::DataId, crate::types::GroupRouteError> {
        self.data.get(did.0 as usize).map(|(id, _)| *id).ok_or({
            crate::types::GroupRouteError::DataIdOutOfBounds {
                idx: did.0,
                objects_len: self.data.len(),
            }
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClusterSignature;

#[derive(Debug)]
pub enum MergeError {
    UserOutOfBounds {
        idx: u64,
        len: usize,
    },
    SenderOutOfBounds {
        idx: u64,
        len: usize,
    },
    SenderUserOutOfBounds {
        sender_idx: u64,
        user_idx: u32,
        users_len: usize,
    },
    DataOutOfBounds {
        idx: u64,
        len: usize,
    },
    DataForwardReference {
        idx: u64,
        max_allowed: u64,
    },
    DataVerify(crate::types::DataVerifyError),
    Route(crate::types::GroupRouteError),
}

impl std::fmt::Display for MergeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UserOutOfBounds { idx, len } => {
                write!(f, "LocalUserId({idx}) out of bounds (users.len={len})")
            }
            Self::SenderOutOfBounds { idx, len } => {
                write!(f, "LocSenderId({idx}) out of bounds (senders.len={len})")
            }
            Self::SenderUserOutOfBounds {
                sender_idx,
                user_idx,
                users_len,
            } => write!(
                f,
                "sender[{sender_idx}] references user_idx={user_idx} \
                 but users.len()={users_len}"
            ),
            Self::DataOutOfBounds { idx, len } => {
                write!(f, "LocDataId({idx}) out of bounds (objects.len={len})")
            }
            Self::DataForwardReference { idx, max_allowed } => write!(
                f,
                "data[{idx}] forward-references data[{idx}] or later (max_allowed={max_allowed})"
            ),
            Self::DataVerify(e) => write!(f, "data verification failed: {e}"),
            Self::Route(e) => write!(f, "routing failed: {e}"),
        }
    }
}

impl std::error::Error for MergeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Route(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub enum RunGearError {
    Merge(MergeError),
    Route(crate::types::GroupRouteError),
}

impl std::fmt::Display for RunGearError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Merge(e) => write!(f, "wire context merge failed: {e}"),
            Self::Route(e) => write!(f, "group route failed: {e}"),
        }
    }
}

impl std::error::Error for RunGearError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Merge(e) => Some(e),
            Self::Route(e) => Some(e),
        }
    }
}

impl From<MergeError> for RunGearError {
    fn from(e: MergeError) -> Self {
        Self::Merge(e)
    }
}
