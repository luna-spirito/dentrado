use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{mpsc, Arc},
};

use crate::{
    core::{
        db,
        doorbell::DoorbellHandle,
        gear::Runtime,
        loc_ctx::{EventContext, LocCtx, StoreResultSuccess, StoredEvent},
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
pub(crate) enum CoreCmd<R: Runtime> {
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
pub(crate) enum CoordCmd<R: Runtime> {
    Op(CoreCmd<R>),
    Shutdown,
}

/// Inter-node singnals.
#[derive(Debug)]
pub(crate) enum InterNodeMsg<R: Runtime> {
    ForwardEvents {
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    },
}

#[derive(Debug)]
pub(crate) enum RerouteMsg<R: Runtime> {
    ForwardToPeer {
        peer_idx: usize,
        wire_ctx: WireLocCtx<R>,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    },
}

#[derive(Debug)]
pub(crate) enum InterCoreMsg<R: Runtime> {
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
struct CoreInner<R: Runtime> {
    gear_cache: HashMap<R::GearId, Box<dyn Any>>,
    gear_in_flight: HashSet<R::GearId>,
    secondary_cache: HashMap<R::GearId, R::GearOut>,
    events_by_group: HashMap<LocGroupId, EventGroup>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct EventGroup {
    pub(crate) added: Vec<AnyLocEventId>,
    pub(crate) removed: Vec<AnyLocEventId>,
}

#[derive(Debug)]
pub struct Core<R: Runtime> {
    num_cores: u32,
    core_id: u32,
    node_id: NodeId,
    module: Arc<R::Module>,

    intercore_tx: Vec<mpsc::Sender<InterCoreMsg<R>>>,
    reroute_tx: Vec<mpsc::Sender<RerouteMsg<R>>>,
    /// One doorbell handle per core (including self). Ring after sending.
    doorbells: Vec<DoorbellHandle>,
    inter_node_peers: Vec<(
        NodeId,
        u32,
        Option<(mpsc::Sender<InterNodeMsg<R>>, DoorbellHandle)>,
    )>,

    loc_ctx: LocCtx<R>,
    inner: RefCell<CoreInner<R>>,
}

impl<R: Runtime> Core<R> {
    pub(crate) fn new(
        num_cores: u32,
        core_id: u32,
        node_id: NodeId,
        module: Arc<R::Module>,
        intercore_tx: Vec<mpsc::Sender<InterCoreMsg<R>>>,
        reroute_tx: Vec<mpsc::Sender<RerouteMsg<R>>>,
        doorbells: Vec<DoorbellHandle>,
        inter_node_peers: Vec<(
            NodeId,
            u32,
            Option<(mpsc::Sender<InterNodeMsg<R>>, DoorbellHandle)>,
        )>,
    ) -> Self {
        Self {
            num_cores,
            core_id,
            node_id,
            loc_ctx: LocCtx::new(),
            module,
            intercore_tx,
            reroute_tx,
            doorbells,
            inter_node_peers,
            inner: RefCell::new(CoreInner {
                gear_cache: HashMap::new(),
                gear_in_flight: HashSet::new(),
                secondary_cache: HashMap::new(),
                events_by_group: HashMap::new(),
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
    pub(crate) fn num_cores(&self) -> u32 {
        self.num_cores
    }

    pub(crate) fn run_any_gear(
        &self,
        gear: R::GearId,
        msg_type: LocMsgTypeId,
        group: &R::Group,
    ) -> R::GearOut {
        let group = self.loc_ctx().find_group(msg_type, group);
        eprintln!(
            "N{}C{}: run_any_gear msg_type={msg_type:?} find_group={group:?}",
            self.node_id.0, self.core_id
        );

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

        let output = R::run_step(&key, self, group, &mut *cache);

        {
            let mut inner = self.inner.borrow_mut();
            inner.gear_in_flight.remove(&key);
            inner.gear_cache.insert(key.clone(), cache);
        }

        output
    }

    pub(crate) fn secondary_get(&self, gear: R::GearId) -> R::GearOut {
        let (msg_type, group) = R::meta(&gear);
        let builder = WireLocCtxBuilder::new(&self.loc_ctx);
        let group_wire = builder
            .remap(group.clone())
            .expect("secondary_get: group remap");
        let wire_ctx = builder.build();
        let target_core = R::route_group(&group_wire, &wire_ctx)
            .expect("secondary_get: route_group")
            .route(self.num_cores);

        if target_core == self.core_id {
            self.run_any_gear(gear.clone(), msg_type, &group)
        } else {
            let cached = self.inner.borrow().secondary_cache.get(&gear).cloned();
            let output =
                cached.unwrap_or_else(|| self.run_any_gear(gear.clone(), msg_type, &group));

            let req_builder = WireLocCtxBuilder::new(&self.loc_ctx);
            let gear_wire = req_builder.remap(gear).expect("secondary_get: gear remap");
            let req_wire_ctx = Arc::new(req_builder.build());
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
        let merger = WireLocCtxMerger::new(&wire_ctx, self);
        for &idx in seed_indices {
            let event = &events[idx as usize];
            let gcid = global_core_ids[idx as usize];
            merger.import_new_event(event, gcid, timestamp, source_node.unwrap_or(self.node_id))?;
        }
        if source_node.is_none() {
            // TODO: Don't pass wire_ctx, pass only the relevant subpart of it. I. e. update WireLocCtxMereger to regenerate
            let events = seed_indices
                .into_iter()
                .map(|&idx| events[idx as usize].clone())
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
        let merger = WireLocCtxMerger::new(wire_ctx, self);
        let gear = merger.remap(gear).map_err(RunGearError::Merge)?;
        let (msg_type, localized_group) = R::meta(&gear);
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
                let merger = WireLocCtxMerger::new(&wire_ctx, self);
                let gear = merger
                    .remap(gear)
                    .expect("SecondaryRequest: failed to localize gear");

                let (msg_type, group) = R::meta(&gear);
                let output = self.run_any_gear(gear.clone(), msg_type, &group);

                let builder = WireLocCtxBuilder::new(self.loc_ctx());
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
                let merger = WireLocCtxMerger::new(&wire_ctx, self);
                let gear = merger
                    .remap(gear)
                    .expect("SecondaryResponse: failed to localize gear");
                let output = merger
                    .remap(output)
                    .expect("SecondaryResponse: failed to localize output");

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
        global_core_ids: &Arc<[GlobalCoreId]>,
        timestamp: u32,
    ) {
        for (peer_idx, (_node_id, remote_num_cores, sender_opt)) in
            self.inter_node_peers.iter().enumerate()
        {
            eprintln!(
                "N{}C{}: We want to send to peer {peer_idx}",
                self.node_id.0, self.core_id
            );
            eprintln!(
                "Remote has {remote_num_cores} cores, sender_opt.is_some: {}",
                sender_opt.is_some()
            );
            if let Some((sender, doorbell)) = sender_opt {
                eprintln!("... and we do it directly.");
                let _ = sender.send(InterNodeMsg::ForwardEvents {
                    wire_ctx: (*wire_ctx).clone(),
                    events: events.clone(),
                    timestamp,
                });
                doorbell.ring();
            } else {
                eprintln!("... but we don't have the connection, so we reroute.");
                let mut proxy_groups: HashMap<u32, Vec<u32>> = HashMap::new();
                for (i, gcid) in global_core_ids.iter().enumerate() {
                    let proxy_core = gcid.route(*remote_num_cores);
                    proxy_groups.entry(proxy_core).or_default().push(i as u32);
                }

                for (proxy_core, seed_indices) in proxy_groups {
                    eprintln!(
                        "Since target has {remote_num_cores} cores, we send via our {proxy_core}"
                    );
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

impl<R: Runtime> EventContext<R> for Core<R> {
    fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        self.loc_ctx.mk_loc_user(uid)
    }

    fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        self.loc_ctx.mk_loc_sender(pk, uid)
    }

    fn mk_loc_group(&self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        self.loc_ctx.mk_loc_group(msg_type, group)
    }

    fn store_event(&self, ev: StoredEvent<R::Body>) -> Option<StoreResultSuccess> {
        let group_id = ev.group;
        let core_id = self.core_id;
        let num_added;

        let res = self.loc_ctx.store_event(ev);
        if let Some(StoreResultSuccess { old, new }) = res {
            let mut s = self.inner.borrow_mut();
            let group = s.events_by_group.entry(group_id).or_default();
            group.added.push(new);
            num_added = group.added.len();
            if let Some(old) = old {
                group.removed.push(old);
            }
        } else {
            num_added = 0;
        }
        eprintln!(
            "N{}C{}: store_event group={group_id:?} added_now={num_added}",
            self.node_id.0, core_id
        );
        res
    }

    fn loc_ctx(&self) -> &LocCtx<R> {
        &self.loc_ctx
    }

    fn mk_data(&self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError> {
        self.loc_ctx.mk_data(data_id, content)
    }
}
