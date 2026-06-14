use std::{any::Any, fmt::Debug, hash::Hash};

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

    fn hash_data(
        data: &Self::Data,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError>;

    fn route_group(
        key: &Self::Group,
        resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError>;

    fn meta(gear: &Self::GearId) -> (LocMsgTypeId, Self::Group);

    fn make_cache(gear: &Self::GearId) -> Box<dyn Any>;

    fn run_step(
        gear: &Self::GearId,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut dyn Any,
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

    fn route_group(
        _key: &Self::Group,
        _resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, crate::types::GroupRouteError> {
        Ok(GlobalCoreId(0))
    }

    fn meta(_gear: &Self::GearId) -> (crate::types::LocMsgTypeId, Self::Group) {
        (LocMsgTypeId(0), ())
    }

    fn make_cache(_gear: &Self::GearId) -> Box<dyn std::any::Any> {
        Box::new(())
    }

    fn run_step(
        _gear: &Self::GearId,
        _core: &crate::core::core_ctx::Core<Self>,
        _group: Option<LocGroupId>,
        _cache: &mut dyn std::any::Any,
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
