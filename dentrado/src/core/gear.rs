use std::{fmt::Debug, hash::Hash, num::NonZero};

use crate::{
    core::{
        core_ctx::{Core, GearCtx, Subscription},
        storage::Storage,
    },
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
#[derive(Debug, Clone)]
pub enum GearInput<R: IsRuntime> {
    Events(LocGroupId),
    Timer { tick: bool },
    Follow { out: GearResult<R> },
}

/// The result of running a gear, tagging whether the output is **shippable**
/// across cores ([`Ship`](GearResult::Ship) — `R::GearOut`, which is
/// `Send + Localizable`) or **pinned to its owning core**
/// ([`Local`](GearResult::Local) — `R::GearOutLocal`, which is *not* required
/// to be `Send` or [`Localizable`]).
///
/// This is what [`IsRuntime::run_step`] returns and what flows through every
/// on-core path (`ActiveGear::output`, `secondary_get`, [`GearInput::Follow`],
/// the worker read APIs). The cross-core/wire boundary carries only `R::GearOut`
/// — extracted via [`GearResult::into_ship`] — so a `Local` output physically
/// cannot enter a `Send`-typed channel (`InterCoreMsg`, the `RunGear` reply).
///
/// `Clone`/`Debug` are implemented manually (not derived) so they don't add a
/// spurious `R: Clone`/`R: Debug` bound beyond what [`IsRuntime`] already
/// requires; only the associated output types need it.
pub enum GearResult<R: IsRuntime> {
    /// A shippable output — crosses cores, is `Send + Localizable`.
    Ship(R::GearOut),
    /// A core-local output — never serialized, never sent across a thread.
    Local(R::GearOutLocal),
}

impl<R: IsRuntime> Clone for GearResult<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Ship(o) => Self::Ship(o.clone()),
            Self::Local(o) => Self::Local(o.clone()),
        }
    }
}

impl<R: IsRuntime> Debug for GearResult<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ship(o) => f.debug_tuple("Ship").field(o).finish(),
            Self::Local(o) => f.debug_tuple("Local").field(o).finish(),
        }
    }
}

impl<R: IsRuntime> GearResult<R> {
    /// The shippable payload, if this is a [`Ship`](GearResult::Ship) result.
    /// Used at the cross-core boundary to extract the value that actually goes
    /// on the wire / through the `RunGear` reply channel.
    pub fn into_ship(self) -> Option<R::GearOut> {
        match self {
            Self::Ship(o) => Some(o),
            Self::Local(_) => None,
        }
    }

    /// The shippable payload, panicking if this is a `Local` result. For tests
    /// and call sites that statically know the gear is shippable.
    ///
    /// # Panics
    ///
    /// If this is a [`Local`](GearResult::Local) result.
    pub fn expect_ship(self) -> R::GearOut {
        match self {
            Self::Ship(o) => o,
            Self::Local(_) => panic!("expected a shippable GearResult, got Local"),
        }
    }
}

pub trait IsRuntime: Debug + Send + Sync + Sized + 'static {
    type GearId: Debug + Hash + Eq + Clone + Send + 'static + Localizable;

    type GearOut: Debug + Clone + Send + 'static + Localizable;

    /// Output of a gear that is **pinned to its owning core**: it never crosses
    /// a thread or core boundary, so (unlike [`GearOut`](IsRuntime::GearOut))
    /// it need not be `Send` or [`Localizable`]. `run_step` returns a
    /// [`GearResult`] that tags which family a given output belongs to. The
    /// runtime guarantees a `Local` output is never routed off its core (a
    /// remote subscription / cross-core `RunGear` against such a gear is a
    /// routing error, surfaced at the wire boundary).
    type GearOutLocal: Debug + Clone + 'static;

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
    ) -> GearResult<Self>;
}

/// A deferred, composable read against a gear's output — the typed layer over
/// the raw `GearCtx::secondary_get` (which returns an untyped `GearOut`).
///
/// `id` names the gear (with its id fields); `getter` extracts `Out` out of the
/// matching shippable `GearOut` variant. The `#[gears]` macro pairs them per
/// variant, so by construction `getter` is always fed the variant it matches —
/// the `unreachable!` arm in it is a defensive invariant, not an expected path.
/// Local (`#[gear(local)]`) outputs are deliberately outside this layer: they
/// have no builder and are read only through `follow` gears.
///
/// Built by the `#[gears]` macro as one fn per gear, e.g.
/// `pub fn repo(repo_meta: RepoMeta) -> GearQuery<R, Arc<RepoData>>`; methods
/// like [`GearQuery::secondary_get`] are the composable surface.
pub struct GearQuery<R: IsRuntime, Out> {
    /// Queried gear.
    pub id: R::GearId,
    /// Getter that extracts `Out` out of the gear's **shippable** output.
    /// `GearQuery` is the typed `secondary_get` layer, and that layer only
    /// ever sees `R::GearOut` — a `Local` output is pinned to its core and is
    /// reachable only through the `follow` mechanism. Must be used with the
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
    /// followed by the per-variant extraction. Only shippable outputs can be
    /// reached this way: a `Local` result is a routing bug (it should have been
    /// read through a `follow` gear on its own core).
    pub async fn secondary_get<S: Storage<R>>(&self, ctx: &GearCtx<R, S>) -> Out
    where
        Out: Send,
    {
        let out = ctx.secondary_get(self.id.clone()).await;
        (self.getter)(
            out.into_ship()
                .expect("secondary_get: local gear output reached through the typed query layer"),
        )
    }

    /// Subscribe to this gear's output (worker-facing push mode). The id is
    /// owned internally, so the caller never names the concrete `R::GearId`
    /// type. The returned [`Subscription`] yields raw [`GearResult`]s; the
    /// per-variant extraction is up to the caller (e.g. via [`GearOut`]).
    pub async fn subscribe<S: Storage<R>>(
        &self,
        core: &std::rc::Rc<Core<R, S>>,
    ) -> Subscription<R, S> {
        core.subscribe_gear(self.id.clone()).await
    }
}

#[derive(Debug)]
pub(crate) struct EmptyRuntime;
impl IsRuntime for EmptyRuntime {
    type GearId = ();
    type GearOut = ();
    type GearOutLocal = ();
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
    ) -> GearResult<Self> {
        GearResult::Ship(())
    }
}
