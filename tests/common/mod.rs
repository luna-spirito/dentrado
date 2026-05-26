use std::{cell::Cell, collections::HashMap, sync::Arc, time::Duration};

use kolorinko::{
    core::{
        db::{create_peer_channel_pair, Db, DbConfig, DbHandle, PeerChannels},
        gear::Runtime,
        loc_ctx::{EventContext, LocCtx, StoredEvent},
    },
    types::*,
    wire::{WireEventBody, WireLocCtx, WireLocCtxBuilder},
};

struct XorShift64 {
    state: Cell<u64>,
}

impl XorShift64 {
    fn new() -> Self {
        Self {
            state: Cell::new(0x1234_5678_9ABC_DEF0),
        }
    }

    fn next_usize(&self, bound: usize) -> usize {
        let mut x = self.state.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.set(x);
        (x as usize) % bound
    }
}

struct Node<R: Runtime> {
    _db: Db<R>,
    handle: DbHandle<R>,
}

pub(crate) struct TestCluster<R: Runtime> {
    module: Arc<R::Module>,
    nodes: Vec<Node<R>>,
    loc_ctx: LocCtx<R>,
    next_data_ts: u32,
    drain_duration: Duration,
    rng: XorShift64,
}

impl<R: Runtime> TestCluster<R> {
    pub(crate) fn start(core_counts: &[u32], module: Arc<R::Module>) -> Self {
        let num_nodes = core_counts.len();
        assert!(num_nodes > 0, "TestCluster needs at least one node");

        let mut all_peers: Vec<HashMap<NodeId, PeerChannels<R>>> =
            (0..num_nodes).map(|_| HashMap::new()).collect();

        for i in 0..num_nodes {
            for j in (i + 1)..num_nodes {
                let num_channels = core_counts[i].min(core_counts[j]);
                let mut halves_i = Vec::with_capacity(num_channels as usize);
                let mut halves_j = Vec::with_capacity(num_channels as usize);
                for _ in 0..num_channels {
                    let (hi, hj) = create_peer_channel_pair::<R>();
                    halves_i.push(hi);
                    halves_j.push(hj);
                }
                all_peers[i].insert(
                    NodeId(j as u32),
                    PeerChannels {
                        remote_num_cores: core_counts[j],
                        channels: halves_i,
                    },
                );
                all_peers[j].insert(
                    NodeId(i as u32),
                    PeerChannels {
                        remote_num_cores: core_counts[i],
                        channels: halves_j,
                    },
                );
            }
        }

        let mut nodes = Vec::with_capacity(num_nodes);
        for (i, &num_cores) in core_counts.iter().enumerate() {
            let config = DbConfig {
                num_cores,
                node_id: NodeId(i as u32),
                module: module.clone(),
                peers: std::mem::take(&mut all_peers[i]),
            };
            let (_db, handle) = Db::start(config).expect("Db::start failed");
            nodes.push(Node { _db, handle });
        }

        let drain_duration = if num_nodes > 1 {
            Duration::from_millis(10)
        } else {
            Duration::ZERO
        };

        Self {
            module,
            nodes,
            loc_ctx: LocCtx::new(),
            next_data_ts: 1,
            drain_duration,
            rng: XorShift64::new(),
        }
    }

    pub(crate) fn add_user(&mut self, pk: SenderPk, uid: UserId) -> LocSenderId {
        EventContext::mk_loc_sender(&mut self.loc_ctx, pk, Some(uid))
    }

    pub(crate) fn add_data(&mut self, content: R::Data) -> LocDataId {
        let ts = self.next_data_ts;
        self.next_data_ts += 1;
        let data_id = compute_data_id::<R>(ts, &content);
        EventContext::mk_data(&mut self.loc_ctx, data_id, content).expect("mk_data failed")
    }

    pub(crate) fn post_events(
        &self,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    ) {
        let builder = WireLocCtxBuilder::new(&self.loc_ctx);
        let wire_events: Vec<_> = events
            .into_iter()
            .map(|e| builder.remap(e).expect("WireLocCtxBuilder: remap event"))
            .collect();
        let wire_ctx = builder.build();

        let handle = self.random_handle();
        handle
            .post_events(wire_ctx, wire_events, timestamp)
            .expect("post_events failed");
    }

    pub(crate) fn run_gear(&self, gear: R::GearId) -> R::GearOut {
        self.drain();
        let (wire_gear, wire_ctx) = self.remap_gear(gear);
        let handle = self.random_handle();
        handle
            .run_gear(wire_gear, wire_ctx)
            .expect("run_gear failed")
    }

    #[must_use]
    pub(crate) fn data_id(&self, did: LocDataId) -> DataId {
        self.loc_ctx.get_data(did).expect("data not found").0
    }

    pub(crate) fn remap_gear(&self, gear: R::GearId) -> (R::GearId, WireLocCtx<R>) {
        let builder = WireLocCtxBuilder::new(&self.loc_ctx);
        let wire_gear = builder.remap(gear).expect("WireLocCtxBuilder: remap gear");
        let wire_ctx = builder.build();
        (wire_gear, wire_ctx)
    }

    fn random_handle(&self) -> &DbHandle<R> {
        let idx = self.rng.next_usize(self.nodes.len());
        &self.nodes[idx].handle
    }

    fn drain(&self) {
        if !self.drain_duration.is_zero() {
            std::thread::sleep(self.drain_duration);
        }
    }
}

use kolorinko::fadeno::{
    bridge::{FadenoModule, FadenoRuntime},
    types::*,
};

pub(crate) type FadenoTestCluster = TestCluster<FadenoRuntime>;

impl TestCluster<FadenoRuntime> {
    pub(crate) fn add_seed_branch(
        &mut self,
        invite_mt: LocMsgTypeId,
        creator_sid: LocSenderId,
    ) -> LocDataId {
        let content = Self::empty_record();
        let did = self.add_data(content.clone());

        let b_core_id = compute_branch_core_id(self.data_id(did), &content);

        let group =
            EventContext::mk_loc_group(&mut self.loc_ctx, invite_mt, LocValue::KolDataId(did));
        EventContext::store_event(
            &mut self.loc_ctx,
            StoredEvent {
                group,
                sender: creator_sid,
                global_core_id: b_core_id,
                tx_id: 0,
                timestamp: 1,
                source_node: NodeId(0),
                body: Self::empty_record(),
            },
        );
        did
    }

    pub(crate) fn build_gear(&self, closure: LocValue, args: Vec<LocValue>) -> KolGear {
        let result = self
            .module
            .call_with_storage(closure, args, &self.loc_ctx)
            .expect("gear construction failed");
        match result {
            LocValue::KolGear(g) => *g,
            other => panic!("expected KolGear, got {other:?}"),
        }
    }

    pub(crate) fn build_and_run_gear(&self, closure: LocValue, args: Vec<LocValue>) -> LocValue {
        let gear = self.build_gear(closure, args);
        self.run_gear(gear)
    }

    #[must_use]
    pub(crate) fn module(&self) -> &FadenoModule {
        &self.module
    }

    #[must_use]
    pub(crate) fn tags(&self) -> &TagRegistry {
        self.module.tags()
    }

    pub(crate) fn msg_type(&self, name: &[u8]) -> LocMsgTypeId {
        match self.tags().record_get(self.module().exports(), name) {
            Some(LocValue::KolEventTypeId(id)) => id,
            other => panic!(
                "msg_type({}): expected KolEventTypeId, got {other:?}",
                std::str::from_utf8(name).unwrap_or("?")
            ),
        }
    }

    pub(crate) fn branch_core_id(&self, did: LocDataId) -> GlobalCoreId {
        let (data_id, content) = self.loc_ctx.get_data(did).expect("data not found");
        compute_branch_core_id(*data_id, content)
    }

    pub(crate) fn find_cross_core_doc_id(&self, invited_core: u32, num_cores: u32) -> u64 {
        let doc_content_closure = self
            .tags()
            .record_get(self.module().exports(), b"doc_content")
            .expect("missing doc_content export")
            .clone();

        (1..10_000)
            .find(|&d| {
                let doc_result = self
                    .module
                    .call_with_storage(
                        doc_content_closure.clone(),
                        vec![LocValue::Num(d as i64)],
                        &self.loc_ctx,
                    )
                    .expect("gear call failed");
                let LocValue::KolGear(doc_gear) = doc_result else {
                    panic!("expected KolGear");
                };
                let (doc_gear_wire, wc) = self.remap_gear(*doc_gear);

                let gear_core = FadenoRuntime::route_group(doc_gear_wire.group(), &wc)
                    .unwrap()
                    .route(num_cores);
                if gear_core == invited_core {
                    return false;
                }
                let event_core = FadenoRuntime::route_group(&LocValue::Num(d as i64), &wc)
                    .unwrap()
                    .route(num_cores);
                event_core == gear_core
            })
            .expect("should find a suitable doc_id for cross-core routing")
    }

    #[must_use]
    pub(crate) fn empty_record() -> LocValue {
        LocValue::Record {
            tag_set: Arc::new(vec![0]),
            fields: Arc::new(Vec::new()),
        }
    }
}

pub(crate) fn wire_event(
    sender: LocSenderId,
    tx_id: u32,
    msg_type: LocMsgTypeId,
    group: LocValue,
    body: LocValue,
) -> WireEventBody<LocValue, LocValue> {
    WireEventBody {
        sender,
        tx_id,
        msg_type,
        group,
        body,
    }
}

pub(crate) fn compute_data_id<R: Runtime>(timestamp: u32, content: &R::Data) -> DataId {
    let empty_resolver = WireLocCtx::<R>::default();
    let hash = R::hash_data(content, &empty_resolver).expect("hash_data failed");
    DataId { timestamp, hash }
}

pub(crate) fn compute_branch_core_id(data_id: DataId, content: &LocValue) -> GlobalCoreId {
    let wire_ctx = WireLocCtx::<FadenoRuntime> {
        users: vec![],
        senders: vec![],
        data: vec![(data_id, content.clone())],
    };
    FadenoRuntime::route_group(&LocValue::KolDataId(LocDataId(0)), &wire_ctx)
        .expect("route_group failed")
}
