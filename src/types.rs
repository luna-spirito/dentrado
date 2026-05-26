use std::mem::size_of;

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub u32);

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
pub struct AnyLocEventId(pub u64);

#[repr(transparent)]
pub struct LocEventId<T>(pub AnyLocEventId, pub std::marker::PhantomData<T>);

impl<T> Clone for LocEventId<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for LocEventId<T> {}

impl<T> std::fmt::Debug for LocEventId<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T> PartialEq for LocEventId<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}
impl<T> Eq for LocEventId<T> {}

impl<T> std::hash::Hash for LocEventId<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl<T> PartialOrd for LocEventId<T> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for LocEventId<T> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl<T> LocEventId<T> {
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(AnyLocEventId(id), std::marker::PhantomData)
    }

    #[must_use]
    pub const fn to_any(self) -> AnyLocEventId {
        self.0
    }
}

impl<T> rkyv::Archive for LocEventId<T> {
    type Archived = <AnyLocEventId as rkyv::Archive>::Archived;
    type Resolver = <AnyLocEventId as rkyv::Archive>::Resolver;

    const COPY_OPTIMIZATION: rkyv::traits::CopyOptimization<Self> =
        rkyv::traits::CopyOptimization::disable();

    fn resolve(&self, resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) {
        self.0.resolve(resolver, out);
    }
}

impl<T, S: rkyv::rancor::Fallible + ?Sized> rkyv::Serialize<S> for LocEventId<T> {
    fn serialize(
        &self,
        serializer: &mut S,
    ) -> Result<Self::Resolver, <S as rkyv::rancor::Fallible>::Error> {
        self.0.serialize(serializer)
    }
}

impl<T, D: rkyv::rancor::Fallible + ?Sized> rkyv::Deserialize<LocEventId<T>, D>
    for <LocEventId<T> as rkyv::Archive>::Archived
{
    fn deserialize(
        &self,
        deserializer: &mut D,
    ) -> Result<LocEventId<T>, <D as rkyv::rancor::Fallible>::Error> {
        let any = <<AnyLocEventId as rkyv::Archive>::Archived as rkyv::Deserialize<
            AnyLocEventId,
            D,
        >>::deserialize(self, deserializer)?;
        Ok(LocEventId(any, std::marker::PhantomData))
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocSenderId(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocUserId(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocMsgTypeId(pub u64);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocGroupId(pub u64);

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
    pub fn route(&self, num_cores: u32) -> u32 {
        jump_consistent_hash(u64::from(self.0), i64::from(num_cores)) as u32
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
    pub timestamp: u32,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocDataId(pub u64);

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

pub trait Localizable: Sized {
    fn localize<U, S, D, E>(
        &self,
        remap_user: &mut U,
        remap_sender: &mut S,
        remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>;
}

macro_rules! impl_localizable_trivial {
    ($t:ty) => {
        impl Localizable for $t {
            fn localize<U, S, D, E>(
                &self,
                _remap_user: &mut U,
                _remap_sender: &mut S,
                _remap_data: &mut D,
            ) -> Result<Option<Self>, E>
            where
                U: FnMut(LocUserId) -> Result<LocUserId, E>,
                S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
                D: FnMut(LocDataId) -> Result<LocDataId, E>,
            {
                Ok(None)
            }
        }
    };
}

impl_localizable_trivial!(i64);
impl_localizable_trivial!(bool);
impl_localizable_trivial!(());

impl<T: Localizable> Localizable for Box<T> {
    fn localize<U, S, D, E>(
        &self,
        remap_user: &mut U,
        remap_sender: &mut S,
        remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        let inner = (**self).localize(remap_user, remap_sender, remap_data)?;
        Ok(inner.map(Box::new))
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
