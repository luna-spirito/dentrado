use std::{fmt::Debug, hash::Hash, num::NonZero};

use crate::{
    core::{core_ctx::GearCtx, storage::Storage},
    types::{GlobalCoreId, GlobalResolver, GroupRouteError, LocGroupId, LocMsgTypeId, Localizable},
};

/// How a gear is driven and where it lives, returned by [`IsRuntime::meta`].
///
/// Every gear is routed to its owning core by `group` (via
/// [`IsRuntime::route_group`]). What differs is the *trigger* for re-running:
///
/// - [`GearMeta::Event`] gears are subscribed to an event group and rerun
///   whenever new events land in it (the original model).
/// - [`GearMeta::Timer`] gears ("oracles") are attached to the core's epoch
///   counter and rerun at most once per `period` epochs while they have
///   interest. They poll an outside system on that cadence — e.g. a git
///   `fetch` or a remote API pull.
#[derive(Debug, Clone)]
pub enum GearMeta<R: IsRuntime> {
    Event {
        msg_type: LocMsgTypeId,
        group: R::Group,
    },
    Timer {
        group: R::Group,
        /// Minimum number of epochs between two timer-triggered (`tick = true`)
        /// runs. With the core's 1-second epoch interval this is "at most once
        /// per `period` seconds."
        period: NonZero<u64>,
    },
}

impl<R: IsRuntime> GearMeta<R> {
    /// The routing group, common to both variants (used to pick the owning
    /// core via [`IsRuntime::route_group`]).
    pub fn group(&self) -> &R::Group {
        match self {
            GearMeta::Event { group, .. } | GearMeta::Timer { group, .. } => group,
        }
    }
}

/// What triggered this `run_step` invocation, passed in place of the old
/// `Option<LocGroupId>`.
///
/// - [`GearInput::Events`] — the gear is event-driven; query events in `group`
///   since the last run (the original `Some(group)` case).
/// - [`GearInput::Timer`] — the gear is an oracle. `tick` is `true` iff the
///   epoch counter has advanced past the gear's `next_due` since the last
///   tick-run — i.e. the timer fired and the oracle may poll the outside
///   system now. `tick` is `false` when this run was triggered by something
///   else (a dependency changed, or the gear was just activated) *and* the
///   timer isn't due yet — recompute cheaply from cached/already-pulled data,
///   don't hit the outside system. The runtime enforces the `period` rate
///   limit, so a `tick = true` run is never issued more often than `period`
///   epochs apart.
#[derive(Debug, Clone, Copy)]
pub enum GearInput {
    Events(LocGroupId),
    Timer { tick: bool },
}

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
    type GearCache<Watermark>: Debug + Clone + 'static
    where
        Watermark: Debug + Clone + 'static;

    fn hash_data(
        data: &Self::Data,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError>;

    fn route_group(
        key: &Self::Group,
        resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError>;

    fn meta(gear: &Self::GearId) -> GearMeta<Self>;

    fn make_cache<Watermark: Debug + Clone + Default + 'static>(
        gear: &Self::GearId,
    ) -> Self::GearCache<Watermark>;

    /// Compute (or incrementally update) the gear's output.
    ///
    /// `ctx` carries the gear's id, the live `Core` (via `Deref`), and the
    /// `secondary_get` entry point for declaring gear→gear dependencies.
    /// Must not hold any `inner` borrow across `.await` (the core's `RefCell`
    /// is shared between all concurrently-polled gears on this core).
    async fn run_step<S: Storage<Self>>(
        ctx: &mut GearCtx<Self, S>,
        input: GearInput,
        cache: &mut Self::GearCache<S::Watermark>,
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
    type GearCache<Watermark>
        = ()
    where
        Watermark: Debug + Clone + 'static;

    fn route_group(
        _key: &Self::Group,
        _resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, crate::types::GroupRouteError> {
        Ok(GlobalCoreId(0))
    }

    fn meta(_gear: &Self::GearId) -> GearMeta<Self> {
        GearMeta::Event {
            msg_type: LocMsgTypeId(0),
            group: (),
        }
    }

    fn make_cache<Watermark: Debug + Clone + Default + 'static>(
        _gear: &Self::GearId,
    ) -> Self::GearCache<Watermark> {
    }

    async fn run_step<S: Storage<Self>>(
        _ctx: &mut crate::core::core_ctx::GearCtx<Self, S>,
        _input: GearInput,
        _cache: &mut Self::GearCache<S::Watermark>,
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
