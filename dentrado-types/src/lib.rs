//! The wire-localization contract shared by the dentrado runtime and its
//! clients: the [`Localizable`] / [`Remapper`] traits (plus the derive), the
//! local-id newtypes they are defined over, and blanket impls.
//!
//! This crate is compio-free and wasm-safe: a client (e.g. `kolorinko-rt`)
//! depends on it directly to derive [`Localizable`] on its wire types, while
//! the `dentrado` runtime re-exports everything here as `dentrado::types::*`.

#![feature(box_take, async_trait_bounds)]

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
pub struct LocUserId(pub u64);

impl LocUserId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocSenderId(pub u64);

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
pub struct LocDataId(pub u64);

impl LocDataId {
    #[must_use]
    pub const fn new_debug(id: u64) -> Self {
        Self(id)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LocSenderEventId(pub LocSenderId, pub u32);

impl Localizable for LocSenderEventId {
    async fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        Ok(LocSenderEventId(self.0.localize(r).await?, self.1))
    }
}

/// Remaps the local-id leaves of a [`Localizable`] value from one core/storage
/// space to another. Implementations are the client/server wire-builder/merger
/// and each core's own storage.
pub trait Remapper {
    type Err;
    async fn remap_user(&mut self, uid: LocUserId) -> Result<LocUserId, Self::Err>;
    async fn remap_sender(&mut self, sid: LocSenderId) -> Result<LocSenderId, Self::Err>;
    async fn remap_data(&mut self, did: LocDataId) -> Result<LocDataId, Self::Err>;
}

/// A value whose local-id leaves can be rewritten via a [`Remapper`]. The
/// server repackages a shippable value through this before sending it (to
/// another core or to a client); clients reverse/apply it against their own
/// storage. See the `Localizable` derive for automatic field-wise recursion.
pub trait Localizable: Sized {
    async fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err>;
}

impl Localizable for LocUserId {
    async fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_user(self).await
    }
}
impl Localizable for LocSenderId {
    async fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_sender(self).await
    }
}
impl Localizable for LocDataId {
    async fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        remapper.remap_data(self).await
    }
}

macro_rules! impl_localizable_trivial {
    ($t:ty) => {
        impl Localizable for $t {
            async fn localize<R: Remapper>(self, _r: &mut R) -> Result<Self, R::Err> {
                Ok(self)
            }
        }
    };
}

impl_localizable_trivial!(i64);
impl_localizable_trivial!(bool);
impl_localizable_trivial!(());
impl_localizable_trivial!(u32);
impl_localizable_trivial!(u64);
impl_localizable_trivial!(usize);
impl_localizable_trivial!(String);
impl_localizable_trivial!(&'static str);
impl_localizable_trivial!(&'static std::path::Path);

impl<T: Localizable> Localizable for Option<T> {
    async fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        match self {
            Some(t) => Ok(Some(t.localize(r).await?)),
            None => Ok(None),
        }
    }
}

impl<A: Localizable, B: Localizable> Localizable for (A, B) {
    async fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        Ok((self.0.localize(r).await?, self.1.localize(r).await?))
    }
}

impl<A: Localizable, B: Localizable, C: Localizable> Localizable for (A, B, C) {
    async fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        Ok((
            self.0.localize(r).await?,
            self.1.localize(r).await?,
            self.2.localize(r).await?,
        ))
    }
}

impl<T: Localizable> Localizable for Box<T> {
    async fn localize<R: Remapper>(self, r: &mut R) -> Result<Self, R::Err> {
        let (inner, b) = Box::take(self);
        Ok(Box::write(b, inner.localize(r).await?))
    }
}

/// Re-export the derive macro so it lives at the same path as the trait
/// (`dentrado_types::Localizable` is both the trait and the derive, in their
/// respective namespaces — same convention as `serde::Serialize`).
pub use dentrado_macros::Localizable;
