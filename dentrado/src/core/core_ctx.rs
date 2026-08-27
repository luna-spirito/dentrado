use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    future::poll_fn,
    num::NonZero,
    ops::Deref,
    panic::AssertUnwindSafe,
    rc::{Rc, Weak},
    sync::{Arc, mpsc},
    task::Poll,
    time::Duration,
};

use compio::runtime::JoinHandle;
use futures::FutureExt;
use slotmap::{SlotMap, new_key_type};

pub use crate::core::subscription::Subscription;
use crate::{
    core::{
        db,
        doorbell::DoorbellHandle,
        gear::{GearInput, GearMeta, GearProduce, GearResult, IsRuntime},
        shared::{RemoteShared, Shared, SharedArena, SharedBus, SharedData, SharedKey},
        stats::CoreStats,
        storage::{GroupStore, Storage},
        subscription::Epoch,
    },
    types::{GlobalCoreId, GlobalHash, LocGroupId, NodeId},
    wire::{
        MergeError, RunGearError, WireEventBody, WireLocCtx, WireLocCtxBuilder, WireLocCtxMerger,
    },
};

// TODO: Crack down on clones?
// TODO: Secondary input caching? Quite desirable to keep processing strictly local.
// TODO: NO GODDAMN ARCs!

// Maybe remove CoordCmd? I don't feel like it, it's more efficient this way.
/// Represents an operation initiate by a direct client of the DBMS.
/// Shared between `CoordCmd` (arrives via `cmd_rx`) and `InterCoreMsg`
/// (arrives via SPSC inter-core channels) to avoid duplicating handler logic.
#[derive(Debug)]
pub(crate) enum CoreCmd<R: IsRuntime> {
    PostEvents {
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: Vec<u32>,
        forwarded_from: Option<NodeId>,
        reply: Option<flume::Sender<Result<(), MergeError>>>,
    },
    RunGear {
        gear: R::GearId,
        wire_ctx: WireLocCtx<R>,
        reply: flume::Sender<Result<R::GearOut, RunGearError>>,
    },
}

/// Command sent from the DBMS's coordinator, i. e. from the caller of `Db::start`.
pub(crate) enum CoordCmd<R: IsRuntime> {
    Op(CoreCmd<R>),
    Shutdown,
}

/// Inter-node singnals.
#[derive(Debug)]
pub(crate) enum InterNodeMsg<R: IsRuntime> {
    ForwardEvents {
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    },
}

#[derive(Debug)]
pub(crate) enum RerouteMsg<R: IsRuntime> {
    ForwardToPeer {
        peer_idx: usize,
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    },
}

/// Cross-core subscription protocol (push-based, replacing the old
/// request/response `SecondaryRequest`/`SecondaryResponse` pair).
///
/// - `StartSubscription`: "I (`from_core`, speaking as arena key `session`)
///   want push updates for `gear`."
/// - `SubscriptionUpdate`: "Here is the current/new output for the gear you
///   subscribed to." Sent on subscribe and on every recompute while subscribed.
/// - `StopSubscription`: "I (`from_core`) no longer care; the subscription was
///   `session`." Carries only the opaque session id — no gear, no `wire_ctx` —
///   so the `Drop`-driven eviction path (`evict_gear` → `send_stop`) never has
///   to read localization tables (which will become async).
#[derive(Debug)]
pub(crate) enum InterCoreMsg<R: IsRuntime> {
    Op(CoreCmd<R>),
    StartSubscription {
        gear: R::GearId,
        wire_ctx: Arc<WireLocCtx<R>>,
        from_core: u32,
        /// Opaque session token = the subscriber's own arena [`GearKey`] for
        /// this remote gear. Echoed back in [`InterCoreMsg::StopSubscription`]
        /// so the receiver can route the stop without localizing `gear`. Per-
        /// arena + generational ⇒ unique per `(from_core, session)`.
        session: GearKey,
    },
    SubscriptionUpdate {
        gear: R::GearId,
        output: R::GearOut,
        wire_ctx: Arc<WireLocCtx<R>>,
    },
    /// Cross-core push of a **shared** gear's output: a raw pointer to the
    /// owner core's immutable payload (`SharedData`), already cross-core
    /// retained there (the owner bumped the arena `xcount` for this push), plus
    /// the opaque [`SharedKey`] the receiver echoes back on unref. The receiver
    /// knows the sender — and thus the allocation's owner — from the channel the
    /// message arrived on, so no `from_core` is carried. The receiver wraps
    /// `data`/`key` in a foreign [`Shared`] handle whose `owner` is that sender.
    SubscriptionUpdateShared {
        gear: R::GearId,
        data: RemoteShared<SharedData<R>>,
        key: SharedKey,
        wire_ctx: Arc<WireLocCtx<R>>,
    },
    /// A foreign core dropped the **last** of its local handles to one of this
    /// core's allocations: release that core's cross-core claim (`xcount -= 1`,
    /// freeing on zero). No retain counterpart exists: a foreign core can only
    /// drop a claim it already holds.
    SharedUnref {
        key: SharedKey,
    },
    StopSubscription {
        /// The session id from the matching [`InterCoreMsg::StartSubscription`].
        session: GearKey,
        from_core: u32,
    },
    /// A core of this node died (thread panic/exit). Sent by the dying core
    /// to every core — itself included — via [`broadcast_core_death`];
    /// recipients stop serving so the whole `Db` fails together instead of
    /// freezing on gears the dead core owned.
    CoreDied,
}

new_key_type! {
    /// Opaque, generational handle into [`CoreLocCtx::gears`]. Cheap to copy and
    /// store in edge sets (no `R::GearId` cloning). The generation tag makes it safe to reuse.
    /// Do we really need one, though? Maybe not necessarily.
    pub(crate) struct GearKey;
}

#[derive(Debug)]
struct CoreLocCtx<R: IsRuntime> {
    // --- subscription state ---
    /// Every gear this core knows about (active), stored in a generational
    /// arena. A slot present ⟺ the gear has been touched by a
    /// `subscribe/read/secondary_get`; it stays here while it has any interest
    /// OR while it sits in `unref_gear` limbo (hot — still rerun on input
    /// events). Removal happens only when popped from `unref_gear`.
    gears: SlotMap<GearKey, ActiveGear<R>>,
    /// Boundary index: `R::GearId` (the public, wire-facing identity) → the
    /// arena [`GearKey`] backing the live gear. 1:1 with `gears`. All internal
    /// edge sets and limbo hold `GearKey`; the `R::GearId` survives only at the
    /// API/wire boundary and inside `ActiveGear::id` (for reverse lookup).
    gear_index: HashMap<R::GearId, GearKey>,
    /// Limbo: gears with no current subscribers, kept hot until popped (FIFO).
    /// Holds `GearKey`s into `gears`; the `ActiveGear` data lives there.
    unref_gear: VecDeque<GearKey>,
    /// Reverse index: which gears care about a given event input.
    event_subscriptions: HashMap<LocGroupId, HashSet<GearKey>>,
    /// Timer-driven (oracle) gears on this core, keyed by arena [`GearKey`].
    /// Scanned by the epoch ticker each tick to find gears whose `next_due`
    /// has been reached. Mirrors `event_subscriptions` for the timer path.
    timer_gears: HashSet<GearKey>,
    /// Incoming cross-core subscriptions: `(from_core, session)` → this core's
    /// arena [`GearKey`] backing the subscribed gear. Populated on
    /// [`InterCoreMsg::StartSubscription`], drained on
    /// [`InterCoreMsg::StopSubscription`] (which carries only the session).
    /// Kept consistent with each gear's `remote_subscribers` (added/removed in
    /// lockstep), so a local gear is only evicted when it has no incoming sub.
    incoming_subs: HashMap<(u32, GearKey), GearKey>,
    /// Monotonic epoch counter, advanced by [`Core::epoch_tick`] every
    /// [`EPOCH_INTERVAL`]. [`GearSource::Timer`] gears compare their
    /// `next_due` against this to decide `tick`.
    epoch: u64,
}

/// Soft cap on the number of gears kept hot in limbo. Beyond this, the
/// oldest demoted gear is fully torn down (cascade-evicting dependencies).
const LIMBO_CAPACITY: usize = 64;

/// Wall-clock interval between two epoch-counter advances. A `Timer` gear with
/// `period = N` is rerun at most once per `N` epochs, i.e. at most once per
/// `N * EPOCH_INTERVAL` of real time while it has interest.
const EPOCH_INTERVAL: Duration = Duration::from_secs(1);

/// Lifecycle of a gear's background computation task.
///
/// `Running`/`RunningQueued` own the spawned task's [`JoinHandle`]. Dropping
/// the handle cancels the task (compio semantics), so when a gear is
/// evicted (`gears.remove` → `ActiveGear` dropped → status dropped → handle
/// dropped) its in-flight `run_step` is aborted immediately — no need for the
/// task to discover the eviction after `run_step` completes. The task itself
/// never drops its own handle: on the `Running → Eepy` transition it `detach`es
/// the handle so it can finish its post-run fan-out and return naturally.
#[derive(Debug, Default)]
pub(crate) enum ActiveGearStatus {
    /// No task attached; inputs haven't changed since the last run.
    #[default]
    Eepy,
    /// A task is currently executing `run_step` for this gear.
    Running { handle: JoinHandle<()>, rerun: bool },
}

/// What drives a local gear's re-runs, stored in [`ActiveGearExecution::Local`].
/// The runtime-facing mirror of [`GearMeta`] plus the bookkeeping (the timer's
/// `next_due`) the core needs to decide when to fire it.
#[derive(Debug)]
pub(crate) enum GearSource {
    /// Event-driven: rerun when new events land in this group.
    Events(LocGroupId),
    /// Timer-driven (oracle): rerun when the core's epoch counter reaches
    /// `next_due`. `period` is the minimum gap between two `tick = true` runs.
    Timer { period: NonZero<u64>, next_due: u64 },
    /// Follow-driven: rerun when the followed gear's output changes. Carries
    /// the target's arena [`GearKey`]. This is a *static* edge declared in
    /// [`GearMeta::Follow`], distinct from the dynamic `secondary_get` edges
    /// tracked in `dep_set`: its sole bookkeeping is the reverse entry in the
    /// target's `local_dependents` (set at activation, torn down in
    /// [`CoreLocCtx::evict_gear`]), so the target fans out a kick to this gear
    /// on every new output. The runtime sees that output via
    /// [`GearInput::Follow`].
    Follow { target: GearKey },
}

#[derive(Debug)]
pub(crate) enum ActiveGearExecution {
    Local {
        source: GearSource,
        status: ActiveGearStatus,
        /// Gears this one depends on (forward gear-dep index). Reconciled each
        /// run from `GearCtx::deps`.
        dep_set: HashSet<GearKey>,
    },
    Remote {
        target_core: u32,
    },
}

/// Per-gear state. Lives in `gears`. Orthogonal axes:
/// - `output`        — has a value ever been computed?
/// - `execution`     — local (with run status) or remote (subscribed elsewhere)
/// - limbo membership (hot/cold) — are there any subscribers? (`unref_gear`)
#[derive(Debug)]
pub(crate) struct ActiveGear<R: IsRuntime> {
    /// The gear's public `R::GearId`. Kept here so the arena can recover the
    /// wire-facing identity for wire remap / `gear_index` reverse lookup.
    pub(crate) id: R::GearId,
    pub(crate) output: Option<GearResult<R>>,
    pub(crate) execution: ActiveGearExecution,
    /// Gears (on this core) that depend on this one (forward gear-dep index).
    /// For a remote gear, these are local gears that read it via `secondary_get`.
    pub(crate) local_dependents: HashSet<GearKey>,
    /// Remote cores subscribed to this gear's output (for local gears only). Stored to know whom to notify.
    pub(crate) remote_subscribers: HashSet<u32>,
    /// Count of direct (worker-side) subscribers. Eager demote trigger.
    pub(crate) direct_subscriber_count: usize,
    /// Change signal for output updates to direct subscribers / `secondary_get`
    /// awaiters. Bumped on every completed run (local) or `SubscriptionUpdate`
    /// (remote); waiters park on its epoch via [`Core::wait_change`].
    pub(crate) changed: Epoch,
}

impl<R: IsRuntime> ActiveGear<R> {
    /// Whether anything still cares about this gear's output.
    fn has_interest(&self) -> bool {
        !self.local_dependents.is_empty()
            || !self.remote_subscribers.is_empty()
            || self.direct_subscriber_count > 0
    }
}

/// Tell every core of this node — the dying one included — that a core just
/// died: send [`InterCoreMsg::CoreDied`] down every channel, then ring each
/// doorbell so parked event loops wake and read it. Recipients mark themselves
/// dead and let their threads end, so the whole `Db` fails together
/// (`Db::park`) instead of freezing on gears a dead core will never update.
/// Used by [`Core::link_task`] (a panicked task) and the thread-level death
/// guard in `db.rs` (a panicked/failed core thread).
pub(crate) fn broadcast_core_death<R: IsRuntime>(
    txs: &[mpsc::Sender<InterCoreMsg<R>>],
    doorbells: &[DoorbellHandle],
) {
    for tx in txs {
        let _ = tx.send(InterCoreMsg::CoreDied);
    }
    for doorbell in doorbells {
        doorbell.ring();
    }
}

#[derive(Debug)]
pub struct Core<R: IsRuntime, S: Storage<R>> {
    num_cores: NonZero<u32>,
    core_id: u32,
    node_id: NodeId,
    module: Arc<R::Module>,

    intercore_tx: Vec<mpsc::Sender<InterCoreMsg<R>>>,
    reroute_tx: Vec<mpsc::Sender<RerouteMsg<R>>>,
    /// One doorbell handle per core (including self). Ring after sending.
    doorbells: Vec<DoorbellHandle>,
    inter_node_peers: Vec<(
        NodeId,
        NonZero<u32>,
        Option<(mpsc::Sender<InterNodeMsg<R>>, DoorbellHandle)>,
    )>,

    /// Typed per-core storage backend. Lives **outside** the `inner` `RefCell`
    /// (it is interior-mutable, mirroring `fs::Fs`), so an `.await` on a storage
    /// op never holds `inner` and never collides with arena mutations. Carries
    /// localization, the event log, and the gear cache (keyed by `R::GearId`).
    storage: S,
    inner: RefCell<CoreLocCtx<R>>,
    /// Owner-local generational arena of cross-core refcounts for [`Shared`]
    /// outputs (`xcount`), kept separate from the immutable payload so refcount
    /// churn never bounces the payload's cache line. Mutated only on this core's
    /// thread.
    shared_arena: RefCell<SharedArena<R>>,
    /// Host-readable introspection gauges ([`CoreStats`]): the engine writes
    /// (only this core's instance, relaxed atomics), the host app reads and
    /// aggregates across cores.
    stats: Arc<CoreStats>,
}

pub(crate) enum HandleMsgResult {
    Ok,
    Die,
}

impl<R: IsRuntime, S: Storage<R>> Core<R, S> {
    pub(crate) fn new(
        num_cores: NonZero<u32>,
        core_id: u32,
        node_id: NodeId,
        module: Arc<R::Module>,
        intercore_tx: Vec<mpsc::Sender<InterCoreMsg<R>>>,
        reroute_tx: Vec<mpsc::Sender<RerouteMsg<R>>>,
        doorbells: Vec<DoorbellHandle>,
        inter_node_peers: Vec<(
            NodeId,
            NonZero<u32>,
            Option<(mpsc::Sender<InterNodeMsg<R>>, DoorbellHandle)>,
        )>,
        storage: S,
    ) -> Self {
        Core {
            num_cores,
            core_id,
            node_id,
            module,
            intercore_tx,
            reroute_tx,
            doorbells,
            inter_node_peers,
            storage,
            shared_arena: RefCell::new(SharedArena::new()),
            stats: Arc::new(CoreStats::default()),
            inner: RefCell::new(CoreLocCtx {
                gears: SlotMap::with_key(),
                gear_index: HashMap::new(),
                unref_gear: VecDeque::new(),
                event_subscriptions: HashMap::new(),
                timer_gears: HashSet::new(),
                incoming_subs: HashMap::new(),
                epoch: 0,
            }),
        }
    }

    /// The per-core storage backend.
    pub(crate) fn storage(&self) -> &S {
        &self.storage
    }

    /// Link a fire-and-forget task's panic to the node's death. compio
    /// catches a panicking task's unwind into its `JoinHandle`, and a detached
    /// handle's result is never taken — the panic would vanish while every
    /// waiter on the task's work freezes. Awaited tasks need no link:
    /// `supervised` in `db.rs` resumes the panic on the core thread, where the
    /// death guard broadcasts. The default panic hook has already reported
    /// the cause by the time we land here.
    pub(crate) async fn link_task<F: std::future::Future>(self: Rc<Self>, fut: F) {
        if AssertUnwindSafe(fut).catch_unwind().await.is_err() {
            log::error!(
                target: "dentrado::core",
                "core {}: task panicked — stopping the node's cores",
                self.core_id
            );
            broadcast_core_death(&self.intercore_tx, &self.doorbells);
        }
    }

    #[must_use]
    pub(crate) fn module(&self) -> &R::Module {
        &self.module
    }

    #[must_use]
    pub fn core_id(&self) -> u32 {
        self.core_id
    }

    /// Does this core own `gear`? Placement is deterministic — the gear's group
    /// hash through the jump-consistent hash, the same computation on every
    /// core — so workers can shard work by ownership without a round trip.
    #[must_use]
    pub fn owns(&self, gear: &R::GearId) -> bool {
        R::meta(gear)
            .group()
            .global_hash(&self.storage)
            .is_ok_and(|h| GlobalCoreId::from_hash(h).route(self.num_cores) == self.core_id)
    }

    #[must_use]
    pub(crate) fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub(crate) fn num_cores(&self) -> NonZero<u32> {
        self.num_cores
    }

    /// This core's introspection gauges, shared to the host app for
    /// cross-core aggregation (the engine writes, the host reads).
    #[must_use]
    pub fn stats(&self) -> &Arc<CoreStats> {
        &self.stats
    }

    /// A group-bound async read view over this core's storage, for the one
    /// group a gear runs for. Cheap to construct (binds the group); gears hand
    /// it to `sg_ord_map`/`state_graph`/`text`.
    #[must_use]
    pub fn group_store(&self, group: LocGroupId) -> GroupStore<'_, R, S> {
        GroupStore::new(&self.storage, group)
    }

    /// Kick off a background run of a local gear if `Eepy`; mark `RunningQueued`
    /// if already running. Panics if the gear is remote/missing.
    /// Kick off a background run of a local gear if `Eepy`; mark `RunningQueued`
    /// if already running. Panics if the gear is remote/missing. Single-shot
    /// convenience wrapper that borrows `inner` — hot loops should call
    /// [`kick_loc_gear_in`] directly under a split field borrow to avoid
    /// re-borrowing `inner` (and a `Vec`) per iteration.
    fn kick_loc_gear(self: &Rc<Self>, key: GearKey) {
        let mut inner = self.inner.borrow_mut();
        Self::kick_loc_gear_in(&mut inner.gears, self, key);
    }

    /// Same as [`kick_loc_gear`](Self::kick_loc_gear) but takes the gears arena
    /// by `&mut` so the caller can hold a split borrow of `inner` (e.g. iterate
    /// `event_subscriptions` / `timer_gears` shared while mutating `gears`) and
    /// kick in-loop without collecting into a `Vec` first.
    fn kick_loc_gear_in(
        gears: &mut SlotMap<GearKey, ActiveGear<R>>,
        self_rc: &Rc<Self>,
        key: GearKey,
    ) {
        let Some(ag) = gears.get_mut(key) else {
            panic!("Couldn't kick non-active gear");
        };
        let ActiveGearExecution::Local { status, .. } = &mut ag.execution else {
            panic!("Kicked non-local gear");
        };
        match status {
            ActiveGearStatus::Eepy => {
                log::trace!(
                    target: "dentrado::gear",
                    "kick core{} {:?} Eepy→Running",
                    self_rc.core_id, ag.id
                );
                let handle = compio::runtime::spawn(
                    self_rc
                        .clone()
                        .link_task(Self::run_loc_gear_task(self_rc.clone(), key)),
                );
                *status = ActiveGearStatus::Running {
                    handle,
                    rerun: false,
                }
            }
            ActiveGearStatus::Running { rerun, .. } => {
                log::trace!(
                    target: "dentrado::gear",
                    "kick core{} {:?} Running→rerun",
                    self_rc.core_id, ag.id
                );
                *rerun = true;
            }
        }
    }

    /// Advance the epoch counter by one and re-kick every timer (oracle) gear
    /// whose `next_due` has been reached *and* that still has interest. Gears
    /// without interest (in limbo) are skipped — oracles only run while active.
    /// A gear that is `Running` gets `rerun` flagged; `run_loc_gear_task`
    /// recomputes `tick` (and re-advances `next_due`) when the rerun starts, so
    /// a tick that lands mid-run is consumed at the next iteration rather than
    /// lost. Public so tests can drive the epoch deterministically instead of
    /// waiting on [`epoch_ticker_task`]'s real-time [`EPOCH_INTERVAL`].
    pub(crate) fn epoch_tick(self: &Rc<Self>) {
        let mut inner = self.inner.borrow_mut();
        // Split-borrow: iterate `timer_gears` (shared) while mutating `gears`
        // (via `kick_loc_gear_in`) — no `Vec` of due keys, one `inner` borrow for
        // the whole tick. `timer_gears` and `gears` are disjoint fields, so the
        // shared iterator borrow and the mutable kick borrow coexist.
        let CoreLocCtx {
            epoch,
            timer_gears,
            gears,
            ..
        } = &mut *inner;
        *epoch = epoch.saturating_add(1);
        let epoch_val = *epoch;
        for k in timer_gears.iter().copied() {
            let due = {
                let Some(ag) = gears.get(k) else {
                    continue;
                };
                if !ag.has_interest() {
                    continue;
                }
                matches!(
                    &ag.execution,
                    ActiveGearExecution::Local {
                        source: GearSource::Timer { next_due, .. },
                        ..
                    } if *next_due <= epoch_val
                )
            };
            if due {
                Self::kick_loc_gear_in(gears, self, k);
            }
        }
    }

    /// Background task that advances the epoch every [`EPOCH_INTERVAL`] and
    /// fires due timer gears. Spawned once per core alongside the worker task;
    /// lives for the core's lifetime (dropping the core's runtime drops this
    /// task). `compio::time::sleep` is `!Send`, but compio spawns this task
    /// locally on the core's own thread, so it never crosses runtimes.
    pub(crate) async fn epoch_ticker_task(self: Rc<Self>) {
        loop {
            compio::time::sleep(EPOCH_INTERVAL).await;
            self.epoch_tick();
        }
    }

    /// The background computation task. Runs `run_step`, then either re-runs
    /// (if `RunningQueued`) or goes back to `Eepy`. Notifies the `changed`
    /// fan-out on each completed run, kicks local dependents, and pushes the
    /// new output to remote subscribers.
    ///
    /// Takes the arena [`GearKey`] (not `R::GearId`): the generation tag *is*
    /// the staleness check. If the gear is evicted (or evicted-and-recreated
    /// under a new generation) mid-flight, `gears.get(key)` returns `None` and
    /// the task abandons — no `gear_index` re-resolution needed. Eviction also
    /// drops the `JoinHandle` stored in `ActiveGearStatus`, which cancels this
    /// task at its next await, so an evicted gear's in-flight `run_step` is
    /// aborted rather than running to completion.
    ///
    /// Pre: `status` is `Running` (set by `kick_loc_gear` or by re-loop).
    async fn run_loc_gear_task(self: Rc<Self>, key: GearKey) {
        // Hold `gear_running` for the task's whole life: the guard drops on
        // every exit path *and* on cancellation (a dropped future drops its
        // state), so the gauge cannot leak.
        let _running = self.stats.running_guard();
        loop {
            // Pull the gear id + cache + run trigger via the arena key. A stale
            // key (gear evicted, or evicted-and-recreated under a new
            // generation) yields `None` → abandon. The `R::GearId` is read from
            // the `ActiveGear` for wire remap / cache construction; the key
            // remains the authoritative handle for the rest of the iteration.
            //
            // For a `Timer` gear the `tick` flag is computed here and
            // `next_due` is advanced *before* `run_step` runs: "consuming" a
            // due tick at the moment the run starts is what enforces the
            // `period` rate limit even if the run is cancelled or re-looped.
            let (gear_id, input) = {
                // TODO: Move outside of loop?
                let mut inner = self.inner.borrow_mut();
                let epoch = inner.epoch;
                let Some(ag) = inner.gears.get_mut(key) else {
                    return;
                };
                let gear_id = ag.id.clone();
                let input = match &mut ag.execution {
                    ActiveGearExecution::Local { source, .. } => match source {
                        GearSource::Events(group) => GearInput::Events(*group),
                        GearSource::Timer { period, next_due } => {
                            let do_tick = ag.output.is_some();
                            if do_tick {
                                *next_due = epoch + period.get();
                                GearInput::Timer { tick: true }
                            } else {
                                GearInput::Timer { tick: false }
                            }
                        }
                        GearSource::Follow { target } => {
                            let target = *target;
                            drop(inner);
                            let out = self
                                .wait_for_output_unpinned(target)
                                .await
                                .expect("run_loc_gear_task: followed gear evicted mid-run");
                            GearInput::Follow { out }
                        }
                    },
                    ActiveGearExecution::Remote { .. } => {
                        unreachable!("run_loc_gear_task on a remote gear")
                    }
                };
                (gear_id, input)
            };

            log::trace!(
                target: "dentrado::gear",
                "run_iter core{} {:?}",
                self.core_id, gear_id
            );

            // Load this gear's working state from storage (cold start ⇒ fresh).
            let mut cache = self
                .storage
                .get_cache(&gear_id)
                .await
                .unwrap_or_else(|| R::make_cache(&gear_id));

            let mut ctx = GearCtx {
                core: Rc::clone(&self),
                gear: gear_id.clone(),
                deps: RefCell::new(HashSet::new()),
            };
            let produce = R::run_step(&mut ctx, input, &mut cache).await;

            // Persist the cache, then install the fresh output (a `Shared`
            // produce is boxed + refcounted here; `Ship`/`Local` pass through)
            // and reconcile dep edges.
            self.storage.put_cache(gear_id.clone(), cache).await;
            let output = self.install_produce(produce);

            let (removed_deps, dependents, remote_subs, do_rerun) = {
                let mut inner = self.inner.borrow_mut();
                let updated_deps = ctx.deps.into_inner();
                // Translate the runtime-facing `R::GearId` deps into arena keys.
                // A dep currently evicted has no key here and is simply dropped
                // from `dep_set` for this run; it re-registers itself on the
                // next `secondary_get`. (Deps are normally active because
                // `secondary_get` force-activates them.) Computed before the
                // mutable `ag` borrow so it only touches `gear_index`.
                let added_deps: HashSet<GearKey> = {
                    let gi = &inner.gear_index;
                    updated_deps
                        .iter()
                        .map(|d| gi.get(d).copied().expect("Missing dependency"))
                        .collect()
                };
                let Some(ag) = inner.gears.get_mut(key) else {
                    unreachable!("Expected run_loc_gear_task to be cancelled");
                };
                ag.output = Some(output.clone());
                let ActiveGearExecution::Local {
                    dep_set, status, ..
                } = &mut ag.execution
                else {
                    unreachable!("run_loc_gear_task on a non-local gear");
                };
                let removed_deps: Vec<GearKey> = dep_set.difference(&added_deps).copied().collect();
                *dep_set = added_deps;
                let ActiveGearStatus::Running { rerun, .. } = status else {
                    unreachable!("run_loc_gear_task on a non-local gear")
                };
                let do_rerun = if *rerun {
                    *rerun = false;
                    true
                } else {
                    *status = ActiveGearStatus::Eepy;
                    false
                };
                let dependents: Vec<GearKey> = ag.local_dependents.iter().copied().collect();
                let remote_subs: Vec<u32> = ag.remote_subscribers.iter().copied().collect();
                ag.changed.bump();
                (removed_deps, dependents, remote_subs, do_rerun)
            };

            // Tear down stale gear→gear edges (no inner borrow held across await).
            {
                let mut inner = self.inner.borrow_mut();
                for dep in &removed_deps {
                    if let Some(dag) = inner.gears.get_mut(*dep) {
                        dag.local_dependents.remove(&key);
                    }
                }
            }
            for dep in &removed_deps {
                self.rebalance_key(*dep);
            }

            // Push the new output to every remote subscriber. A `Local` output
            // never ships (it is pinned to this core), so by construction it has
            // no remote subscribers — only the `Ship` arm can reach the wire.
            match &output {
                GearResult::Ship(o) => {
                    for target in &remote_subs {
                        self.push_remote_update(&gear_id, o, *target).await;
                    }
                }
                GearResult::Shared(s) => {
                    for target in &remote_subs {
                        self.push_remote_update_shared(&gear_id, s, *target).await;
                    }
                }
                GearResult::Local(_) => {
                    debug_assert!(
                        remote_subs.is_empty(),
                        "local gear output has remote subscribers — routing bug"
                    );
                }
            }

            // Cascade reruns to local dependents.
            for dep in dependents {
                self.kick_loc_gear(dep);
            }

            if !do_rerun {
                return;
            }
        }
    }

    /// Ensure `gear` is active: create it if absent (routing to its owning
    /// core and starting its first computation/subscription) or promote it from
    /// limbo if present. For a newly-created remote gear, sends
    /// `StartSubscription` exactly once (per creation — re-sent only after
    /// eviction + re-creation, since the only way its subscription ends is
    /// eviction, which removes it from the arena). For a newly-created local
    /// gear, registers the event-input edge and kicks a run.
    ///
    /// Existing gears are left alone: an arena-present gear either already has
    /// an output (`Eepy`) or has a computation/subscription in flight
    /// (`Running` / not-yet-answered `StartSubscription`), so there is nothing
    /// to kick or (re-)subscribe — re-kicking a `Running` gear would only flag a
    /// redundant rerun.
    ///
    /// Returns the gear's arena [`GearKey`]. On return an output is either
    /// already cached or a run/subscription that will produce one is in flight,
    /// so callers can [`wait_for_output_unpinned`] unconditionally — no "was it
    /// cold?" flag is needed.
    ///
    /// WARNING: Forces active even if there are no subscribers.
    async fn force_active(self: &Rc<Self>, gear: &R::GearId) -> GearKey {
        // TODO: FATAL: TOCTOU RACE.
        // Existing gear: promote it out of limbo if needed. Nothing to kick or
        // re-subscribe — it already has an output or a run/subscription in flight.
        {
            let mut inner = self.inner.borrow_mut();
            if let Some(&key) = inner.gear_index.get(gear) {
                if let Some(pos) = inner.unref_gear.iter().position(|g| *g == key) {
                    inner.unref_gear.remove(pos);
                }
                return key;
            }
        }
        // New gear: route, then (for a local event gear) allocate its localized
        // group — both via `storage` (async). Done WITHOUT an `inner` borrow so
        // no borrow spans the `.await`.
        let meta = R::meta(gear);
        let group_key = meta.group().clone();
        let target_core = group_key
            .global_hash(&self.storage)
            .map(GlobalCoreId::from_hash)
            .expect("force_active: global_hash")
            .route(self.num_cores);
        enum Reg {
            Event(LocGroupId),
            Timer,
            Follow { target: GearKey },
            Remote,
        }
        let (execution, registration) = if target_core == self.core_id {
            match meta {
                GearMeta::Event { msg_type, group } => {
                    let loc_group = self.storage.mk_loc_group(msg_type, group).await;
                    (
                        ActiveGearExecution::Local {
                            source: GearSource::Events(loc_group),
                            status: ActiveGearStatus::Eepy,
                            dep_set: HashSet::new(),
                        },
                        Reg::Event(loc_group),
                    )
                }
                GearMeta::Timer { group: _, period } => (
                    ActiveGearExecution::Local {
                        source: GearSource::Timer {
                            period,
                            // Due immediately on first activation: the oracle
                            // has never polled, so its first run is `tick = true`.
                            next_due: 0,
                        },
                        status: ActiveGearStatus::Eepy,
                        dep_set: HashSet::new(),
                    },
                    Reg::Timer,
                ),
                GearMeta::Follow {
                    gear: target,
                    baked_group: _,
                } => {
                    // Co-located with the target (routing uses its group), so
                    // the target is local: force-activate it and bake its arena
                    // key into the source. `dep_set` is left empty — the static
                    // follow edge is NOT a `secondary_get` dep; its only
                    // bookkeeping is the reverse `local_dependents` entry wired
                    // up in `Reg::Follow` below (and torn down in `evict_gear`).
                    let target_key = Box::pin(self.force_active(&target)).await;
                    (
                        ActiveGearExecution::Local {
                            source: GearSource::Follow { target: target_key },
                            status: ActiveGearStatus::Eepy,
                            dep_set: HashSet::new(),
                        },
                        Reg::Follow { target: target_key },
                    )
                }
            }
        } else {
            (ActiveGearExecution::Remote { target_core }, Reg::Remote)
        };
        let key = {
            let mut inner = self.inner.borrow_mut();
            let key = inner.gears.insert(ActiveGear {
                id: gear.clone(),
                output: None,
                execution,
                local_dependents: HashSet::new(),
                remote_subscribers: HashSet::new(),
                direct_subscriber_count: 0,
                changed: Epoch::new(),
            });
            inner.gear_index.insert(gear.clone(), key);
            log::debug!(
                target: "dentrado::gear",
                "create core{} {:?} key={:?} live_gears={}",
                self.core_id, gear, key, inner.gears.len()
            );
            match registration {
                Reg::Event(loc_group) => {
                    inner
                        .event_subscriptions
                        .entry(loc_group)
                        .or_default()
                        .insert(key);
                }
                Reg::Timer => {
                    inner.timer_gears.insert(key);
                }
                Reg::Follow { target } => {
                    // Reverse edge of the static follow dep: the target kicks
                    // its `local_dependents` whenever it produces a new output —
                    // that is what re-runs this gear. Done in the same `borrow`
                    // as the arena insert, so both halves of the edge appear
                    // atomically. (The forward half lives in `GearSource`.)
                    if let Some(ag) = inner.gears.get_mut(target) {
                        ag.local_dependents.insert(key);
                    }
                }
                Reg::Remote => {}
            }
            key
        };
        if target_core == self.core_id {
            self.kick_loc_gear(key);
        } else {
            self.send_start_subscription(gear, key, target_core).await;
        }
        key
    }

    /// Wait until the gear at arena `key` has a computed output and return it.
    /// Returns immediately if one is already cached; otherwise awaits the gear's
    /// `changed` event (fired on every completed run / `SubscriptionUpdate`).
    /// Returns `None` if the gear was evicted — the generation tag on `key`
    /// turns that into a safe staleness check instead of a dangling read.
    ///
    /// # This call is *unpinned* — it registers no interest of its own.
    ///
    /// This is the single shared "produce an output" primitive for *all*
    /// consumers, and only one of them is a direct subscriber — so it
    /// deliberately does **not** touch `direct_subscriber_count` or
    /// [`Subscription`] RAII. That means the gear is **not** kept alive by this
    /// wait: if every other interest in it disappears while we're awaiting (a
    /// dependent is evicted, a remote subscriber drops, a `Subscription` is
    /// dropped), `rebalance` is free to evict it out from under us and this
    /// returns `None`. Each caller must therefore register the interest
    /// appropriate to *its* relationship (`local_dependents` for gear→gear
    /// edges, `remote_subscribers` for cross-core subs, `direct_subscriber_count`
    /// via [`Subscription`] for worker reads) **before** awaiting. See
    /// `secondary_get_impl` / `StartSubscription` / `subscribe_gear*`.
    async fn wait_for_output_unpinned(self: &Rc<Self>, key: GearKey) -> Option<GearResult<R>> {
        // No timeout needed: any core death cascades (`CoreDied`) through
        // every core, the runtime drops, and this parked task is cancelled
        // along with its reply channel — callers fail through the drop.
        loop {
            let seen = {
                let inner = self.inner.borrow();
                let Some(ag) = inner.gears.get(key) else {
                    return None;
                };
                if let Some(out) = ag.output.clone() {
                    return Some(out);
                }
                ag.changed.current()
            };
            if !self.wait_change(key, seen).await {
                return None;
            }
        }
    }

    /// Declare a dependency on `gear`'s output and pull its current value,
    /// awaiting it if not yet computed. Records **both** halves of the gear→gear
    /// edge eagerly, **before** awaiting: the reverse edge
    /// (`caller` ∈ `gear.local_dependents` — the interest that keeps `gear` from
    /// being evicted while we wait) and the forward edge (`gear` ∈ the caller's
    /// `dep_set`). The forward edge is rewritten from `GearCtx::deps` at the end
    /// of `run_step`; recording it eagerly means that if the run is cancelled
    /// mid-flight (the caller's `JoinHandle` is dropped on eviction),
    /// `evict_gear` still walks a `dep_set` that includes every dependency we
    /// declared this run — so no reverse edge is left orphaned in a dependency
    /// we never reconciled.
    async fn secondary_get_impl(
        self: &Rc<Self>,
        caller: R::GearId,
        gear: R::GearId,
    ) -> GearResult<R> {
        let gear_key = self.force_active(&gear).await;
        {
            let mut inner = self.inner.borrow_mut();
            let Some(caller_key) = inner.gear_index.get(&caller).copied() else {
                panic!("secondary_get_impl: caller not active");
            };
            if let Some(ag) = inner.gears.get_mut(gear_key) {
                ag.local_dependents.insert(caller_key);
            }
            if let Some(caller_ag) = inner.gears.get_mut(caller_key)
                && let ActiveGearExecution::Local { dep_set, .. } = &mut caller_ag.execution
            {
                dep_set.insert(gear_key);
            }
        }
        self.wait_for_output_unpinned(gear_key)
            .await
            .expect("secondary_get_impl: dependency evicted while awaiting its output")
    }

    pub(crate) fn current_output_key(&self, key: GearKey) -> Option<GearResult<R>> {
        let inner = self.inner.borrow();
        inner.gears.get(key).and_then(|ag| ag.output.clone())
    }

    /// Current change-epoch of the gear at `key`, or `None` if it has been
    /// evicted.
    pub(crate) fn change_epoch(&self, key: GearKey) -> Option<u64> {
        self.inner
            .borrow()
            .gears
            .get(key)
            .map(|ag| ag.changed.current())
    }

    /// Park until the gear's change-epoch advances past `seen`, or the gear is
    /// evicted. `seen` must be captured *before* the caller last observed the
    /// gear's state, so a change landing between capture and park is not lost.
    /// Returns `false` if the gear was evicted while waiting.
    pub(crate) async fn wait_change(self: &Rc<Self>, key: GearKey, seen: u64) -> bool {
        let core = Rc::clone(self);
        poll_fn(move |cx| {
            let mut inner = core.inner.borrow_mut();
            let Some(ag) = inner.gears.get_mut(key) else {
                return Poll::Ready(false);
            };
            if ag.changed.current() == seen {
                ag.changed.park(cx);
                Poll::Pending
            } else {
                Poll::Ready(true)
            }
        })
        .await
    }

    fn is_locally_running_key(&self, key: GearKey) -> bool {
        let inner = self.inner.borrow();
        inner.gears.get(key).is_some_and(|ag| {
            matches!(
                &ag.execution,
                ActiveGearExecution::Local { status, .. } if !matches!(status, ActiveGearStatus::Eepy)
            )
        })
    }

    /// Register one direct (worker-side) subscriber for `key`. The matching
    /// decrement is owned by the [`Subscription`] handed back to the caller, so
    /// callers must construct that `Subscription` before any subsequent `.await`
    /// — that way a cancelled wait still drops the `Subscription` and decrements.
    fn inc_direct_subscriber(&self, key: GearKey) {
        let mut inner = self.inner.borrow_mut();
        let ag = inner
            .gears
            .get_mut(key)
            .expect("inc_direct_subscriber: gear evicted immediately after force_active");
        ag.direct_subscriber_count += 1;
        log::debug!(
            target: "dentrado::gear",
            "subscribe core{} {:?} direct_subscriber_count={} local_dependents={} remote_subscribers={}",
            self.core_id, ag.id, ag.direct_subscriber_count,
            ag.local_dependents.len(), ag.remote_subscribers.len()
        );
    }

    /// Decrement the direct-subscriber count for `key`; returns `true` if the
    /// gear consequently lost all interest (the caller should then rebalance).
    /// Matching increment: [`Core::inc_direct_subscriber`].
    pub(crate) fn release_direct_subscriber(&self, key: GearKey) -> bool {
        let mut inner = self.inner.borrow_mut();
        let Some(ag) = inner.gears.get_mut(key) else {
            return false;
        };
        ag.direct_subscriber_count = ag.direct_subscriber_count.saturating_sub(1);
        !ag.has_interest()
    }

    // --- cross-core subscription wiring ---

    async fn build_gear_wire(&self, gear: &R::GearId) -> (R::GearId, Arc<WireLocCtx<R>>) {
        let mut builder = WireLocCtxBuilder::new(&self.storage);
        let gear_wire = builder
            .remap(gear.clone())
            .await
            .expect("build_gear_wire: gear remap");
        (gear_wire, Arc::new(builder.build()))
    }

    async fn send_start_subscription(
        &self,
        gear: &R::GearId,
        subscriber_key: GearKey,
        target: u32,
    ) {
        let (gear_wire, wire_ctx) = self.build_gear_wire(gear).await;
        let _ = self.intercore_tx[target as usize].send(InterCoreMsg::StartSubscription {
            gear: gear_wire,
            wire_ctx,
            from_core: self.core_id,
            session: subscriber_key,
        });
        self.doorbells[target as usize].ring();
    }

    async fn push_remote_update(&self, gear: &R::GearId, output: &R::GearOut, target: u32) {
        let mut builder = WireLocCtxBuilder::new(&self.storage);
        let gear_wire = builder
            .remap(gear.clone())
            .await
            .expect("push_remote_update: gear remap");
        let output_wire = builder
            .remap(output.clone())
            .await
            .expect("push_remote_update: output remap");
        let wire_ctx = Arc::new(builder.build());
        let _ = self.intercore_tx[target as usize].send(InterCoreMsg::SubscriptionUpdate {
            gear: gear_wire,
            output: output_wire,
            wire_ctx,
        });
        self.doorbells[target as usize].ring();
    }

    /// Install a fresh [`GearProduce`] into its stored [`GearResult`]: a
    /// `Shared` produce is boxed into an immutable [`SharedData`] payload and
    /// registered in this core's [`SharedArena`] (`xcount = 1`, this handle),
    /// then wrapped in an owner-local [`Shared`]; `Ship`/`Local` pass through
    /// unchanged.
    fn install_produce(self: &Rc<Self>, produce: GearProduce<R>) -> GearResult<R> {
        match produce {
            GearProduce::Ship(o) => GearResult::Ship(o),
            GearProduce::Local(o) => GearResult::Local(o),
            GearProduce::Shared(v) => {
                let data = SharedData::new(v);
                let key = self.shared_arena.borrow_mut().insert(data);
                GearResult::Shared(Shared::new_owner(
                    data,
                    key,
                    self.shared_bus(),
                    self.core_id,
                ))
            }
        }
    }

    /// The owner side of shipping a shared output to a subscriber: bump the
    /// cross-core refcount (arena `xcount += 1`, direct — we are the owner)
    /// *before* sending (one more core will hold a claim), then push both the
    /// immutable payload pointer and the unref key as a
    /// `SubscriptionUpdateShared`.
    async fn push_remote_update_shared(
        self: &Rc<Self>,
        gear: &R::GearId,
        shared: &Shared<R>,
        target: u32,
    ) {
        let data = shared.data();
        let key = shared.key();
        self.shared_arena.borrow_mut().inc(key);
        let (gear_wire, wire_ctx) = self.build_gear_wire(gear).await;
        let _ = self.intercore_tx[target as usize].send(InterCoreMsg::SubscriptionUpdateShared {
            gear: gear_wire,
            data: RemoteShared::from_ptr(data),
            key,
            wire_ctx,
        });
        self.doorbells[target as usize].ring();
    }

    /// Wrap a freshly-received foreign payload as a [`Shared`] handle on this
    /// core. The owner already retained it for this push; this handle's `Drop`
    /// sends the balancing `SharedUnref`.
    fn shared_from_remote(
        self: &Rc<Self>,
        data: RemoteShared<SharedData<R>>,
        key: SharedKey,
        owner: u32,
    ) -> Shared<R> {
        Shared::new_foreign(data.as_ptr(), key, self.shared_bus(), owner)
    }

    /// `Weak<dyn SharedBus>` view of this core — how a [`Shared`] handle minted
    /// here routes an unref to its owner. `Weak` (not `Rc`) breaks the
    /// `Core → ActiveGear → Shared → Core` cycle, and `std::rc::Weak::upgrade`
    /// is non-atomic.
    fn shared_bus(self: &Rc<Self>) -> Weak<dyn SharedBus> {
        let bus: Rc<dyn SharedBus> = self.clone();
        Rc::downgrade(&bus)
    }

    /// Rebalance a gear by its arena key. Any `StopSubscription` messages
    /// produced by cascade evictions are emitted directly by `CoreLocCtx` via
    /// the borrowed [`StopCtx`] (channels/doorbells/core id) — no `Vec` of
    /// pending stops is round-tripped through `Core`.
    pub(crate) fn rebalance_key(self: &Rc<Self>, key: GearKey) {
        let stop_ctx = StopCtx {
            intercore_tx: &self.intercore_tx,
            doorbells: &self.doorbells,
            core_id: self.core_id,
        };
        self.inner.borrow_mut().rebalance_gear(key, &stop_ctx);
    }

    /// Import events into this core, optionally forwarding to inter-node peers,
    /// then schedule reruns for any active gear whose inputs were touched.
    async fn post_events(
        self: &Rc<Self>,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: &Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: &[u32],
        source_node: Option<NodeId>,
    ) -> Result<(), MergeError> {
        let node_id = self.node_id;
        // Merge via storage (async; no `inner` borrow spans the await), then
        // kick dirty gears under a split borrow of `inner`.
        let mut dirty: HashSet<LocGroupId> = HashSet::new();
        {
            let mut merger = WireLocCtxMerger::new(&wire_ctx, &self.storage);
            for &idx in seed_indices {
                let event = &events[idx as usize];
                let (group_id, store_result) = merger
                    .import_new_event(event.clone(), timestamp, source_node.unwrap_or(node_id))
                    .await?;
                if store_result.is_some() {
                    dirty.insert(group_id);
                }
            }
        }
        {
            let mut inner = self.inner.borrow_mut();
            let CoreLocCtx {
                gears,
                event_subscriptions,
                ..
            } = &mut *inner;
            for group in dirty {
                let Some(keys) = event_subscriptions.get(&group) else {
                    continue;
                };
                for key in keys.iter().copied() {
                    debug_assert!(
                        gears.get(key).is_some(),
                        "event_subscription gear is missing from gears"
                    );
                    Self::kick_loc_gear_in(gears, self, key);
                }
            }
        }
        if source_node.is_none() {
            // TODO: Don't pass wire_ctx, pass only the relevant subpart of it. I. e. update WireLocCtxMereger to regenerate
            let events = seed_indices
                .iter()
                .map(|&idx| events[idx as usize].clone())
                .collect();
            let global_core_ids = seed_indices
                .iter()
                .map(|&idx| global_core_ids[idx as usize])
                .collect();
            self.forward_to_peers(wire_ctx, events, global_core_ids, timestamp);
        }
        Ok(())
    }

    /// Handle a `RunGear` operation: localize the gear, then read its output
    /// once via a short-lived [`Subscription`] (`read_gear`). That force-
    /// activates the gear, registers a direct subscriber for the duration of the
    /// wait (so it can't be evicted out from under us — unlike a bare
    /// `force_active` + `wait_for_output`, which left the gear with *no* interest
    /// and either leaked a hot slot or panicked under limbo pressure), and drops
    /// on completion so the gear can rebalance. Async — the caller is expected
    /// to be a spawned task.
    pub(crate) async fn run_gear(
        self: &Rc<Self>,
        gear: R::GearId,
        wire_ctx: &WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        let gear = {
            let mut merger = WireLocCtxMerger::new(wire_ctx, &self.storage);
            merger.remap(gear).await.map_err(RunGearError::Merge)?
        };
        // The reply crosses a thread (flume); it must be the shippable type. A
        // local gear reached via `RunGear` is a routing error.
        self.read_gear(gear)
            .await
            .into_ship()
            .ok_or(RunGearError::NotShippable)
    }

    /// Handle a `ClientOp` (received from a channel that is).
    pub(crate) async fn handle_client_op(self: &Rc<Self>, op: CoreCmd<R>) {
        match op {
            CoreCmd::PostEvents {
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
                seed_indices,
                forwarded_from,
                reply,
            } => {
                let result = self
                    .post_events(
                        wire_ctx,
                        events,
                        &global_core_ids,
                        timestamp,
                        &seed_indices,
                        forwarded_from,
                    )
                    .await;
                if let Some(reply) = reply {
                    reply
                        .send(result)
                        .expect("PostEvents: reply channel closed");
                }
            }
            CoreCmd::RunGear {
                gear,
                wire_ctx,
                reply,
            } => {
                let this = Rc::clone(self);
                compio::runtime::spawn(this.clone().link_task(async move {
                    let result = this.run_gear(gear, &wire_ctx).await;
                    let _ = reply.send(result);
                }))
                .detach();
            }
        }
    }

    #[must_use]
    pub(crate) async fn handle_intercore_msg(
        self: &Rc<Self>,
        msg: InterCoreMsg<R>,
        from_core: u32,
    ) -> HandleMsgResult {
        match msg {
            InterCoreMsg::Op(op) => self.handle_client_op(op).await,
            InterCoreMsg::StartSubscription {
                gear,
                wire_ctx,
                from_core,
                session,
            } => {
                let this = Rc::clone(self);
                compio::runtime::spawn(this.clone().link_task(async move {
                    let gear = {
                        let mut merger = WireLocCtxMerger::new(&wire_ctx, &this.storage);
                        merger
                            .remap(gear)
                            .await
                            .expect("StartSubscription: failed to localize gear")
                    };
                    // Register the remote subscriber, ensure a run, await output,
                    // then push it back. The subscriber is attached before the
                    // await, so the gear can't be evicted mid-wait.
                    let key = this.force_active(&gear).await;
                    {
                        let mut inner = this.inner.borrow_mut();
                        if let Some(ag) = inner.gears.get_mut(key) {
                            ag.remote_subscribers.insert(from_core);
                        }
                        // Route table for the eventual StopSubscription (which
                        // carries only `session`, no gear to localize).
                        inner.incoming_subs.insert((from_core, session), key);
                    }
                    let output = this.wait_for_output_unpinned(key).await.expect(
                        "StartSubscription: gear evicted while a remote subscriber was attached",
                    );
                    // A remote subscription is satisfied by a shippable or a
                    // shared output; a `Local` gear reached this way is a
                    // routing bug.
                    match &output {
                        GearResult::Ship(o) => {
                            this.push_remote_update(&gear, o, from_core).await;
                        }
                        GearResult::Shared(s) => {
                            this.push_remote_update_shared(&gear, s, from_core).await;
                        }
                        GearResult::Local(_) => {
                            debug_assert!(
                                false,
                                "StartSubscription for a non-shippable (local) gear — routing bug"
                            );
                        }
                    }
                }))
                .detach();
            }
            InterCoreMsg::SubscriptionUpdate {
                gear,
                output,
                wire_ctx,
            } => {
                let (gear, output) = {
                    let mut merger = WireLocCtxMerger::new(&wire_ctx, &self.storage);
                    let gear = merger
                        .remap(gear)
                        .await
                        .expect("SubscriptionUpdate: failed to localize gear");
                    let output = merger
                        .remap(output)
                        .await
                        .expect("SubscriptionUpdate: failed to localize output");
                    (gear, output)
                };
                let dependents = {
                    let mut inner = self.inner.borrow_mut();
                    let Some(key) = inner.gear_index.get(&gear).copied() else {
                        return HandleMsgResult::Ok;
                    };
                    let Some(ag) = inner.gears.get_mut(key) else {
                        return HandleMsgResult::Ok;
                    };
                    ag.output = Some(GearResult::Ship(output));
                    ag.changed.bump();
                    ag.local_dependents.iter().copied().collect::<Vec<_>>()
                };
                for dep in dependents {
                    self.kick_loc_gear(dep);
                }
            }
            InterCoreMsg::SubscriptionUpdateShared {
                gear,
                data,
                key,
                wire_ctx,
            } => {
                let gear = {
                    let mut merger = WireLocCtxMerger::new(&wire_ctx, &self.storage);
                    merger
                        .remap(gear)
                        .await
                        .expect("SubscriptionUpdateShared: failed to localize gear")
                };
                let shared = self.shared_from_remote(data, key, from_core);
                let dependents = {
                    let mut inner = self.inner.borrow_mut();
                    let Some(key) = inner.gear_index.get(&gear).copied() else {
                        return HandleMsgResult::Ok;
                    };
                    let Some(ag) = inner.gears.get_mut(key) else {
                        return HandleMsgResult::Ok;
                    };
                    ag.output = Some(GearResult::Shared(shared));
                    ag.changed.bump();
                    ag.local_dependents.iter().copied().collect::<Vec<_>>()
                };
                for dep in dependents {
                    self.kick_loc_gear(dep);
                }
            }
            InterCoreMsg::SharedUnref { key } => {
                // Owner thread: release one cross-core claim, reclaim payload on
                // zero (after the arena borrow is released, so the payload's own
                // `Drop` can never reenter the arena).
                let data = self.shared_arena.borrow_mut().dec(key);
                if let Some(data) = data {
                    SharedData::<R>::reclaim(data);
                }
            }
            InterCoreMsg::StopSubscription { session, from_core } => {
                // No localization: route purely by the opaque session token.
                self.rebalance_remote_unsub(from_core, session);
            }
            // A peer's core thread died — cascade: stop this core too. The
            // event loop exits on `is_dead` right after the handler returns.
            InterCoreMsg::CoreDied => {
                return HandleMsgResult::Die;
            }
        }
        HandleMsgResult::Ok
    }

    /// Remove a remote subscriber and rebalance the (local) gear.
    fn rebalance_remote_unsub(self: &Rc<Self>, from_core: u32, session: GearKey) {
        let stop_ctx = StopCtx {
            intercore_tx: &self.intercore_tx,
            doorbells: &self.doorbells,
            core_id: self.core_id,
        };
        let mut inner = self.inner.borrow_mut();
        let Some(key) = inner.incoming_subs.remove(&(from_core, session)) else {
            return;
        };
        if let Some(ag) = inner.gears.get_mut(key) {
            ag.remote_subscribers.remove(&from_core);
        }
        inner.rebalance_gear(key, &stop_ctx);
    }

    pub(crate) async fn handle_inter_node_msg(
        self: &Rc<Self>,
        peer_idx: usize,
        msg: InterNodeMsg<R>,
    ) {
        let source_node = self.inter_node_peers[peer_idx].0;
        match msg {
            InterNodeMsg::ForwardEvents {
                wire_ctx,
                events,
                timestamp,
            } => self
                .db_post_events(wire_ctx, events, timestamp, (Some(source_node), || None))
                .await
                .expect("Received invalid push from server of the cluster"), // TODO: Don't fail.
        }
    }

    pub(crate) fn handle_reroute_msg(&self, msg: RerouteMsg<R>) {
        match msg {
            RerouteMsg::ForwardToPeer {
                peer_idx,
                wire_ctx,
                events,
                timestamp,
            } => {
                let (sender, doorbell) = self
                    .inter_node_peers
                    .get(peer_idx)
                    .and_then(|(_, _, s)| s.as_ref())
                    .expect("handle_reroute_msg: no channel to peer");
                let _ = sender.send(InterNodeMsg::ForwardEvents {
                    wire_ctx,
                    events,
                    timestamp,
                });
                doorbell.ring();
            }
        }
    }

    fn forward_to_peers(
        &self,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        global_core_ids: Vec<GlobalCoreId>,
        timestamp: u32,
    ) {
        for (peer_idx, (_node_id, remote_num_cores, sender_opt)) in
            self.inter_node_peers.iter().enumerate()
        {
            if let Some((sender, doorbell)) = sender_opt {
                let _ = sender.send(InterNodeMsg::ForwardEvents {
                    wire_ctx: (*wire_ctx).clone(),
                    events: events.clone(),
                    timestamp,
                });
                doorbell.ring();
            } else {
                let mut proxy_groups: HashMap<u32, Vec<u32>> = HashMap::new();
                for (i, gcid) in global_core_ids.iter().enumerate() {
                    let proxy_core = gcid.route(*remote_num_cores);
                    proxy_groups.entry(proxy_core).or_default().push(i as u32);
                }

                for (proxy_core, seed_indices) in proxy_groups {
                    let proxy_events: Vec<_> = seed_indices
                        .iter()
                        .map(|&idx| events[idx as usize].clone())
                        .collect();

                    let _ = self.reroute_tx[proxy_core as usize].send(RerouteMsg::ForwardToPeer {
                        peer_idx,
                        wire_ctx: (*wire_ctx).clone(),
                        events: proxy_events,
                        timestamp,
                    });
                    self.doorbells[proxy_core as usize].ring();
                }
            }
        }
    }

    // Send commands to db via this Core

    /// Post events, routing each to the correct core.
    /// Self-targeting events call `Core::do_post_events` directly.
    /// Remote events go through SPSC `intercore_tx`.
    pub async fn db_post_events(
        self: &Rc<Self>,
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
        (forwarded_from, mut mk_reply): (
            Option<NodeId>,
            impl FnMut() -> Option<flume::Sender<Result<(), MergeError>>>,
        ),
    ) -> Result<(), MergeError> {
        let routed = db::route_events(wire_ctx, events, self.num_cores())?;

        let mut our_task = None;
        for (target_core, seed_indices) in routed.core_seeds {
            if target_core == self.core_id() {
                // Direct call on this core: no channel overhead.
                our_task = Some(seed_indices);
            } else {
                // Remote: send through SPSC intercore channel
                let op = CoreCmd::PostEvents {
                    wire_ctx: Arc::clone(&routed.wire_ctx),
                    events: Arc::clone(&routed.events),
                    global_core_ids: Arc::clone(&routed.global_core_ids),
                    timestamp,
                    seed_indices,
                    forwarded_from,
                    reply: mk_reply(),
                };
                self.intercore_tx[target_core as usize]
                    .send(InterCoreMsg::Op(op))
                    .expect("post_events: intercore channel closed");
                self.doorbells[target_core as usize].ring();
            }
        }

        if let Some(seed_indices) = our_task {
            self.post_events(
                routed.wire_ctx,
                routed.events,
                &routed.global_core_ids,
                timestamp,
                &seed_indices,
                forwarded_from,
            )
            .await?;
        }
        Ok(())
    }

    /// Run a gear on the core that owns it.
    /// Self-targeting: calls `Core::do_run_gear` directly.
    /// Remote: sends through SPSC `intercore_tx`.
    pub async fn db_run_gear(
        self: &Rc<Self>,
        gear: R::GearId,
        wire_ctx: WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        let target_core = db::route_gear(&gear, &wire_ctx, self.num_cores())?;

        if target_core == self.core_id {
            // Direct call on this core.
            self.run_gear(gear, &wire_ctx).await
        } else {
            // Remote: send through SPSC intercore channel
            let (reply_tx, reply_rx) = flume::bounded(1);
            let op = CoreCmd::RunGear {
                gear,
                wire_ctx,
                reply: reply_tx,
            };
            self.intercore_tx[target_core as usize]
                .send(InterCoreMsg::Op(op))
                .expect("run_gear: intercore channel closed");
            self.doorbells[target_core as usize].ring();
            reply_rx.recv_async().await.expect("channel closed")
        }
    }

    // --- worker-facing subscription API ---

    /// Subscribe to a gear's output, returning a **fresh** value: if the gear is
    /// local and currently computing (`Running`), waits for that run to land so
    /// `current()` reads the latest value rather than a stale one. Otherwise
    /// behaves like `subscribe_gear_stale`.
    pub async fn subscribe_gear(self: &Rc<Self>, gear: R::GearId) -> Subscription<R, S> {
        let key = self.force_active(&gear).await;
        // Register interest BEFORE awaiting. `inc_direct_subscriber` is
        // immediately followed by constructing `sub` with no `.await` in
        // between, so if the wait below is cancelled the already-moved `sub` is
        // dropped and its `Drop` decrements the count — no leak. Registering
        // first also pins the gear (`has_interest`), so `key` can't go stale
        // during the wait and there is nothing to rebind. The `Subscription`
        // stores that `key` directly (not the `R::GearId`), so `Drop`/`current`/
        // `next` skip the `gear_index` lookup.
        self.inc_direct_subscriber(key);
        let sub = Subscription {
            core: Rc::clone(self),
            key,
        };
        // Fresh: wait for the first output, and for any in-flight run, so
        // `current()` returns the latest value rather than a pre-recompute one.
        if self.current_output_key(key).is_none() || self.is_locally_running_key(key) {
            let _ = self.wait_for_output_unpinned(key).await;
        }
        sub
    }

    /// Subscribe to a gear's output, returning immediately with whatever output
    /// is currently available — waiting only for the *first* computation if the
    /// gear is cold, never for an in-flight recompute. Reactivity reconciles
    /// later via `changed`/dependent reruns.
    pub async fn subscribe_gear_stale(self: &Rc<Self>, gear: R::GearId) -> Subscription<R, S> {
        let key = self.force_active(&gear).await;
        // See `subscribe_gear`: interest goes up before the await so the key
        // can't go stale and a cancelled wait still decrements via `Drop`.
        self.inc_direct_subscriber(key);
        let sub = Subscription {
            core: Rc::clone(self),
            key,
        };
        if self.current_output_key(key).is_none() {
            let _ = self.wait_for_output_unpinned(key).await;
        }
        sub
    }

    /// Read a gear's current output once (fresh). Implemented as a short-lived
    /// subscription.
    pub async fn read_gear(self: &Rc<Self>, gear: R::GearId) -> GearResult<R> {
        let sub = self.subscribe_gear(gear).await;
        let out = sub.current();
        drop(sub);
        out
    }

    /// Read a gear's current output once (stale — does not wait for in-flight
    /// runs). Implemented as a short-lived subscription.
    pub async fn read_gear_stale(self: &Rc<Self>, gear: R::GearId) -> GearResult<R> {
        let sub = self.subscribe_gear_stale(gear).await;
        let out = sub.current();
        drop(sub);
        out
    }
}

/// The context handed to `IsRuntime::run_step`. Carries the gear's own id, a
/// handle to the live `Core` (via `Deref`, so `core.group_store()` keeps
/// working unchanged), and the per-run `deps` set
/// accumulated by `secondary_get` calls (reconciled against the gear's stored
/// `dep_set` at run end).
pub struct GearCtx<R: IsRuntime, S: Storage<R>> {
    pub(crate) core: Rc<Core<R, S>>,
    pub(crate) gear: R::GearId,
    /// Deps accumulated by `secondary_get`. Interior-mutable so `secondary_get`
    /// can be `&self` (lets a `dep_resolver` closure share `ctx` with sibling
    /// closures that read `ctx` immutably).
    pub(crate) deps: RefCell<HashSet<R::GearId>>,
}

impl<R: IsRuntime, S: Storage<R>> Deref for GearCtx<R, S> {
    type Target = Core<R, S>;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl<R: IsRuntime, S: Storage<R>> GearCtx<R, S> {
    /// The id of the gear currently running.
    pub fn gear(&self) -> &R::GearId {
        &self.gear
    }

    /// The underlying `Core`.
    pub fn core(&self) -> &Rc<Core<R, S>> {
        &self.core
    }

    /// The per-core storage backend.
    pub fn storage(&self) -> &S {
        &self.core.storage
    }

    /// Declare a dependency on `dep`'s output and pull its current value
    /// (awaiting it if not yet computed). Records the edge `self.gear → dep`
    /// (both the forward `deps` entry here and the reverse
    /// `dep.local_dependents` entry in the core) so that when `dep` changes,
    /// this gear reruns.
    pub async fn secondary_get(&self, dep: R::GearId) -> GearResult<R> {
        self.deps.borrow_mut().insert(dep.clone());
        self.core.secondary_get_impl(self.gear.clone(), dep).await
    }
}

/// Borrowed cross-core send plumbing handed to `CoreLocCtx` during a
/// rebalance/eviction so it can emit `StopSubscription` messages directly —
/// without returning a `Vec` of pending stops for `Core` to drain, and without
/// re-borrowing the `inner` `RefCell` (which `Core`'s send helpers would need).
/// Carries only the channels, doorbells, and owning core id; the wire context
/// is still built from `CoreLocCtx::loc_ctx`.
struct StopCtx<'a, R: IsRuntime> {
    intercore_tx: &'a [mpsc::Sender<InterCoreMsg<R>>],
    doorbells: &'a [DoorbellHandle],
    core_id: u32,
}

/// How a [`Shared`] handle on this core, on dropping its core's *last* local
/// reference, releases that core's cross-core claim: an **owner** handle hits
/// the arena directly; a **foreign** handle forwards a `SharedUnref` over the
/// inter-core channel (FIFO per core-pair ⇒ unrefs from one core land in order,
/// so the owner's `xcount` never goes negative).
impl<R: IsRuntime, S: Storage<R>> SharedBus for Core<R, S> {
    fn shared_local_unref(&self, key: SharedKey) {
        let data = self.shared_arena.borrow_mut().dec(key);
        if let Some(data) = data {
            SharedData::<R>::reclaim(data);
        }
    }

    fn shared_unref(&self, owner: u32, key: SharedKey) {
        let _ = self.intercore_tx[owner as usize].send(InterCoreMsg::SharedUnref { key });
        self.doorbells[owner as usize].ring();
    }
}

impl<R: IsRuntime> CoreLocCtx<R> {
    /// Emit a `StopSubscription` carrying only the opaque `session` id. No
    /// localization is read here: `send_stop` is on the `Drop`-driven eviction
    /// path (`evict_gear`), and `Drop` is synchronous, so it must not depend on
    /// state that will become async.
    fn send_stop(&self, session: GearKey, target: u32, stop_ctx: &StopCtx<'_, R>) {
        let _ = stop_ctx.intercore_tx[target as usize].send(InterCoreMsg::StopSubscription {
            session,
            from_core: stop_ctx.core_id,
        });
        stop_ctx.doorbells[target as usize].ring();
    }

    /// If `gear` has no external interest, append it to the limbo deque. If
    /// limbo exceeds capacity, evict the oldest entry (full teardown, cascading
    /// to dependencies). Remote `StopSubscription` messages produced by any
    /// eviction are emitted directly via `stop_ctx`.
    fn rebalance_gear(&mut self, key: GearKey, stop_ctx: &StopCtx<'_, R>) {
        let has_interest = self.gears.get(key).is_some_and(ActiveGear::has_interest);
        if has_interest {
            return;
        }
        // Already in limbo?
        if self.unref_gear.iter().any(|k| *k == key) {
            return;
        }
        if self.gears.get(key).is_none() {
            return;
        }
        self.unref_gear.push_back(key);
        while self.unref_gear.len() > LIMBO_CAPACITY {
            let Some(evicted) = self.unref_gear.pop_front() else {
                break;
            };
            if let Some(ag) = self.gears.remove(evicted) {
                self.evict_gear(evicted, ag, stop_ctx);
            }
        }
    }

    /// Fully tear down a gear: fire remote `StopSubscription` if it was a
    /// subscribed remote dep, drop its trigger registration (event group or
    /// epoch counter), remove ourselves from each dependency's
    /// `local_dependents`, and cascade-rebalance dependencies that lose their
    /// last dependent. The dependency graph is acyclic by construction, so this
    /// terminates. Remote stops are emitted directly via `stop_ctx`.
    fn evict_gear(&mut self, key: GearKey, ag: ActiveGear<R>, stop_ctx: &StopCtx<'_, R>) {
        let gear_id = ag.id.clone();
        self.gear_index.remove(&gear_id);
        let dep_set = match ag.execution {
            ActiveGearExecution::Remote { target_core } => {
                // Session id = this (subscriber) gear's own arena key. The
                // receiver routes the stop by it alone — no gear, no wire_ctx,
                // no localization read on this `Drop`-driven path.
                self.send_stop(key, target_core, stop_ctx);
                HashSet::new()
            }
            ActiveGearExecution::Local {
                source, dep_set, ..
            } => {
                match source {
                    GearSource::Events(group) => {
                        if let Some(set) = self.event_subscriptions.get_mut(&group) {
                            set.remove(&key);
                            if set.is_empty() {
                                self.event_subscriptions.remove(&group);
                            }
                        }
                    }
                    GearSource::Timer { .. } => {
                        self.timer_gears.remove(&key);
                    }
                    GearSource::Follow { target } => {
                        // Tear down the static follow edge's reverse half:
                        // remove ourselves from the target's `local_dependents`
                        // and rebalance it (it may lose its last dependent).
                        // Mirrors the generic `dep_set` walk below for dynamic
                        // edges. (If the gear also `secondary_get`s its own
                        // target, the duplicate removal/rebalance is a harmless
                        // no-op.)
                        if let Some(dag) = self.gears.get_mut(target) {
                            dag.local_dependents.remove(&key);
                        }
                        self.rebalance_gear(target, stop_ctx);
                    }
                }
                dep_set
            }
        };
        // Gear-dep edges: drop ourselves from each dependency, then cascade.
        for dep in &dep_set {
            if let Some(dag) = self.gears.get_mut(*dep) {
                dag.local_dependents.remove(&key);
            }
            self.rebalance_gear(*dep, stop_ctx);
        }
        // NOTE: `gear_cache` is intentionally NOT touched here. It is keyed by
        // the stable `R::GearId` and persists across eviction/reactivation so a
        // returning gear resumes from its old working state (e.g. its watermark).
    }
}
