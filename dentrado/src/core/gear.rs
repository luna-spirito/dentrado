use std::{fmt::Debug, hash::Hash};

use crate::{
    core::core_ctx::GearCtx,
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
    type GearCache: Debug + Clone + 'static;

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

    /// Compute (or incrementally update) the gear's output.
    ///
    /// `ctx` carries the gear's id, the live `Core` (via `Deref`), and the
    /// `secondary_get` entry point for declaring gear→gear dependencies.
    /// Must not hold any `inner` borrow across `.await` (the core's `RefCell`
    /// is shared between all concurrently-polled gears on this core).
    async fn run_step(
        ctx: &mut GearCtx<Self>,
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

    async fn run_step(
        _ctx: &mut crate::core::core_ctx::GearCtx<Self>,
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
