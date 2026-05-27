use std::{
    any::Any,
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{mpsc, Arc},
};

use crate::{
    core::{
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

pub(crate) enum CoreCmd<R: Runtime> {
    PostEvents {
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
        seed_indices: Vec<u32>,
        reply: mpsc::Sender<Result<(), MergeError>>,
    },
    RunGear {
        gear: R::GearId,
        wire_ctx: Arc<WireLocCtx<R>>,
        reply: mpsc::Sender<Result<R::GearOut, RunGearError>>,
    },
    Shutdown,
}

pub(crate) enum RerouteMsg<R: Runtime> {
    ForwardToPeer {
        peer_idx: usize,
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
    },
}

pub(crate) enum NodeMsg<R: Runtime> {
    ForwardEvents {
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
    },
}

pub(crate) enum InterCoreMsg<R: Runtime> {
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
    NodeForwardedEvents {
        wire_ctx: Arc<WireLocCtx<R>>,
        events: Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: Arc<[GlobalCoreId]>,
        timestamp: u32,
        source_node: NodeId,
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

    intercore_senders: Vec<mpsc::Sender<InterCoreMsg<R>>>,
    reroute_senders: Vec<mpsc::Sender<RerouteMsg<R>>>,
    inter_node_peers: Vec<(NodeId, u32, Option<mpsc::Sender<NodeMsg<R>>>)>,

    loc_ctx: LocCtx<R>,
    inner: RefCell<CoreInner<R>>,
}

impl<R: Runtime> Core<R> {
    pub(crate) fn new(
        num_cores: u32,
        core_id: u32,
        node_id: NodeId,
        module: Arc<R::Module>,
        intercore_senders: Vec<mpsc::Sender<InterCoreMsg<R>>>,
        reroute_senders: Vec<mpsc::Sender<RerouteMsg<R>>>,
        inter_node_peers: Vec<(NodeId, u32, Option<mpsc::Sender<NodeMsg<R>>>)>,
    ) -> Self {
        Self {
            num_cores,
            core_id,
            node_id,
            loc_ctx: LocCtx::new(),
            module,
            intercore_senders,
            reroute_senders,
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
            let _ =
                self.intercore_senders[target_core as usize].send(InterCoreMsg::SecondaryRequest {
                    gear: gear_wire,
                    wire_ctx: req_wire_ctx,
                    from_core: self.core_id,
                });

            output
        }
    }

    pub(crate) fn handle_cmd(&mut self, cmd: CoreCmd<R>) -> bool {
        match cmd {
            CoreCmd::PostEvents {
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
                seed_indices,
                reply,
            } => {
                let node_id = self.node_id;
                let result = (|| -> Result<(), MergeError> {
                    let merger = WireLocCtxMerger::new(&wire_ctx, self);
                    for &idx in &seed_indices {
                        let event = &events[idx as usize];
                        let gcid = global_core_ids[idx as usize];
                        merger.import_new_event(event, gcid, timestamp, node_id)?;
                    }
                    Ok(())
                })();

                self.forward_to_peers(&wire_ctx, &events, &global_core_ids, timestamp);

                reply
                    .send(result)
                    .expect("PostEvents: reply channel closed");
                false
            }
            CoreCmd::RunGear {
                gear,
                wire_ctx,
                reply,
            } => {
                let gear = {
                    let merger = WireLocCtxMerger::new(&wire_ctx, self);
                    match merger.remap(gear) {
                        Ok(g) => g,
                        Err(e) => {
                            reply
                                .send(Err(RunGearError::Merge(e)))
                                .expect("RunGear: reply channel closed");
                            return false;
                        }
                    }
                };

                let (msg_type, localized_group) = R::meta(&gear);

                let output = self.run_any_gear(gear.clone(), msg_type, &localized_group);
                eprintln!("CORE {}: run_gear output = {:?}", self.core_id, output);
                reply
                    .send(Ok(output))
                    .expect("RunGear: reply channel closed");

                false
            }
            CoreCmd::Shutdown => true,
        }
    }

    pub(crate) fn handle_intercore_msg(&mut self, msg: InterCoreMsg<R>) {
        eprintln!("CORE {}: received intercore msg", self.core_id);
        match msg {
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

                let _ = self.intercore_senders[from_core as usize].send(
                    InterCoreMsg::SecondaryResponse {
                        gear: gear_wire,
                        output: output_wire,
                        wire_ctx: reply_wire_ctx,
                    },
                );
            }
            InterCoreMsg::SecondaryResponse {
                gear,
                output,
                wire_ctx,
            } => {
                eprintln!(
                    "CORE {}: SecondaryResponse received, inserting into secondary cache",
                    self.core_id
                );
                let merger = WireLocCtxMerger::new(&wire_ctx, self);
                let gear = merger
                    .remap(gear)
                    .expect("SecondaryResponse: failed to localize gear");
                let output = merger
                    .remap(output)
                    .expect("SecondaryResponse: failed to localize output");

                self.inner.borrow_mut().secondary_cache.insert(gear, output);
            }
            InterCoreMsg::NodeForwardedEvents {
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
                source_node,
            } => {
                let core_id = self.core_id;
                let merger = WireLocCtxMerger::new(&wire_ctx, self);
                for (i, event) in events.iter().enumerate() {
                    let gcid = global_core_ids[i];
                    if let Err(e) = merger.import_new_event(event, gcid, timestamp, source_node) {
                        eprintln!("CORE {core_id}: NodeForwardedEvents import error: {e:?}");
                    }
                }
            }
        }
    }

    pub(crate) fn handle_inter_node_msg(&mut self, peer_idx: usize, msg: NodeMsg<R>) {
        match msg {
            NodeMsg::ForwardEvents {
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
            } => {
                let source_node = self.inter_node_peers[peer_idx].0;
                let num_cores = self.num_cores;
                let core_id = self.core_id;

                let mut local_indices: Vec<u32> = Vec::new();
                let mut forwarded: HashMap<u32, Vec<u32>> = HashMap::new();

                for (i, gcid) in global_core_ids.iter().enumerate() {
                    let target_core = gcid.route(num_cores);
                    if target_core == core_id {
                        local_indices.push(i as u32);
                    } else {
                        forwarded.entry(target_core).or_default().push(i as u32);
                    }
                }

                let merger = WireLocCtxMerger::new(&wire_ctx, self);
                for &idx in &local_indices {
                    if let Err(e) = merger.import_new_event(
                        &events[idx as usize],
                        global_core_ids[idx as usize],
                        timestamp,
                        source_node,
                    ) {
                        eprintln!("CORE {core_id}: inter-node ForwardEvents import error: {e:?}");
                    }
                }

                for (target_core, indices) in forwarded {
                    let fw_events: Arc<[WireEventBody<_, _>]> = indices
                        .iter()
                        .map(|&idx| events[idx as usize].clone())
                        .collect();
                    let fw_gcids: Arc<[GlobalCoreId]> = indices
                        .iter()
                        .map(|&idx| global_core_ids[idx as usize])
                        .collect();

                    let _ = self.intercore_senders[target_core as usize].send(
                        InterCoreMsg::NodeForwardedEvents {
                            wire_ctx: Arc::clone(&wire_ctx),
                            events: fw_events,
                            global_core_ids: fw_gcids,
                            timestamp,
                            source_node,
                        },
                    );
                }
            }
        }
    }

    pub(crate) fn handle_reroute_msg(&mut self, msg: RerouteMsg<R>) {
        match msg {
            RerouteMsg::ForwardToPeer {
                peer_idx,
                wire_ctx,
                events,
                global_core_ids,
                timestamp,
            } => {
                let sender = self
                    .inter_node_peers
                    .get(peer_idx)
                    .and_then(|(_, _, s)| s.as_ref())
                    .expect("handle_reroute_msg: no channel to peer");
                let _ = sender.send(NodeMsg::ForwardEvents {
                    wire_ctx,
                    events,
                    global_core_ids,
                    timestamp,
                });
            }
        }
    }

    fn forward_to_peers(
        &self,
        wire_ctx: &Arc<WireLocCtx<R>>,
        events: &Arc<[WireEventBody<R::Group, R::Body>]>,
        global_core_ids: &Arc<[GlobalCoreId]>,
        timestamp: u32,
    ) {
        for (peer_idx, (_node_id, remote_num_cores, sender_opt)) in
            self.inter_node_peers.iter().enumerate()
        {
            if let Some(sender) = sender_opt {
                let _ = sender.send(NodeMsg::ForwardEvents {
                    wire_ctx: Arc::clone(wire_ctx),
                    events: Arc::clone(events),
                    global_core_ids: Arc::clone(global_core_ids),
                    timestamp,
                });
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
                    let proxy_gcids: Vec<_> = seed_indices
                        .iter()
                        .map(|&idx| global_core_ids[idx as usize])
                        .collect();

                    let _ =
                        self.reroute_senders[proxy_core as usize].send(RerouteMsg::ForwardToPeer {
                            peer_idx,
                            wire_ctx: Arc::clone(wire_ctx),
                            events: Arc::from(proxy_events),
                            global_core_ids: Arc::from(proxy_gcids),
                            timestamp,
                        });
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

        let res = self.loc_ctx.store_event(ev);
        if let Some(StoreResultSuccess { old, new }) = res {
            let mut s = self.inner.borrow_mut();
            let group = s.events_by_group.entry(group_id).or_default();
            group.added.push(new);
            if let Some(old) = old {
                group.removed.push(old);
            }
        }
        res
    }

    fn loc_ctx(&self) -> &LocCtx<R> {
        &self.loc_ctx
    }

    fn mk_data(&self, data_id: DataId, content: R::Data) -> Result<LocDataId, DataVerifyError> {
        self.loc_ctx.mk_data(data_id, content)
    }
}
