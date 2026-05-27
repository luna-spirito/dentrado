use std::{
    cell::Cell,
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

use kolorinko::{
    core::{
        db::{create_peer_channel_pair, Db, DbConfig, PeerChannels},
        gear::Runtime,
        loc_ctx::{EventContext, LocCtx},
    },
    fadeno::{
        bridge::{FadenoModule, FadenoRuntime},
        types::{KolGear, LocValue, TagRegistry},
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
    db: Db<R>,
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
    pub(crate) fn start(core_counts: &[u32], module: R::Module) -> Self {
        let num_nodes = core_counts.len();
        assert!(num_nodes > 0, "TestCluster needs at least one node");
        let module = Arc::new(module);

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
            let db = Db::start(config).expect("Db::start failed");
            nodes.push(Node { db });
        }

        let drain_duration = if num_nodes > 1 {
            Duration::from_millis(100)
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
        let hash = R::hash_data(&content, &self.loc_ctx).expect("hash_data failed");
        let data_id = DataId {
            timestamp: ts,
            hash,
        };
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

        let handle = self.random_db();
        handle
            .post_events(wire_ctx, wire_events, timestamp)
            .expect("post_events failed");
    }

    pub(crate) fn run_gear(&self, gear: R::GearId) -> R::GearOut {
        self.drain();
        let (wire_gear, wire_ctx) = self.remap_gear(gear);
        let handle = self.random_db();
        handle
            .run_gear(wire_gear, wire_ctx)
            .expect("run_gear failed")
    }

    pub(crate) fn run_gear_on(&self, machine_idx: usize, gear: R::GearId) -> R::GearOut {
        self.drain();
        let (wire_gear, wire_ctx) = self.remap_gear(gear);
        let handle = &self.nodes[machine_idx].db;
        handle
            .run_gear(wire_gear, wire_ctx)
            .expect("run_gear failed")
    }

    #[must_use]
    pub(crate) fn data_id(&self, did: LocDataId) -> DataId {
        self.loc_ctx
            .get_data(did, |(d, _)| *d)
            .expect("data not found")
    }

    pub(crate) fn remap_gear(&self, gear: R::GearId) -> (R::GearId, WireLocCtx<R>) {
        let builder = WireLocCtxBuilder::new(&self.loc_ctx);
        let wire_gear = builder.remap(gear).expect("WireLocCtxBuilder: remap gear");
        let wire_ctx = builder.build();
        (wire_gear, wire_ctx)
    }

    fn random_db(&self) -> &Db<R> {
        let idx = self.rng.next_usize(self.nodes.len());
        &self.nodes[idx].db
    }

    fn drain(&self) {
        if !self.drain_duration.is_zero() {
            std::thread::sleep(self.drain_duration);
        }
    }
}

pub(crate) struct WikiTestCluster(TestCluster<FadenoRuntime>);

impl Deref for WikiTestCluster {
    type Target = TestCluster<FadenoRuntime>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl DerefMut for WikiTestCluster {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl WikiTestCluster {
    pub(crate) fn start(core_counts: &[u32], mut module: FadenoModule) -> Self {
        let creator = module.ensure_tag_id(b"creator");
        let created_at = module.ensure_tag_id(b"created_at");
        let _ = module.ensure_tag_set(&[creator, created_at]);
        Self(TestCluster::start(core_counts, module))
    }

    pub(crate) fn add_seed_branch(
        &mut self,
        invite_mt: LocMsgTypeId,
        creator_uid: LocUserId,
    ) -> LocDataId {
        let content = self.tags().make_record(&[
            (b"creator", LocValue::KolUserId(creator_uid)),
            (b"created_at", LocValue::Num(1i64)),
        ]);
        let did = self.add_data(content.clone());

        self.loc_ctx
            .mk_loc_group(invite_mt, LocValue::KolDataId(did));
        did
    }

    pub(crate) fn mk_loc_user(&self, uid: UserId) -> LocUserId {
        EventContext::mk_loc_user(&self.loc_ctx, uid)
    }

    pub(crate) fn kol_user_id(&self, uid: UserId) -> LocValue {
        LocValue::KolUserId(self.mk_loc_user(uid))
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
        FadenoRuntime::route_group(&LocValue::KolDataId(did), &self.loc_ctx)
            .expect("route_group failed")
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
