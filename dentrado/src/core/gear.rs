//! The gear-facing layer: how a runtime runs a gear, and the typed query
//! handles built on top of it.
//!
//! The value vocabulary ([`GearMeta`], [`GearInput`], [`GearProduce`],
//! [`GearResult`]) lives in [`crate::core::api`] so storage and wire can name it
//! without depending on the gear machinery. This module owns the behaviour
//! trait ([`IsRuntime`]), the query/subscription lenses, and the empty runtime
//! used by storage-only tests.

use std::{fmt::Debug, ops::Deref, rc::Rc};

pub use crate::core::api::{GearInput, GearMeta, GearProduce, GearResult, RuntimeTypes};
pub use crate::core::core_ctx::IsRuntime;
use crate::{
    core::{
        api::Shared,
        core_ctx::{Core, GearCtx, Subscription},
        storage::IsStorage,
    },
    types::LocMsgTypeId,
};

/// A deferred, composable read against a gear's output — the typed layer over
/// the raw [`GearCtx::secondary_get`] (which returns an untyped [`GearResult`]).
///
/// `id` names the gear (with its id fields); `getter` extracts `Out` out of the
/// stored [`GearResult`]. Both are **private by design**: a `GearQuery` is an
/// opaque handle — callers address gears through the generated builders and
/// read results through [`GearQuery::secondary_get`] / [`GearQuery::subscribe`]
/// (whose [`GearSubscription::current`] yields the extracted `Out`). If a new
/// use needs direct access, add a method here rather than reopening the fields.
/// The `#[gears]` macro pairs them per gear, so by construction `getter` is
/// always fed the family it matches — the `unreachable!` arm in it is a
/// defensive invariant, not an expected path.
/// For a shippable gear `Out` is an owned value (peeled out of the `Ship` arm);
/// for a `#[gear(shared)]` gear `Out` is [`SharedView<R, T>`] (built from the
/// `Shared` arm). Local (`#[gear(local)]`) outputs are deliberately outside
/// this layer: they have no builder and are read only through `follow` gears.
///
/// Built by the `#[gears]` macro as one fn per gear, e.g.
/// `pub fn repo() -> GearQuery<R, Rc<RepoData>>`.
pub struct GearQuery<R: IsRuntime, Out> {
    /// Queried gear.
    id: R::GearId,
    /// Extracts `Out` out of the gear's **stored** result ([`GearResult`]).
    /// For a shippable gear `Out` is owned, peeled from the `Ship` arm; for a
    /// shared gear `Out` is [`SharedView<R, T>`], built from the `Shared` arm.
    /// A `Local` result is never fed here — a `Local` output is pinned to its
    /// core and reachable only through `follow`; the getter's defensive arm
    /// panics if one somehow arrives.
    getter: fn(GearResult<R>) -> Out,
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
    /// The queried gear's id. Prefer the methods below over inspecting it.
    pub fn id(&self) -> &R::GearId {
        &self.id
    }

    /// Extract `Out` out of a raw stored [`GearResult`] — the per-family
    /// projection this query was built to read (e.g. peeling a subscription
    /// push or a `read_gear` result).
    pub fn extract(&self, out: GearResult<R>) -> Out {
        (self.getter)(out)
    }

    /// Declare a dependency on this gear's output and pull its current value
    /// (awaiting it if not yet computed) — the raw [`GearCtx::secondary_get`]
    /// followed by the getter's per-family extraction. A `Local` result never
    /// reaches here: it is a routing bug (a `Local` output should be read
    /// through a `follow` gear on its own core), so the getter's defensive arm
    /// panics.
    pub async fn secondary_get<S: IsStorage<R>>(&self, ctx: &GearCtx<R, S>) -> Out {
        let out = ctx.secondary_get(self.id.clone()).await;
        (self.getter)(out)
    }

    /// Subscribe to this gear's output (worker-facing push mode). The id is
    /// owned internally, so the caller never names the concrete `R::GearId`
    /// type. The returned [`GearSubscription`] is the query's typed lens over
    /// the raw stream: `current`/`next` yield the extracted `Out`, so callers
    /// never match [`GearResult`] variants by hand (the raw form is reachable
    /// through `Deref` if a wire layer needs it).
    pub async fn subscribe<S: IsStorage<R>>(
        &self,
        core: &Rc<Core<R, S>>,
    ) -> GearSubscription<R, S, Out> {
        GearSubscription {
            sub: core.subscribe_gear(self.id.clone()).await,
            getter: self.getter,
        }
    }

    /// The raw-subscription escape hatch: the same subscription without the
    /// getter projection, for wire bridges that re-encode pushes in the
    /// unprojected [`GearResult`] form (everything else should prefer
    /// [`GearQuery::subscribe`]).
    pub async fn subscribe_raw<S: IsStorage<R>>(
        &self,
        core: &Rc<Core<R, S>>,
    ) -> Subscription<R, S> {
        core.subscribe_gear(self.id.clone()).await
    }

    /// The macro-facing constructor: pairs an id with its getter. Not for
    /// hand-written code — build queries through the generated per-gear
    /// builders.
    #[doc(hidden)]
    pub fn new(id: R::GearId, getter: fn(GearResult<R>) -> Out) -> Self {
        Self { id, getter }
    }
}

/// A [`Subscription`] projected through a [`GearQuery`]'s getter — the typed
/// read side of the query layer. [`GearSubscription::current`] and
/// [`GearSubscription::next`] return the extracted `Out` (an owned value for a
/// shippable gear, a [`SharedView`] for a shared one) instead of the raw
/// stored [`GearResult`], so callers never re-implement the per-family
/// projection the macro already paired with the gear. `Deref`s to the raw
/// [`Subscription`] for the rare consumer that needs the unprojected form
/// (e.g. a wire bridge re-encoding pushes).
#[must_use]
pub struct GearSubscription<R: IsRuntime, S: IsStorage<R>, Out> {
    sub: Subscription<R, S>,
    getter: fn(GearResult<R>) -> Out,
}

impl<R: IsRuntime, S: IsStorage<R>, Out> GearSubscription<R, S, Out> {
    /// The gear's currently-cached output, extracted.
    #[must_use]
    pub fn current(&self) -> Out {
        (self.getter)(self.sub.current())
    }

    /// Wait for the next output update and return the new value, extracted.
    pub async fn next(&self) -> Out {
        (self.getter)(self.sub.next().await)
    }
}

impl<R: IsRuntime, S: IsStorage<R>, Out> Deref for GearSubscription<R, S, Out> {
    type Target = Subscription<R, S>;
    fn deref(&self) -> &Subscription<R, S> {
        &self.sub
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
impl RuntimeTypes for EmptyRuntime {
    type GearId = ();
    type GearOut = ();
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = ();
    type Body = ();
    type Data = ();
}

impl IsRuntime for EmptyRuntime {
    fn meta(_gear: &Self::GearId) -> GearMeta<Self> {
        GearMeta::Event {
            msg_type: LocMsgTypeId(0),
            group: (),
        }
    }

    async fn run_step<S: IsStorage<Self>>(
        _ctx: &mut GearCtx<Self, S>,
        _input: GearInput<Self>,
        _view: &mut S::View<'_>,
    ) -> GearProduce<Self> {
        GearProduce::Ship(())
    }
}
