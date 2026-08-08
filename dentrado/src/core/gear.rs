use std::{fmt::Debug, hash::Hash, num::NonZero, ops::Deref, rc::Rc};

use crate::{
    core::{
        core_ctx::{Core, GearCtx, Subscription},
        shared::Shared,
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

/// The fresh output of one `run_step` — what the gear *produces*. Distinct
/// from [`GearResult`] (the *stored* form) only in the `Shared` arm: a gear
/// produces an owned `R::GearOutShared`, which the core then heap-allocates and
/// refcounts into a [`Shared`] handle. `Ship`/`Local` are their own stored form.
/// Never cloned: produced once per run and immediately installed.
pub enum GearProduce<R: IsRuntime> {
    Ship(R::GearOut),
    Shared(R::GearOutShared),
    Local(R::GearOutLocal),
}

impl<R: IsRuntime> Debug for GearProduce<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ship(o) => f.debug_tuple("Ship").field(o).finish(),
            Self::Shared(o) => f.debug_tuple("Shared").field(o).finish(),
            Self::Local(o) => f.debug_tuple("Local").field(o).finish(),
        }
    }
}

/// The **stored** form of a gear output — what lives in `ActiveGear::output`,
/// flows through [`GearInput::Follow`], and is handed out by `secondary_get` /
/// the worker read APIs.
///
/// Three families, distinguished by how the value is shared:
/// - [`Ship`](GearResult::Ship) — cheaply-cloned, `Send + Localizable`. Crosses
///   cores by value (one clone per subscriber core).
/// - [`Shared`](GearResult::Shared) — opaque `Sync` monolith, read by reference.
///   Crosses cores by raw pointer to the *same* allocation ([`RemoteShared`]).
/// - [`Local`](GearResult::Local) — pinned to its owning core, `!Send`.
///
/// `Clone`/`Debug` are manual (not derived) so they add no `R: Clone`/`R: Debug`
/// bound beyond what [`IsRuntime`] requires.
pub enum GearResult<R: IsRuntime> {
    Ship(R::GearOut),
    Shared(Shared<R>),
    Local(R::GearOutLocal),
}

impl<R: IsRuntime> Clone for GearResult<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Ship(o) => Self::Ship(o.clone()),
            Self::Shared(s) => Self::Shared(s.clone()),
            Self::Local(o) => Self::Local(o.clone()),
        }
    }
}

impl<R: IsRuntime> Debug for GearResult<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ship(o) => f.debug_tuple("Ship").field(o).finish(),
            Self::Shared(s) => f.debug_tuple("Shared").field(s).finish(),
            Self::Local(o) => f.debug_tuple("Local").field(o).finish(),
        }
    }
}

impl<R: IsRuntime> GearResult<R> {
    /// The shippable payload, if this is a [`Ship`](GearResult::Ship) result.
    pub fn into_ship(self) -> Option<R::GearOut> {
        match self {
            Self::Ship(o) => Some(o),
            Self::Local(_) | Self::Shared(_) => None,
        }
    }

    /// The shared handle, if this is a [`Shared`](GearResult::Shared) result.
    pub fn into_shared(self) -> Option<Shared<R>> {
        match self {
            Self::Shared(s) => Some(s),
            _ => None,
        }
    }

    /// The shippable payload, panicking otherwise. For call sites that
    /// statically know the gear is shippable.
    pub fn expect_ship(self) -> R::GearOut {
        match self {
            Self::Ship(o) => o,
            _ => panic!("expected a shippable GearResult"),
        }
    }
}

pub trait IsRuntime: Debug + Send + Sync + Sized + 'static {
    type GearId: Debug + Hash + Eq + Clone + Send + 'static + Localizable;

    type GearOut: Debug + Clone + Send + 'static + Localizable;

    /// An opaque, `Sync` output shared **by reference** across consumers and
    /// cores (the `Shared` family). The runtime treats it as a monolith: it
    /// never inspects or clones the value, only heap-allocates it once and
    /// hands out refcounted [`Shared`] handles (raw pointer across cores).
    /// Need not be `Clone` (the core never copies it) nor `Localizable` (it
    /// crosses cores by pointer, not by serialized value). `Sync` is required
    /// so a foreign core may read the shared allocation.
    type GearOutShared: Debug + Sync + 'static; // TODO: CRITICAL: We need a new marker constraint here to describe the situation
    // "this value doesn't need localization"

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
    ) -> GearProduce<Self>;
}

/// A deferred, composable read against a gear's output — the typed layer over
/// the raw [`GearCtx::secondary_get`] (which returns an untyped [`GearResult`]).
///
/// `id` names the gear (with its id fields); `getter` extracts `Out` out of the
/// stored [`GearResult`]. The `#[gears]` macro pairs them per gear, so by
/// construction `getter` is always fed the family it matches — the
/// `unreachable!` arm in it is a defensive invariant, not an expected path.
/// For a shippable gear `Out` is an owned value (peeled out of the `Ship` arm);
/// for a `#[gear(shared)]` gear `Out` is [`SharedView<R, T>`] (built from the
/// `Shared` arm). Local (`#[gear(local)]`) outputs are deliberately outside
/// this layer: they have no builder and are read only through `follow` gears.
///
/// Built by the `#[gears]` macro as one fn per gear, e.g.
/// `pub fn repo(repo_meta: RepoMeta) -> GearQuery<R, Arc<RepoData>>`; methods
/// like [`GearQuery::secondary_get`] are the composable surface.
pub struct GearQuery<R: IsRuntime, Out> {
    /// Queried gear.
    pub id: R::GearId,
    /// Extracts `Out` out of the gear's **stored** result ([`GearResult`]).
    /// For a shippable gear `Out` is owned, peeled from the `Ship` arm; for a
    /// shared gear `Out` is [`SharedView<R, T>`], built from the `Shared` arm.
    /// A `Local` result is never fed here — a `Local` output is pinned to its
    /// core and reachable only through `follow`; the getter's defensive arm
    /// panics if one somehow arrives.
    pub getter: fn(GearResult<R>) -> Out,
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
    /// (awaiting it if not yet computed) — the raw [`GearCtx::secondary_get`]
    /// followed by the getter's per-family extraction. A `Local` result never
    /// reaches here: it is a routing bug (a `Local` output should be read
    /// through a `follow` gear on its own core), so the getter's defensive arm
    /// panics.
    pub async fn secondary_get<S: Storage<R>>(&self, ctx: &GearCtx<R, S>) -> Out {
        let out = ctx.secondary_get(self.id.clone()).await;
        (self.getter)(out)
    }

    /// Subscribe to this gear's output (worker-facing push mode). The id is
    /// owned internally, so the caller never names the concrete `R::GearId`
    /// type. The returned [`Subscription`] yields raw [`GearResult`]s; the
    /// per-variant extraction is up to the caller (e.g. via [`GearOut`]).
    pub async fn subscribe<S: Storage<R>>(&self, core: &Rc<Core<R, S>>) -> Subscription<R, S> {
        core.subscribe_gear(self.id.clone()).await
    }
}

/// The borrow-returning `Out` of a shared [`GearQuery`]: an owned refcounted
/// handle ([`Shared<R>`]) that [`Deref`]s to a projected `&Out` of the shared
/// value. Built by the getter of `GearQuery<R, SharedView<R, T>>`.
///
/// [`GearQuery::secondary_get`] cannot return a borrow tied to the core's
/// internals, so it returns this owned handle instead — the borrow stays valid
/// for as long as the caller holds the `SharedView`, and dropping it releases
/// the refcount (a cheap local decrement). `Clone` is likewise a local
/// refcount bump (never an inter-core message); `Deref` projects inside
/// `R::GearOutShared` via the stored fn.
pub struct SharedView<R: IsRuntime, Out> {
    /// The refcounted handle backing the borrow.
    pub inner: Shared<R>,
    /// Per-variant projection `&GearOutShared → &Out`; borrows from the shared
    /// allocation, valid for any lifetime the caller requests.
    pub project: for<'a> fn(&'a R::GearOutShared) -> &'a Out,
}

impl<R: IsRuntime, Out> Clone for SharedView<R, Out> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            project: self.project,
        }
    }
}

impl<R: IsRuntime, Out> Deref for SharedView<R, Out> {
    type Target = Out;
    fn deref(&self) -> &Out {
        (self.project)(&*self.inner)
    }
}

impl<R: IsRuntime, Out: Debug> Debug for SharedView<R, Out> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

#[derive(Debug)]
pub(crate) struct EmptyRuntime;
impl IsRuntime for EmptyRuntime {
    type GearId = ();
    type GearOut = ();
    type GearOutShared = ();
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
    ) -> GearProduce<Self> {
        GearProduce::Ship(())
    }
}
