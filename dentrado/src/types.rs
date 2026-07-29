use std::{mem::size_of, num::NonZero};

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

/// Per-group event identity: a slot indexing one group's event shard.
///
/// The group itself is **not** carried here. Storage is sharded by group, and
/// a gear is bound to exactly one group (`GearSource::Events(loc_group)`), so a
/// `LocEventId` is only ever interpreted within the single group whose shard
/// it indexes — the group is a property of the gear/store, never of each event.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GroupEventId(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocSenderId(pub(crate) u64);

impl LocSenderId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct LocUserId(pub(crate) u64);

impl LocUserId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocMsgTypeId(pub u64); // TODO: Switch to pub(crate)

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocGroupId(pub(crate) u64);

impl LocGroupId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GlobalCoreId(pub u32);

fn jump_consistent_hash(hash: u64, num_cores: i64) -> i32 {
    let mut b = -1i64;
    let mut j = 0i64;
    let mut key = hash;

    while j < num_cores {
        b = j;
        key = key.wrapping_mul(2862933555777941757).wrapping_add(1);

        let probability = (1i64 << 31) as f64 / ((key >> 33) + 1) as f64;
        j = ((b + 1) as f64 * probability) as i64;
    }

    b as i32
}

impl GlobalCoreId {
    #[must_use]
    pub fn route(&self, num_cores: NonZero<u32>) -> u32 {
        jump_consistent_hash(u64::from(self.0), i64::from(num_cores.get())) as u32
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRouteError {
    DataIdOutOfBounds { idx: u64, objects_len: usize },
    UserIdOutOfBounds { idx: u64, users_len: usize },
    ContextPlaceholder,
    DomainValue(&'static str),
}

impl std::fmt::Display for GroupRouteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DataIdOutOfBounds { idx, objects_len } => write!(
                f,
                "KolDataId({idx}) out of bounds (objects_len={objects_len})"
            ),
            Self::UserIdOutOfBounds { idx, users_len } => {
                write!(f, "KolUserId({idx}) out of bounds (users_len={users_len})")
            }
            Self::ContextPlaceholder => write!(f, "KolPrimary/KolSecondary in value"),
            Self::DomainValue(name) => write!(f, "domain value {name} in value"),
        }
    }
}

impl std::error::Error for GroupRouteError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DataId {
    pub timestamp: u32, // To ensure on-drive ordering instead of random writes
    pub hash: [u8; 32],
}

pub trait GlobalResolver {
    fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError>;
    fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataVerifyError {
    HashMismatch {
        claimed: DataId,
        computed_hash: [u8; 32],
    },
    UnresolvableId(GroupRouteError),
}

impl std::fmt::Display for DataVerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::HashMismatch {
                claimed,
                computed_hash,
            } => write!(
                f,
                "DataId hash mismatch: claimed {:?}, computed {:?}",
                claimed.hash, computed_hash
            ),
            Self::UnresolvableId(e) => write!(f, "unresolvable local ID: {e}"),
        }
    }
}

impl std::error::Error for DataVerifyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::UnresolvableId(e) => Some(e),
            _ => None,
        }
    }
}

#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct LocDataId(pub(crate) u64);

impl LocDataId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
pub struct Id(pub u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IdentityServerPk(pub [u8; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SenderPk(pub [u8; 32]);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct UserId {
    pub id: u64,
    pub identity_server_pk: IdentityServerPk,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Ed25519Signature(pub [u8; 64]);

#[allow(dead_code)]
#[derive(Clone, Debug)]
pub(crate) struct Attestation {
    pub(crate) user_id: u64,
    pub(crate) pk: SenderPk,
    pub(crate) timestamp: u64,
    pub(crate) serial: u64,
    pub(crate) signature: Ed25519Signature,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocSenderEventId(pub LocSenderId, pub GlobalCoreId, pub u32);

impl Localizable for LocSenderEventId {
    fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        Ok(LocSenderEventId(self.0.localize(r)?, self.1, self.2))
    }
}

#[repr(C, packed)]
#[derive(Clone, Copy)]
pub(crate) struct EventHeader {
    pub(crate) group: u64,
    pub(crate) sender: u64,
    pub(crate) tx_id: u32,
    pub(crate) timestamp: u32,
    pub(crate) body_len: u32,
}

const _: () = assert!(size_of::<EventHeader>() == 28);

#[allow(dead_code)]
impl EventHeader {
    pub(crate) const SIZE: usize = size_of::<EventHeader>();

    #[must_use]
    pub(crate) const fn record_disk_size(body_len: u32) -> usize {
        Self::SIZE + body_len as usize
    }
}

#[allow(dead_code)]
pub(crate) const META_TAG_SENDER: u64 = u32::MAX as u64;
#[allow(dead_code)]
pub(crate) const META_TAG_GROUP: u64 = (u32::MAX - 2) as u64;

#[allow(dead_code)]
pub(crate) const META_SENDER_RECORD_SIZE: usize = 8 + 32;

#[repr(C, packed)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct MetaGroupHeader {
    pub(crate) msg_type: LocMsgTypeId,
    pub(crate) body_len: u32,
}

const _: () = assert!(size_of::<MetaGroupHeader>() == 12);

#[allow(dead_code)]
pub(crate) const SEGMENT_SIZE_BYTES: usize = 256 * 1024 * 1024;

pub trait Remapper {
    type Err;
    fn remap_user(&mut self, uid: LocUserId) -> Result<LocUserId, Self::Err>;
    fn remap_sender(&mut self, sid: LocSenderId) -> Result<LocSenderId, Self::Err>;
    fn remap_data(&mut self, did: LocDataId) -> Result<LocDataId, Self::Err>;
}

pub trait Localizable: Sized {
    fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err>;
}

impl Localizable for LocUserId {
    fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_user(self)
    }
}
impl Localizable for LocSenderId {
    fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_sender(self)
    }
}
impl Localizable for LocDataId {
    fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_data(self)
    }
}

macro_rules! impl_localizable_trivial {
    ($t:ty) => {
        impl Localizable for $t {
            fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
                Ok(self)
            }
        }
    };
}

impl_localizable_trivial!(i64);
impl_localizable_trivial!(bool);
impl_localizable_trivial!(());

impl<T: Localizable> Localizable for Box<T> {
    fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        let (inner, b) = Box::take(self);
        Ok(Box::write(b, inner.localize(r)?))
    }
}

#[allow(dead_code)]
pub(crate) fn encode_varint(mut val: u64, buf: &mut Vec<u8>) {
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        buf.push(byte);
        if val == 0 {
            break;
        }
    }
}

#[must_use]
#[allow(dead_code)]
pub(crate) fn decode_varint(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut result = 0u64;
    let mut shift = 0;
    let mut pos = offset;
    loop {
        if pos >= data.len() {
            return None;
        }
        let byte = data[pos];
        pos += 1;
        result |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    Some((result, pos - offset))
}
