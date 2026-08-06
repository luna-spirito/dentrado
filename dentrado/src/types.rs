use std::{mem::size_of, num::NonZero};

// Wire-localization contract + local-id newtypes live in `dentrado-types`
// (compio-free, client-reachable); re-exported so `dentrado::types::*` keeps
// resolving.
pub use dentrado_types::{
    LocDataId, LocSenderEventId, LocSenderId, LocUserId, Localizable, Remapper,
};

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
    /// Derive a placement id from a content hash by taking its low 32 bits.
    /// Combined with [`GlobalCoreId::route`] this is the whole of "route by
    /// hash": a value's `global_hash` → `from_hash` → `route(num_cores)`.
    /// Only routing truncates; the content-hash path keeps the full 32 bytes.
    #[must_use]
    pub fn from_hash(hash: [u8; 32]) -> Self {
        Self(u32::from_le_bytes([hash[0], hash[1], hash[2], hash[3]]))
    }

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

/// A value that knows its own global content hash.
///
/// Supertrait of [`Localizable`] because anything content-addressed or
/// routable must also be localizable. The hash is on the *value*, not the
/// runtime: routing becomes `value.global_hash(resolver)` →
/// [`GlobalCoreId::from_hash`] → [`GlobalCoreId::route`], and content
/// addressing keeps the full `[u8; 32]`.
///
/// `resolver` resolves embedded [`LocDataId`] refs — the reason the old
/// `IsRuntime::hash_data` took a resolver (see `storage/in_memory.rs`). For
/// id/key types with no embedded content-addressed refs this is a pure
/// `Hash` fold that ignores the resolver.
pub trait GlobalHash: Localizable {
    fn global_hash(&self, resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError>;
}

impl GlobalHash for () {
    fn global_hash(&self, _resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        Ok([0; 32])
    }
}

/// Content hash of a `LocDataId` = the hash of the global [`DataId`] it
/// resolves to (`timestamp || hash`). This is the canonical way to
/// content-address a local data id; routing then truncates via
/// [`GlobalCoreId::from_hash`].
impl GlobalHash for LocDataId {
    fn global_hash(&self, resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        let resolved = resolver.resolve_data(*self)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&resolved.timestamp.to_le_bytes());
        hasher.update(&resolved.hash);
        Ok(*hasher.finalize().as_bytes())
    }
}

/// An `i64` used as a routing key hashes via blake3 of its little-endian bytes
/// (deterministic placement; matches the former hand-written runtime impl).
impl GlobalHash for i64 {
    fn global_hash(&self, _resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.to_le_bytes());
        Ok(*hasher.finalize().as_bytes())
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

#[cfg(test)]
mod localizable_derive_tests {
    use super::*;

    /// Polls a non-blocking future to completion on the current thread.
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        use std::sync::Arc;
        use std::task::{Context, Poll, Wake, Waker};
        struct Noop;
        impl Wake for Noop {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(Noop));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        match fut.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("future yielded unexpectedly"),
        }
    }

    /// Test remapper: shifts every local id by a fixed delta so we can observe
    /// which fields were actually remapped.
    struct ShiftRemapper;
    impl Remapper for ShiftRemapper {
        type Err = ();
        async fn remap_user(&mut self, uid: LocUserId) -> Result<LocUserId, Self::Err> {
            Ok(LocUserId(uid.0 + 100))
        }
        async fn remap_sender(&mut self, sid: LocSenderId) -> Result<LocSenderId, Self::Err> {
            Ok(LocSenderId(sid.0 + 100))
        }
        async fn remap_data(&mut self, did: LocDataId) -> Result<LocDataId, Self::Err> {
            Ok(LocDataId(did.0 + 100))
        }
    }

    // Named-field struct recursing into a `Localizable` field and skipping a
    // plain-data field.
    #[derive(Debug, PartialEq, Localizable)]
    struct Mixed {
        user: LocUserId,
        #[localizable(skip)]
        tag: String,
        note: &'static str,
    }

    #[derive(Debug, PartialEq, Localizable)]
    enum TestEnum {
        Unit,
        Sender(LocSenderId),
        Data {
            d: LocDataId,
            #[localizable(skip)]
            opaque: u64,
        },
        #[localizable(skip)]
        SkipWhole(u64, &'static str),
    }

    #[test]
    fn struct_recurses_and_skips() {
        let val = Mixed {
            user: LocUserId(5),
            tag: String::from("hi"),
            note: "x",
        };
        let out = block_on(val.localize(&mut ShiftRemapper)).unwrap();
        assert_eq!(out.user, LocUserId(105));
        assert_eq!(out.tag, "hi");
        assert_eq!(out.note, "x");
    }

    #[test]
    fn enum_all_shapes() {
        let unit = block_on(TestEnum::Unit.localize(&mut ShiftRemapper)).unwrap();
        assert_eq!(unit, TestEnum::Unit);

        let sender =
            block_on(TestEnum::Sender(LocSenderId(7)).localize(&mut ShiftRemapper)).unwrap();
        assert_eq!(sender, TestEnum::Sender(LocSenderId(107)));

        let data = block_on(
            TestEnum::Data {
                d: LocDataId(9),
                opaque: 42,
            }
            .localize(&mut ShiftRemapper),
        )
        .unwrap();
        assert_eq!(
            data,
            TestEnum::Data {
                d: LocDataId(109),
                opaque: 42,
            }
        );

        let skipped = block_on(TestEnum::SkipWhole(1, "z").localize(&mut ShiftRemapper)).unwrap();
        assert_eq!(skipped, TestEnum::SkipWhole(1, "z"));
    }
}
