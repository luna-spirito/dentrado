use std::{fmt::Debug, hash::Hash, num::NonZero};

use crate::{
    core::{core_ctx::GearCtx, storage::Storage},
    types::{GlobalHash, LocGroupId, LocMsgTypeId, Localizable},
};

/// How a gear is driven and where it lives, returned by [`IsRuntime::meta`].
///
/// Every gear is routed to its owning core by `group` (the group value's
/// [`GlobalHash`] → [`GlobalCoreId`](crate::types::GlobalCoreId)). What differs
/// is the *trigger* for re-running:
///
/// - [`GearMeta::Event`] gears are subscribed to an event group and rerun
///   whenever new events land in it (the original model).
/// - [`GearMeta::Timer`] gears ("oracles") are attached to the core's epoch
///   counter and rerun at most once per `period` epochs while they have
///   interest. They poll an outside system on that cadence — e.g. a git
///   `fetch` or a remote API pull.
/// - [`GearMeta::Follow`] gears are subscribed, directly, to the result of
///   other gear on the same core.
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
    Follow {
        gear: R::GearId,
        /// Optimization: we bake target group here, so that the database isn't stuck
        /// in recursion trying to figure out the root group.
        baked_group: R::Group,
    },
}

impl<R: IsRuntime> GearMeta<R> {
    /// The routing group, common to both variants (used to pick the owning
    /// core via the group's [`GlobalHash`]).
    pub fn group(&self) -> &R::Group {
        match self {
            GearMeta::Event { group, .. } | GearMeta::Timer { group, .. } => group,
            GearMeta::Follow { baked_group, .. } => baked_group,
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
pub enum GearInput<R: IsRuntime> {
    Events(LocGroupId),
    Timer { tick: bool },
    Follow { out: R::GearOut },
}

pub trait IsRuntime: Debug + Send + Sync + Sized + 'static {
    type GearId: Debug + Hash + Eq + Clone + Send + 'static + Localizable;

    type GearOut: Debug + Clone + Send + 'static + Localizable;

    type Module: Debug + Send + Sync + 'static;

    type Group: Debug + Clone + Hash + Eq + Send + Sync + 'static + GlobalHash;

    type Body: Debug + Clone + Send + Sync + 'static + Localizable;

    type Data: Debug + Clone + Hash + Eq + Send + Sync + 'static + GlobalHash;

    /// Per-gear working state carried across `run_step` invocations.
    ///
    /// A concrete runtime (e.g. `FadenoRuntime`) may set this to a single concrete
    /// type and skip all type erasure. A runtime whose gears need heterogeneous
    /// cache payloads can instead erase them, e.g. `type GearCache = Box<dyn Any>`
    /// (or a tagged pointer) and downcast inside `run_step`.
    type GearCache<Watermark>: Debug + Clone + 'static
    where
        Watermark: Debug + Clone + 'static;

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
        input: GearInput<Self>,
        cache: &mut Self::GearCache<S::Watermark>,
    ) -> Self::GearOut;
}

/// A deferred, composable read against a gear's output — the typed layer over
/// the raw `GearCtx::secondary_get` (which returns an untyped `GearOut`).
///
/// `id` names the gear (with its id fields); `getter` extracts `Out` out of the
/// matching `GearOut` variant. The `#[gears]` macro pairs them per variant, so
/// by construction `getter` is always fed the variant it matches — the
/// `unreachable!` arm in it is a defensive invariant, not an expected path.
///
/// Built by the `#[gears]` macro as one fn per gear, e.g.
/// `pub fn repo(repo_meta: RepoMeta) -> GearQuery<R, Arc<RepoData>>`; methods
/// like [`GearQuery::secondary_get`] are the composable surface.
pub struct GearQuery<R: IsRuntime, Out> {
    /// Queried gear.
    pub id: R::GearId,
    /// Getter that extracts `Out` out of the result. Must be used with the
    /// response provided by `id`, else panics.
    pub getter: fn(R::GearOut) -> Out,
}

// Manual (not derived) so we don't add `Out: Clone` / `R: Clone` bounds: the
// id is `Clone` via the `IsRuntime` associated-type bound and a `fn` pointer
// is `Copy`, which is all `clone` needs.
impl<R: IsRuntime, Out> Clone for GearQuery<R, Out> {
    fn clone(&self) -> Self {
        GearQuery {
            id: self.id.clone(),
            getter: self.getter,
        }
    }
}

impl<R: IsRuntime, Out> GearQuery<R, Out> {
    /// Declare a dependency on this gear's output and pull its current value
    /// (awaiting it if not yet computed) — the raw `GearCtx::secondary_get`
    /// followed by the per-variant extraction.
    pub async fn secondary_get<S: Storage<R>>(&self, ctx: &GearCtx<R, S>) -> Out
    where
        Out: Send,
    {
        (self.getter)(ctx.secondary_get(self.id.clone()).await)
    }
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
        _input: GearInput<Self>,
        _cache: &mut Self::GearCache<S::Watermark>,
    ) -> Self::GearOut {
    }
}
