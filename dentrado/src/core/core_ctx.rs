use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    hash::Hash,
    num::NonZero,
    rc::Rc,
    sync::{Arc, mpsc},
};

use synchrony::unsync::watch;

use crate::{
    core::{
        db,
        doorbell::DoorbellHandle,
        gear::IsRuntime,
        loc_ctx::{EventContext, EventStore, LocCtx, StoreResultSuccess, StoredEvent},
    },
    types::{
        AnyLocEventId, DataId, DataVerifyError, GlobalCoreId, LocDataId, LocGroupId, LocMsgTypeId,
        LocSenderId, LocUserId, NodeId, SenderPk, UserId,
    },
    wire::{
        MergeError, RunGearError, WireEventBody, WireLocCtx, WireLocCtxBuilder, WireLocCtxMerger,
    },
};

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

#[derive(Debug)]
pub(crate) enum InterCoreMsg<R: IsRuntime> {
    Op(CoreCmd<R>),
    SecondaryRequest {
        gear: R::GearId,
        wire_ctx: Arc<WireLocCtx<R>>,
        from_core: u32,
    },
    SecondaryResponse {
        gear: R::GearId,
        output: R::GearOut,
        wire_ctx: Arc<WireLocCtx<R>>,
    },
}

#[derive(Debug)]
struct CoreLocCtx<R: IsRuntime> {
    gear_cache: HashMap<R::GearId, R::GearCache>,
    gear_in_flight: HashSet<R::GearId>,
    secondary_cache: HashMap<R::GearId, R::GearOut>,
    events_by_group: HashMap<LocGroupId, EventGroup>,
    loc_ctx: LocCtx<R>,
    // --- subscription state ---
    /// Gears with active external interest (dependents / remote cores /
    /// direct subscribers). Entry present ⟺ `has_interest()`.
    gear_subscriptions: HashMap<R::GearId, GearSub<R>>,
    /// Limbo: gears with no current interest, kept hot until LRU eviction.
    unref_gear: LruCache<R::GearId, GearSub<R>>,
    /// Reverse index: which gears care about a given event input.
    event_subscriptions: HashMap<LocGroupId, HashSet<R::GearId>>,
    /// Forward index: a gear's event inputs (for O(deps) cleanup on evict).
    event_deps: HashMap<R::GearId, HashSet<LocGroupId>>,
    /// `SubId` → gear owning that direct subscription (for O(1) teardown).
    subscriptions_by_id: HashMap<SubId, R::GearId>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EventGroup {
    pub(crate) added: Vec<AnyLocEventId>,
    pub(crate) removed: Vec<AnyLocEventId>,
}

/// Soft cap on the number of gears kept hot in limbo. Beyond this, the
/// least-recently-demoted gear is evicted (its subscription is torn down and
/// its dependencies may cascade-evict).
const LIMBO_CAPACITY: usize = 64;

/// Identifier for a single direct (worker-side) subscription. Unique per core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SubId(u64);

/// Per-gear subscription state. Lives in `gear_subscriptions` while the gear has
/// any external interest (dependents, remote cores, or direct subscribers), and
/// is moved to the `unref_gear` limbo when interest drops to zero. While in limbo
/// the gear **keeps** running on events (stays hot) — only LRU eviction tears
/// its subscription down (see `evict_gear`).
#[derive(Debug)]
pub(crate) struct GearSub<R: IsRuntime> {
    pub(crate) output: R::GearOut,
    /// Gears this one depends on (discovered via `secondary_get`).
    pub(crate) dep_set: HashSet<R::GearId>,
    /// Gears that depend on this one (forward gear-dep index).
    pub(crate) local_dependents: HashSet<R::GearId>,
    /// Remote cores subscribed to this gear's output.
    pub(crate) remote_subscribers: HashSet<u32>,
    /// Direct worker-side subscribers, keyed by `SubId`.
    pub(crate) direct_subscribers: HashMap<SubId, watch::Sender<R::GearOut>>,
}

impl<R: IsRuntime> GearSub<R> {
    /// Whether anything still cares about this gear's output.
    fn has_interest(&self) -> bool {
        !self.local_dependents.is_empty()
            || !self.remote_subscribers.is_empty()
            || !self.direct_subscribers.is_empty()
    }
}

/// Minimal LRU: insertion order = recency (front = oldest). Used for the limbo
/// cache; capacity is enforced by the caller draining `pop_lru` after insert.
#[derive(Debug)]
struct LruCache<K: Hash + Eq + Clone, V> {
    order: VecDeque<K>,
    entries: HashMap<K, V>,
    capacity: usize,
}

impl<K: Hash + Eq + Clone, V> LruCache<K, V> {
    fn new(capacity: usize) -> Self {
        Self {
            order: VecDeque::new(),
            entries: HashMap::new(),
            capacity,
        }
    }

    fn capacity(&self) -> usize {
        self.capacity
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn insert(&mut self, key: K, value: V) {
        if self.entries.contains_key(&key) {
            self.order.retain(|k| k != &key);
        }
        self.entries.insert(key.clone(), value);
        self.order.push_back(key);
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        let v = self.entries.remove(key)?;
        self.order.retain(|k| k != key);
        Some(v)
    }

    /// Pop the least-recently-inserted entry.
    fn pop_lru(&mut self) -> Option<(K, V)> {
        loop {
            let key = self.order.pop_front()?;
            if let Some(v) = self.entries.remove(&key) {
                return Some((key, v));
            }
        }
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
                gear_in_flight: HashSet::new(),
                secondary_cache: HashMap::new(),
                events_by_group: HashMap::new(),
                loc_ctx: LocCtx::new(),
                gear_subscriptions: HashMap::new(),
                unref_gear: LruCache::new(LIMBO_CAPACITY),
                event_subscriptions: HashMap::new(),
                event_deps: HashMap::new(),
                subscriptions_by_id: HashMap::new(),
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
        eid: AnyLocEventId,
        f: impl Fn(&StoredEvent<R::Body>) -> F,
    ) -> Option<F> {
        self.inner.borrow().loc_ctx.get_stored_event(eid, f)
    }

    pub(crate) fn run_any_gear(
        &self,
        gear: R::GearId,
        msg_type: LocMsgTypeId,
        group: &R::Group,
    ) -> R::GearOut {
        let group = self.inner.borrow().loc_ctx.find_group(msg_type, group);

        {
            let mut inner = self.inner.borrow_mut();
            assert!(
                !inner.gear_in_flight.contains(&gear),
                "run_any_gear: gear is already in-flight (re-entrant execution)",
            );
            inner.gear_in_flight.insert(gear.clone());
        }

        let (key, mut cache) = {
            let mut inner = self.inner.borrow_mut();
            if let Some(entry) = inner.gear_cache.remove_entry(&gear) {
                entry
            } else {
                let cache = R::make_cache(&gear);
                (gear, cache)
            }
        };

        let output = R::run_step(&key, self, group, &mut cache);

        {
            let mut inner = self.inner.borrow_mut();
            inner.gear_in_flight.remove(&key);
            inner.gear_cache.insert(key.clone(), cache);
        }

        output
    }

    pub fn secondary_get(&self, gear: R::GearId) -> R::GearOut {
        let (msg_type, group) = R::meta(&gear);
        let (group_wire, wire_ctx) = {
            let inner = self.inner.borrow_mut();
            let mut builder = WireLocCtxBuilder::new(&inner.loc_ctx);
            let group_wire = builder
                .remap(group.clone())
                .expect("secondary_get: group remap");
            (group_wire, builder.build())
        };
        println!(
            "SECONDARY REQUEST TO {:?}, WHICH IS {:?}",
            group_wire,
            R::route_group(&group_wire, &wire_ctx).unwrap()
        );
        let target_core = R::route_group(&group_wire, &wire_ctx)
            .expect("secondary_get: route_group")
            .route(self.num_cores);

        if target_core == self.core_id {
            self.run_any_gear(gear.clone(), msg_type, &group)
        } else {
            // Snapshot any cached output first, releasing the borrow before
            // `run_any_gear` (which borrows `inner` itself).
            let cached = self.inner.borrow().secondary_cache.get(&gear).cloned();
            let output =
                cached.unwrap_or_else(|| self.run_any_gear(gear.clone(), msg_type, &group));

            let (gear_wire, req_wire_ctx) = {
                let inner = self.inner.borrow();
                let mut req_builder = WireLocCtxBuilder::new(&inner.loc_ctx);
                let gear_wire = req_builder.remap(gear).expect("secondary_get: gear remap");
                (gear_wire, Arc::new(req_builder.build()))
            };
            let _ = self.intercore_tx[target_core as usize].send(InterCoreMsg::SecondaryRequest {
                gear: gear_wire,
                wire_ctx: req_wire_ctx,
                from_core: self.core_id,
            });
            self.doorbells[target_core as usize].ring();

            output
        }
    }

    /// Handle a `PostEvents` operation directly.
    /// Import events into this core, optionally forwarding to inter-node peers.
    fn post_events(
        &self,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: &Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: &[u32],
        source_node: Option<NodeId>,
    ) -> Result<(), MergeError> {
        let node_id = self.node_id;
        {
            let mut inner = self.inner.borrow_mut();
            let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
            for &idx in seed_indices {
                let event = &events[idx as usize];
                let gcid = global_core_ids[idx as usize];
                merger.import_new_event(
                    event.clone(),
                    gcid,
                    timestamp,
                    source_node.unwrap_or(node_id),
                )?;
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

    /// Handle a `RunGear` operation directly.
    pub(crate) fn run_gear(
        &self,
        gear: R::GearId,
        wire_ctx: &WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        // Scope the merger's `borrow_mut` so it is released before `run_any_gear`
        // (which borrows `inner` again) — otherwise RefCell panics.
        let (gear, msg_type, localized_group) = {
            let mut inner = self.inner.borrow_mut();
            let mut merger = WireLocCtxMerger::new(wire_ctx, &mut *inner);
            let gear = merger.remap(gear).map_err(RunGearError::Merge)?;
            let (msg_type, localized_group) = R::meta(&gear);
            (gear, msg_type, localized_group)
        };
        Ok(self.run_any_gear(gear, msg_type, &localized_group))
    }

    /// Handle a `ClientOp` (received from a channel that is).
    pub(crate) fn handle_client_op(&self, op: CoreCmd<R>) {
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
                let result = self.run_gear(gear, &wire_ctx);
                reply.send(result).expect("RunGear: reply channel closed");
            }
        }
    }

    pub(crate) fn handle_intercore_msg(&self, msg: InterCoreMsg<R>) {
        match msg {
            InterCoreMsg::Op(op) => self.handle_client_op(op),
            InterCoreMsg::SecondaryRequest {
                gear,
                wire_ctx,
                from_core,
            } => {
                let gear = {
                    let mut inner = self.inner.borrow_mut();
                    let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
                    merger
                        .remap(gear)
                        .expect("SecondaryRequest: failed to localize gear")
                };

                let (msg_type, group) = R::meta(&gear);
                let output = self.run_any_gear(gear.clone(), msg_type, &group);

                let inner = self.inner.borrow();
                let mut builder = WireLocCtxBuilder::new(&inner.loc_ctx);
                let gear_wire = builder
                    .remap(gear)
                    .expect("SecondaryRequest: failed to remap gear");
                let output_wire = builder
                    .remap(output)
                    .expect("SecondaryRequest: failed to remap output");
                let reply_wire_ctx = Arc::new(builder.build());

                let _ =
                    self.intercore_tx[from_core as usize].send(InterCoreMsg::SecondaryResponse {
                        gear: gear_wire,
                        output: output_wire,
                        wire_ctx: reply_wire_ctx,
                    });
                self.doorbells[from_core as usize].ring();
            }
            InterCoreMsg::SecondaryResponse {
                gear,
                output,
                wire_ctx,
            } => {
                let (gear, output) = {
                    let mut inner = self.inner.borrow_mut();
                    let mut merger = WireLocCtxMerger::new(&wire_ctx, &mut *inner);
                    let gear = merger
                        .remap(gear)
                        .expect("SecondaryResponse: failed to localize gear");
                    let output = merger
                        .remap(output)
                        .expect("SecondaryResponse: failed to localize output");
                    (gear, output)
                };
                self.inner.borrow_mut().secondary_cache.insert(gear, output);
            }
        }
    }

    pub(crate) fn handle_inter_node_msg(&self, peer_idx: usize, msg: InterNodeMsg<R>) {
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
        f: impl Fn(&[AnyLocEventId], &[AnyLocEventId]) -> F,
    ) -> Option<F> {
        self.inner
            .borrow()
            .events_by_group
            .get(&group)
            .map(|eg| f(&eg.added[since.0..], &eg.removed[since.1..]))
    }

    // Send commands to db via this Core

    /// Post events, routing each to the correct core.
    /// Self-targeting events call `Core::do_post_events` directly.
    /// Remote events go through SPSC `intercore_tx`.
    pub fn db_post_events(
        &self,
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
        &self,
        gear: R::GearId,
        wire_ctx: WireLocCtx<R>,
    ) -> Result<R::GearOut, RunGearError> {
        let target_core = db::route_gear(&gear, &wire_ctx, self.num_cores())?;

        if target_core == self.core_id {
            // Direct call: synchronous, no channel overhead
            self.run_gear(gear, &wire_ctx)
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
}

/// RAII handle for a direct, worker-side subscription to a gear's output.
///
/// Dropping it removes the subscription from its core and rebalances the gear
/// (demoting it to limbo, or evicting it under LRU pressure). Naturally `!Sync`
/// via `Rc<Core>`: it must live on the owning core's thread.
///
/// Value access (`next`/borrow) lands in Phase 2 alongside `subscribe_gear`.
#[must_use]
pub struct Subscription<R: IsRuntime> {
    core: Rc<Core<R>>,
    sub_id: SubId,
}

impl<R: IsRuntime> Drop for Subscription<R> {
    fn drop(&mut self) {
        self.core.inner.borrow_mut().drop_direct_sub(self.sub_id);
    }
}

impl<R: IsRuntime> CoreLocCtx<R> {
    /// Remove a direct subscriber; if its gear loses all interest, demote it to
    /// limbo (and possibly LRU-evict, cascading to dependencies).
    fn drop_direct_sub(&mut self, sub_id: SubId) {
        let Some(gear) = self.subscriptions_by_id.remove(&sub_id) else {
            return;
        };
        let still_active = self.gear_subscriptions.get_mut(&gear).is_some_and(|g| {
            g.direct_subscribers.remove(&sub_id);
            g.has_interest()
        });
        if !still_active {
            self.rebalance_gear(&gear);
        }
    }

    /// If `gear` has no external interest, move it from the active set to limbo.
    /// LRU over-capacity then evicts the least-recently-demoted entry, cascading
    /// to its dependencies.
    fn rebalance_gear(&mut self, gear: &R::GearId) {
        let has_interest = self
            .gear_subscriptions
            .get(gear)
            .is_some_and(GearSub::has_interest);
        if has_interest {
            return;
        }
        let Some(gsub) = self.gear_subscriptions.remove(gear) else {
            // Already in limbo or fully absent: nothing to demote.
            return;
        };
        self.unref_gear.insert(gear.clone(), gsub);
        while self.unref_gear.len() >= self.unref_gear.capacity() {
            let Some((evicted_id, evicted_sub)) = self.unref_gear.pop_lru() else {
                break;
            };
            self.evict_gear(evicted_id, evicted_sub);
        }
    }

    /// Fully tear down a gear: drop its event-input edges (reverse + forward
    /// index), remove ourselves from each dependency's `local_dependents`, and
    /// cascade-rebalance dependencies that lose their last dependent. The
    /// dependency graph is acyclic by construction, so this terminates.
    ///
    /// Remote (`remote_subscribers`) teardown lands in Phase 4.
    fn evict_gear(&mut self, gear: R::GearId, gsub: GearSub<R>) {
        // 1. Event-input edges.
        if let Some(keys) = self.event_deps.remove(&gear) {
            for key in keys {
                if let Some(set) = self.event_subscriptions.get_mut(&key) {
                    set.remove(&gear);
                    if set.is_empty() {
                        self.event_subscriptions.remove(&key);
                    }
                }
            }
        }
        // 2. Gear-dep edges: drop ourselves from each dependency, then rebalance
        //    any dependency that loses its last dependent. `gsub` is owned and
        //    disjoint from `self`, so we can mutate `self` mid-iteration.
        for dep in &gsub.dep_set {
            let dep_lost_interest = self.gear_subscriptions.get_mut(dep).is_some_and(|dg| {
                dg.local_dependents.remove(&gear);
                !dg.has_interest()
            });
            if dep_lost_interest {
                self.rebalance_gear(dep);
            }
        }
        // 3. Drop cached computation state for this gear.
        self.gear_cache.remove(&gear);
    }
}

impl<R: IsRuntime> EventStore<R> for Core<R> {
    fn stored_event(&self, eid: AnyLocEventId) -> Option<StoredEvent<R::Body>> {
        self.inner
            .borrow()
            .loc_ctx
            .get_stored_event(eid, std::clone::Clone::clone)
    }

    fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.inner.borrow().loc_ctx.sender_user(sid)
    }

    fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.inner.borrow().loc_ctx.sender_pk(sid)
    }

    fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
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
        let group_id = ev.group;

        let res = self.loc_ctx.store_event(ev);
        if let Some(StoreResultSuccess { old, new }) = res {
            let group = self.events_by_group.entry(group_id).or_default();
            group.added.push(new);
            if let Some(old) = old {
                group.removed.push(old);
            }
        }
        res
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
