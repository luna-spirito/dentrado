//! The runtime's type vocabulary: everything that a storage backend or the
//! wire layer may name without knowing how gears run.
//!
//! [`RuntimeTypes`] is the associated-type half of the runtime (ids, groups,
//! bodies, data, the three output families). [`IsRuntime`] adds the behaviour
//! half — routing metadata and `run_step` — and lives later, in
//! [`core_ctx`](crate::core::core_ctx), because it depends on the gear
//! context. Splitting the two is what keeps storage and wire free of the gear
//! machinery.

mod shared;

use std::{fmt::Debug, hash::Hash, num::NonZero};

use crate::types::{GlobalHash, LocGroupId, LocMsgTypeId, Localizable};

pub use self::shared::Shared;
pub(crate) use self::shared::{Epoch, RemoteShared, SharedArena, SharedBus, SharedData, SharedKey};

pub trait RuntimeTypes: Debug + Send + Sync + Sized + 'static {
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
    /// a thread or core boundary, so (unlike [`GearOut`](RuntimeTypes::GearOut))
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
}

/// How a gear is driven and where it lives, returned by
/// [`IsRuntime::meta`](crate::core::core_ctx::IsRuntime::meta).
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
pub enum GearMeta<R: RuntimeTypes> {
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

impl<R: RuntimeTypes> GearMeta<R> {
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
pub enum GearInput<R: RuntimeTypes> {
    Events(LocGroupId),
    Timer { tick: bool }, // TODO: Currently GearInput::Timer causes unnecessary cascade updates!
    Follow { out: GearResult<R> },
}

/// The fresh output of one `run_step` — what the gear *produces*. Distinct
/// from [`GearResult`] (the *stored* form) only in the `Shared` arm: a gear
/// produces an owned `R::GearOutShared`, which the core then heap-allocates and
/// refcounts into a [`Shared`] handle. `Ship`/`Local` are their own stored form.
/// Never cloned: produced once per run and immediately installed.
pub enum GearProduce<R: RuntimeTypes> {
    Ship(R::GearOut),
    Shared(R::GearOutShared),
    Local(R::GearOutLocal),
}

impl<R: RuntimeTypes> Debug for GearProduce<R> {
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
/// bound beyond what [`RuntimeTypes`] requires.
pub enum GearResult<R: RuntimeTypes> {
    Ship(R::GearOut),
    Shared(Shared<R>),
    Local(R::GearOutLocal),
}

impl<R: RuntimeTypes> Clone for GearResult<R> {
    fn clone(&self) -> Self {
        match self {
            Self::Ship(o) => Self::Ship(o.clone()),
            Self::Shared(s) => Self::Shared(s.clone()),
            Self::Local(o) => Self::Local(o.clone()),
        }
    }
}

impl<R: RuntimeTypes> Debug for GearResult<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ship(o) => f.debug_tuple("Ship").field(o).finish(),
            Self::Shared(s) => f.debug_tuple("Shared").field(s).finish(),
            Self::Local(o) => f.debug_tuple("Local").field(o).finish(),
        }
    }
}

impl<R: RuntimeTypes> GearResult<R> {
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
