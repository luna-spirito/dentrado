use std::{fmt::Debug, hash::Hash};

use crate::{
    core::core_ctx::Core,
    types::{GlobalCoreId, GlobalResolver, GroupRouteError, LocGroupId, LocMsgTypeId, Localizable},
};

pub trait IsRuntime: Debug + Send + Sync + Sized + 'static {
    type GearId: Debug + Hash + Eq + Clone + Send + 'static + Localizable;

    type GearOut: Debug + Clone + Send + 'static + Localizable;

    type Module: Debug + Send + Sync + 'static;

    type Group: Debug + Clone + Hash + Eq + Send + Sync + 'static + Localizable;

    type Body: Debug + Clone + Send + Sync + 'static + Localizable;

    type Data: Debug + Clone + Hash + Eq + Send + Sync + 'static + Localizable;

    /// Per-gear working state carried across `run_step` invocations.
    ///
    /// A concrete runtime (e.g. `FadenoRuntime`) may set this to a single concrete
    /// type and skip all type erasure. A runtime whose gears need heterogeneous
    /// cache payloads can instead erase them, e.g. `type GearCache = Box<dyn Any>`
    /// (or a tagged pointer) and downcast inside `run_step`.
    type GearCache: Debug + 'static;

    fn hash_data(
        data: &Self::Data,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError>;

    fn route_group(
        key: &Self::Group,
        resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError>;

    fn meta(gear: &Self::GearId) -> (LocMsgTypeId, Self::Group);

    fn make_cache(gear: &Self::GearId) -> Self::GearCache;

    fn run_step(
        gear: &Self::GearId,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut Self::GearCache,
    ) -> Self::GearOut;
}

#[derive(Debug)]
pub(crate) struct EmptyRuntime;
impl IsRuntime for EmptyRuntime {
    type GearId = ();
    type GearOut = ();
    type Module = ();
    type Group = ();
    type Body = ();
    type Data = ();
    type GearCache = ();

    fn route_group(
        _key: &Self::Group,
        _resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, crate::types::GroupRouteError> {
        Ok(GlobalCoreId(0))
    }

    fn meta(_gear: &Self::GearId) -> (crate::types::LocMsgTypeId, Self::Group) {
        (LocMsgTypeId(0), ())
    }

    fn make_cache(_gear: &Self::GearId) -> Self::GearCache {}

    fn run_step(
        _gear: &Self::GearId,
        _core: &crate::core::core_ctx::Core<Self>,
        _group: Option<LocGroupId>,
        _cache: &mut Self::GearCache,
    ) -> Self::GearOut {
    }

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let hash = *blake3::Hasher::new().finalize().as_bytes();
        Ok(hash)
    }
}
