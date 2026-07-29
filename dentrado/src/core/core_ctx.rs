use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    num::NonZero,
    rc::Rc,
    sync::{Arc, mpsc},
    time::Duration,
};

use compio::runtime::JoinHandle;
use slotmap::{SlotMap, new_key_type};
use synchrony::unsync::event::Event;

use crate::{
    core::{
        db,
        doorbell::DoorbellHandle,
        gear::{GearInput, GearMeta, IsRuntime},
        loc_ctx::{
            EventContext, GroupEventSource, GroupStore, LocCtx, StoreResultSuccess, StoredEvent,
        },
    },
    types::{
        DataId, DataVerifyError, GlobalCoreId, GroupEventId, LocDataId, LocGroupId, LocMsgTypeId,
        LocSenderId, LocUserId, NodeId, SenderPk, UserId,
    },
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
    StopSubscription {
        /// The session id from the matching [`InterCoreMsg::StartSubscription`].
        session: GearKey,
        from_core: u32,
    },
}

new_key_type! {
    /// Opaque, generational handle into [`CoreLocCtx::gears`]. Cheap to copy and
    /// store in edge sets (no `R::GearId` cloning). The generation tag makes it safe to reuse.
    /// Do we really need one, though? Maybe not necessarily.
    pub(crate) struct GearKey;
}

#[derive(Debug)]
struct CoreLocCtx<R: IsRuntime> {
    /// Per-gear working state (`R::GearCache`), keyed by the **stable**
    /// `R::GearId`. Persistent across activation/eviction cycles: a gear that
    /// is evicted from the arena and later reactivated picks up its old cache
    /// (so, e.g., the watermark it stores is preserved). Stays in RAM — the hot
    /// cache is not routed through the async `Storage` trait.
    gear_cache: HashMap<R::GearId, R::GearCache>,
    loc_ctx: LocCtx<R>,
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
    pub(crate) output: Option<R::GearOut>,
    pub(crate) execution: ActiveGearExecution,
    /// Gears (on this core) that depend on this one (forward gear-dep index).
    /// For a remote gear, these are local gears that read it via `secondary_get`.
    pub(crate) local_dependents: HashSet<GearKey>,
    /// Remote cores subscribed to this gear's output (for local gears only). Stored to know whom to notify.
    pub(crate) remote_subscribers: HashSet<u32>,
    /// Count of direct (worker-side) subscribers. Eager demote trigger.
    pub(crate) direct_subscriber_count: usize,
    /// Fan-out for output updates to direct subscribers / `secondary_get`
    /// awaiters. Persistent: created on first activate, notified on every
    /// completed run (local) or `SubscriptionUpdate` (remote).
    /// TODO: Replace with other mechanism?
    pub(crate) changed: Event,
}

impl<R: IsRuntime> ActiveGear<R> {
    /// Whether anything still cares about this gear's output.
    fn has_interest(&self) -> bool {
        !self.local_dependents.is_empty()
            || !self.remote_subscribers.is_empty()
            || self.direct_subscriber_count > 0
    }
}

#[derive(Debug)]
pub struct Core<R: IsRuntime> {
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

    inner: RefCell<CoreLocCtx<R>>,
}

impl<R: IsRuntime> Core<R> {
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
            inner: RefCell::new(CoreLocCtx {
                gear_cache: HashMap::new(),
                loc_ctx: LocCtx::new(),
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

    #[must_use]
    pub(crate) fn module(&self) -> &R::Module {
        &self.module
    }

    #[must_use]
    pub(crate) fn core_id(&self) -> u32 {
        self.core_id
    }

    #[must_use]
    pub(crate) fn node_id(&self) -> NodeId {
        self.node_id
    }

    #[must_use]
    pub(crate) fn num_cores(&self) -> NonZero<u32> {
        self.num_cores
    }

    #[must_use]
    /// Panics if `Fn` accesses `Core`.
    pub fn get_stored_event<F>(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
        f: impl Fn(&StoredEvent<R::Body>) -> F,
    ) -> Option<F> {
        self.inner.borrow().loc_ctx.get_stored_event(group, slot, f)
    }

    /// A group-bound read view over this core, for the one group a gear runs
    /// for. See [`GroupStore`].
    #[must_use]
    pub fn group_store(&self, group: LocGroupId) -> GroupStore<'_, R> {
        GroupStore::new(self, group)
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
                let handle = compio::runtime::spawn(Self::run_loc_gear_task(self_rc.clone(), key));
                *status = ActiveGearStatus::Running {
                    handle,
                    rerun: false,
                }
            }
            ActiveGearStatus::Running { rerun, .. } => {
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
            let (gear_id, mut cache, input) = {
                let mut inner = self.inner.borrow_mut();
                let epoch = inner.epoch;
                let CoreLocCtx {
                    gears, gear_cache, ..
                } = &mut *inner;
                let Some(ag) = gears.get_mut(key) else {
                    return;
                };
                let gear_id = ag.id.clone();
                let cache = gear_cache
                    .get(&gear_id)
                    .cloned()
                    .unwrap_or_else(|| R::make_cache(&gear_id));
                let input = match &mut ag.execution {
                    ActiveGearExecution::Local { source, .. } => match source {
                        GearSource::Events(group) => GearInput::Events(*group),
                        GearSource::Timer { period, next_due } => {
                            let tick = *next_due <= epoch;
                            if tick {
                                *next_due = epoch + period.get();
                            }
                            GearInput::Timer { tick }
                        }
                    },
                    ActiveGearExecution::Remote { .. } => {
                        unreachable!("run_loc_gear_task on a remote gear")
                    }
                };
                (gear_id, cache, input)
            };

            let mut ctx = GearCtx {
                core: Rc::clone(&self),
                gear: gear_id.clone(),
                deps: RefCell::new(HashSet::new()),
            };
            let output = R::run_step(&mut ctx, input, &mut cache).await;

            // Write output + cache; reconcile stale dep edges; collect fan-out.
            let (removed_deps, dependents, remote_subs, do_rerun) = {
                let mut inner = self.inner.borrow_mut();
                inner.gear_cache.insert(gear_id.clone(), cache);
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
                ag.changed.notify_all();
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

            // Push the new output to every remote subscriber.
            for target in &remote_subs {
                self.push_remote_update(&gear_id, &output, *target);
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
    fn force_active(self: &Rc<Self>, gear: &R::GearId) -> GearKey {
        let mut inner = self.inner.borrow_mut();
        // Existing gear: just promote it out of limbo if needed. See the doc:
        // nothing to kick or re-subscribe — it already has an output or a run /
        // StartSubscription in flight.
        if let Some(&key) = inner.gear_index.get(gear) {
            if let Some(pos) = inner.unref_gear.iter().position(|g| *g == key) {
                inner.unref_gear.remove(pos);
            }
            return key;
        }
        // New gear: route, insert, and start its first computation/subscription.
        let meta = R::meta(gear);
        let group_key = meta.group().clone();
        let target_core = R::route_group(&group_key, &inner.loc_ctx)
            .expect("force_active: route_group")
            .route(self.num_cores);
        // What kind of gear, and what (if anything) to register it in once
        // inserted. Event gears join `event_subscriptions`; timer (oracle)
        // gears join `timer_gears`. Remote gears register nothing here — their
        // owning core tracks them.
        enum Reg {
            Event(LocGroupId),
            Timer,
            Remote,
        }
        let (execution, registration) = if target_core == self.core_id {
            match meta {
                GearMeta::Event { msg_type, group } => {
                    let loc_group = inner.loc_ctx.mk_loc_group(msg_type, group);
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
            }
        } else {
            (ActiveGearExecution::Remote { target_core }, Reg::Remote)
        };
        let key = inner.gears.insert(ActiveGear {
            id: gear.clone(),
            output: None,
            execution,
            local_dependents: HashSet::new(),
            remote_subscribers: HashSet::new(),
            direct_subscriber_count: 0,
            changed: Event::new(),
        });
        inner.gear_index.insert(gear.clone(), key);
        // Register the local gear in its trigger index.
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
            Reg::Remote => {}
        }
        drop(inner);
        if target_core == self.core_id {
            self.kick_loc_gear(key);
        } else {
            self.send_start_subscription(gear, key, target_core);
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
    async fn wait_for_output_unpinned(self: &Rc<Self>, key: GearKey) -> Option<R::GearOut> {
        loop {
            let listener = {
                let inner = self.inner.borrow();
                let Some(ag) = inner.gears.get(key) else {
                    return None;
                };
                if let Some(out) = ag.output.clone() {
                    return Some(out);
                }
                ag.changed.listen()
            };
            listener.await;
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
    async fn secondary_get_impl(self: &Rc<Self>, caller: R::GearId, gear: R::GearId) -> R::GearOut {
        let gear_key = self.force_active(&gear);
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

    fn current_output_key(&self, key: GearKey) -> Option<R::GearOut> {
        let inner = self.inner.borrow();
        inner.gears.get(key).and_then(|ag| ag.output.clone())
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
        inner
            .gears
            .get_mut(key)
            .expect("inc_direct_subscriber: gear evicted immediately after force_active")
            .direct_subscriber_count += 1;
    }

    // --- cross-core subscription wiring ---

    fn build_gear_wire(&self, gear: &R::GearId) -> (R::GearId, Arc<WireLocCtx<R>>) {
        self.inner.borrow().build_gear_wire(gear)
    }

    fn send_start_subscription(&self, gear: &R::GearId, subscriber_key: GearKey, target: u32) {
        let (gear_wire, wire_ctx) = self.build_gear_wire(gear);
        let _ = self.intercore_tx[target as usize].send(InterCoreMsg::StartSubscription {
            gear: gear_wire,
            wire_ctx,
            from_core: self.core_id,
            session: subscriber_key,
        });
        self.doorbells[target as usize].ring();
    }

    fn push_remote_update(&self, gear: &R::GearId, output: &R::GearOut, target: u32) {
        let wire_ctx = {
            let inner = self.inner.borrow();
            let mut builder = WireLocCtxBuilder::new(&inner.loc_ctx);
            let gear_wire = builder
                .remap(gear.clone())
                .expect("push_remote_update: gear remap");
            let output_wire = builder
                .remap(output.clone())
                .expect("push_remote_update: output remap");
            let wire_ctx = Arc::new(builder.build());
            (gear_wire, output_wire, wire_ctx)
        };
        let (gear_wire, output_wire, wire_ctx) = wire_ctx;
        let _ = self.intercore_tx[target as usize].send(InterCoreMsg::SubscriptionUpdate {
            gear: gear_wire,
            output: output_wire,
            wire_ctx,
        });
        self.doorbells[target as usize].ring();
    }

    /// Rebalance a gear by its arena key. Any `StopSubscription` messages
    /// produced by cascade evictions are emitted directly by `CoreLocCtx` via
    /// the borrowed [`StopCtx`] (channels/doorbells/core id) — no `Vec` of
    /// pending stops is round-tripped through `Core`.
    fn rebalance_key(self: &Rc<Self>, key: GearKey) {
        let stop_ctx = StopCtx {
            intercore_tx: &self.intercore_tx,
            doorbells: &self.doorbells,
            core_id: self.core_id,
        };
        self.inner.borrow_mut().rebalance_gear(key, &stop_ctx);
    }

    /// Import events into this core, optionally forwarding to inter-node peers,
    /// then schedule reruns for any active gear whose inputs were touched.
    fn post_events(
        self: &Rc<Self>,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: &Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: &[u32],
        source_node: Option<NodeId>,
    ) -> Result<(), MergeError> {
        let node_id = self.node_id;
        // Merge imports under a full `&mut inner` borrow, then split-borrow to
        // kick: iterate `event_subscriptions` (shared) while mutating `gears`
        // (via `kick_loc_gear_in`) in one pass — no `Vec` of kick keys, no
        // re-borrow per kick. `event_subscriptions` and `gears` are disjoint
        // fields, so the shared iterator borrow and the mutable kick borrow
        // coexist.
        {
            let mut inner = self.inner.borrow_mut();
            let mut dirty: HashSet<LocGroupId> = HashSet::new();
            {
                let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
                for &idx in seed_indices {
                    let event = &events[idx as usize];
                    let gcid = global_core_ids[idx as usize];
                    let (group_id, store_result) = merger.import_new_event(
                        event.clone(),
                        gcid,
                        timestamp,
                        source_node.unwrap_or(node_id),
                    )?;
                    if store_result.is_some() {
                        dirty.insert(group_id);
                    }
                }
            }
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
            // Scope ALL inner borrows so the async state machine doesn't hold
            // them across the `.await` below.
            let mut inner = self.inner.borrow_mut();
            let mut merger = WireLocCtxMerger::new(wire_ctx, &mut *inner);
            merger.remap(gear).map_err(RunGearError::Merge)?
        };
        Ok(self.read_gear(gear).await)
    }

    /// Handle a `ClientOp` (received from a channel that is).
    pub(crate) fn handle_client_op(self: &Rc<Self>, op: CoreCmd<R>) {
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
                let result = self.post_events(
                    wire_ctx,
                    events,
                    &global_core_ids,
                    timestamp,
                    &seed_indices,
                    forwarded_from,
                );
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
                compio::runtime::spawn(async move {
                    let result = this.run_gear(gear, &wire_ctx).await;
                    let _ = reply.send(result);
                })
                .detach();
            }
        }
    }

    pub(crate) fn handle_intercore_msg(self: &Rc<Self>, msg: InterCoreMsg<R>) {
        match msg {
            InterCoreMsg::Op(op) => self.handle_client_op(op),
            InterCoreMsg::StartSubscription {
                gear,
                wire_ctx,
                from_core,
                session,
            } => {
                let this = Rc::clone(self);
                compio::runtime::spawn(async move {
                    let gear = {
                        let mut inner = this.inner.borrow_mut();
                        let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
                        merger
                            .remap(gear)
                            .expect("StartSubscription: failed to localize gear")
                    };
                    // Register the remote subscriber, ensure a run, await output,
                    // then push it back. The subscriber is attached before the
                    // await, so the gear can't be evicted mid-wait.
                    let key = this.force_active(&gear);
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
                    this.push_remote_update(&gear, &output, from_core);
                })
                .detach();
            }
            InterCoreMsg::SubscriptionUpdate {
                gear,
                output,
                wire_ctx,
            } => {
                let (gear, output) = {
                    let mut inner = self.inner.borrow_mut();
                    let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
                    let gear = merger
                        .remap(gear)
                        .expect("SubscriptionUpdate: failed to localize gear");
                    let output = merger
                        .remap(output)
                        .expect("SubscriptionUpdate: failed to localize output");
                    (gear, output)
                };
                let dependents = {
                    let mut inner = self.inner.borrow_mut();
                    let Some(key) = inner.gear_index.get(&gear).copied() else {
                        return;
                    };
                    let Some(ag) = inner.gears.get_mut(key) else {
                        return;
                    };
                    ag.output = Some(output);
                    ag.changed.notify_all();
                    ag.local_dependents.iter().copied().collect::<Vec<_>>()
                };
                for dep in dependents {
                    self.kick_loc_gear(dep);
                }
            }
            InterCoreMsg::StopSubscription { session, from_core } => {
                // No localization: route purely by the opaque session token.
                self.rebalance_remote_unsub(from_core, session);
            }
        }
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

    pub(crate) fn handle_inter_node_msg(self: &Rc<Self>, peer_idx: usize, msg: InterNodeMsg<R>) {
        let source_node = self.inter_node_peers[peer_idx].0;
        match msg {
            InterNodeMsg::ForwardEvents {
                wire_ctx,
                events,
                timestamp,
            } => self
                .db_post_events(wire_ctx, events, timestamp, (Some(source_node), || None))
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

    #[must_use]
    pub fn query_events<F>(
        &self,
        group: LocGroupId,
        since: (usize, usize),
        f: impl Fn(&[GroupEventId], &[GroupEventId]) -> F,
    ) -> Option<F> {
        self.inner.borrow().loc_ctx.query_events(group, since, f)
    }

    // Send commands to db via this Core

    /// Post events, routing each to the correct core.
    /// Self-targeting events call `Core::do_post_events` directly.
    /// Remote events go through SPSC `intercore_tx`.
    pub fn db_post_events(
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
                // Direct call: synchronous, no channel overhead
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
            )?;
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
    pub async fn subscribe_gear(self: &Rc<Self>, gear: R::GearId) -> Subscription<R> {
        let key = self.force_active(&gear);
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
    pub async fn subscribe_gear_stale(self: &Rc<Self>, gear: R::GearId) -> Subscription<R> {
        let key = self.force_active(&gear);
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
    pub async fn read_gear(self: &Rc<Self>, gear: R::GearId) -> R::GearOut {
        let sub = self.subscribe_gear(gear).await;
        let out = sub.current();
        drop(sub);
        out
    }

    /// Read a gear's current output once (stale — does not wait for in-flight
    /// runs). Implemented as a short-lived subscription.
    pub async fn read_gear_stale(self: &Rc<Self>, gear: R::GearId) -> R::GearOut {
        let sub = self.subscribe_gear_stale(gear).await;
        let out = sub.current();
        drop(sub);
        out
    }
}

/// RAII handle for a direct, worker-side subscription to a gear's output.
///
/// Dropping it decrements the gear's direct-subscriber count and rebalances
/// the gear (demoting it to limbo, or evicting it under pressure). Naturally
/// `!Sync` via `Rc<Core>`: it must live on the owning core's thread.
#[must_use]
pub struct Subscription<R: IsRuntime> {
    core: Rc<Core<R>>,
    /// The arena key, stored directly (not the `R::GearId`) so `current`/
    /// `next`/`Drop` skip the `gear_index` lookup. Safe because a live
    /// `Subscription` holds `direct_subscriber_count >= 1`, so `has_interest`
    /// is true and the gear cannot be evicted (its key cannot go stale) for as
    /// long as this handle exists.
    key: GearKey,
}

impl<R: IsRuntime> Subscription<R> {
    /// Read the gear's currently-cached output. The value is guaranteed to be
    /// present (subscribe awaits the first computation).
    #[must_use]
    pub fn current(&self) -> R::GearOut {
        let inner = self.core.inner.borrow();
        inner
            .gears
            .get(self.key)
            .and_then(|ag| ag.output.clone())
            .expect("Subscription::current: gear has no output")
    }

    /// Wait for the next output update (the gear's `changed` event fires after
    /// each completed run / `SubscriptionUpdate`) and return the new value, or
    /// `None` if the gear was evicted.
    pub async fn next(&self) -> Option<R::GearOut> {
        let listener = {
            let inner = self.core.inner.borrow();
            let Some(ag) = inner.gears.get(self.key) else {
                return None;
            };
            ag.changed.listen()
        };
        listener.await;
        let inner = self.core.inner.borrow();
        inner.gears.get(self.key).and_then(|ag| ag.output.clone())
    }
}

impl<R: IsRuntime> Drop for Subscription<R> {
    fn drop(&mut self) {
        let lost_interest = {
            let mut inner = self.core.inner.borrow_mut();
            let Some(ag) = inner.gears.get_mut(self.key) else {
                return;
            };
            ag.direct_subscriber_count = ag.direct_subscriber_count.saturating_sub(1);
            !ag.has_interest()
        };
        if lost_interest {
            self.core.rebalance_key(self.key);
        }
    }
}

/// The context handed to `IsRuntime::run_step`. Carries the gear's own id, a
/// handle to the live `Core` (via `Deref`, so `core.query_events()` /
/// `core.stored_event()` keep working unchanged), and the per-run `deps` set
/// accumulated by `secondary_get` calls (reconciled against the gear's stored
/// `dep_set` at run end).
pub struct GearCtx<R: IsRuntime> {
    pub(crate) core: Rc<Core<R>>,
    pub(crate) gear: R::GearId,
    /// Deps accumulated by `secondary_get`. Interior-mutable so `secondary_get`
    /// can be `&self` (lets a `dep_resolver` closure share `ctx` with sibling
    /// closures that read `ctx` immutably).
    pub(crate) deps: RefCell<HashSet<R::GearId>>,
}

impl<R: IsRuntime> std::ops::Deref for GearCtx<R> {
    type Target = Core<R>;
    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl<R: IsRuntime> GearCtx<R> {
    /// The id of the gear currently running.
    pub fn gear(&self) -> &R::GearId {
        &self.gear
    }

    /// The underlying `Core`.
    pub fn core(&self) -> &Rc<Core<R>> {
        &self.core
    }

    /// Declare a dependency on `dep`'s output and pull its current value
    /// (awaiting it if not yet computed). Records the edge `self.gear → dep`
    /// (both the forward `deps` entry here and the reverse
    /// `dep.local_dependents` entry in the core) so that when `dep` changes,
    /// this gear reruns.
    pub async fn secondary_get(&self, dep: R::GearId) -> R::GearOut {
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

impl<R: IsRuntime> CoreLocCtx<R> {
    /// Build a `(gear, WireLocCtx)` pair for an outgoing cross-core message.
    /// Lives on `CoreLocCtx` (not `Core`) so the rebalance/eviction path can
    /// build wires from `self.loc_ctx` while holding `&mut self`, without
    /// re-borrowing `inner`.
    fn build_gear_wire(&self, gear: &R::GearId) -> (R::GearId, Arc<WireLocCtx<R>>) {
        let mut builder = WireLocCtxBuilder::new(&self.loc_ctx);
        let gear_wire = builder
            .remap(gear.clone())
            .expect("build_gear_wire: gear remap");
        (gear_wire, Arc::new(builder.build()))
    }

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

impl<R: IsRuntime> GroupEventSource<R> for Core<R> {
    fn stored_event_in(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<R::Body>> {
        self.inner
            .borrow()
            .loc_ctx
            .get_stored_event(group, slot, std::clone::Clone::clone)
    }

    fn sender_user_in(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.inner.borrow().loc_ctx.sender_user(sid)
    }

    fn sender_pk_in(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.inner.borrow().loc_ctx.sender_pk(sid)
    }

    fn data_in(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.inner.borrow().loc_ctx.get_data(did, Clone::clone)
    }
}

impl<R: IsRuntime> EventContext<R> for CoreLocCtx<R> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId {
        self.loc_ctx.mk_loc_user(uid)
    }

    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        self.loc_ctx.mk_loc_sender(pk, uid)
    }

    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        self.loc_ctx.mk_loc_group(msg_type, group)
    }

    fn store_event(&mut self, ev: StoredEvent<R::Body>) -> Option<StoreResultSuccess> {
        // Bodies + dedup + added/removed changelog all live together inside
        // `loc_ctx`'s per-group shards now, so this is a plain delegate. The
        // changelog push that used to live here moved into `LocCtx::store_event`.
        self.loc_ctx.store_event(ev)
    }

    fn mk_data(&mut self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError> {
        self.loc_ctx.mk_data(data_id, content)
    }

    fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId> {
        self.loc_ctx.find_data_by_data_id(data_id)
    }
}

impl<R: IsRuntime> EventContext<R> for Core<R> {
    fn mk_loc_user(&mut self, uid: UserId) -> LocUserId {
        self.inner.get_mut().mk_loc_user(uid)
    }

    fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        self.inner.get_mut().mk_loc_sender(pk, uid)
    }

    fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        self.inner.get_mut().mk_loc_group(msg_type, group)
    }

    fn store_event(&mut self, event: StoredEvent<R::Body>) -> Option<StoreResultSuccess> {
        self.inner.get_mut().store_event(event)
    }

    fn mk_data(&mut self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError> {
        self.inner.get_mut().mk_data(data_id, content)
    }

    fn find_data_by_data_id(&self, data_id: &DataId) -> Option<LocDataId> {
        self.inner.borrow().find_data_by_data_id(data_id)
    }
}
