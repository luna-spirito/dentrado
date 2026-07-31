use std::{collections::HashMap, num::NonZero, sync::Arc, time::Duration};

use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell, DoorbellHandle, PeerChannels, create_peer_channel_pair},
        gear::IsRuntime,
        storage::Storage,
    },
    types::*,
    wire::{WireEventBody, WireLocCtx, WireLocCtxBuilder},
};

struct XorShift64 {
    state: std::cell::Cell<u64>,
}

impl XorShift64 {
    fn new() -> Self {
        Self {
            state: std::cell::Cell::new(0x1234_5678_9ABC_DEF0),
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

struct Node<R: IsRuntime> {
    db: Db<R>,
}

pub(crate) struct TestCluster<R: IsRuntime, S: Storage<R>> {
    module: Arc<R::Module>,
    nodes: Vec<Node<R>>,
    /// Client-side context for building wire events: allocates local ids and
    /// resolves them into a `WireLocCtx`. Separate from each core's own storage
    /// (the merger re-localizes into those on the receiving core).
    pub(crate) client: S,
    next_data_ts: u32,
    drain_duration: Duration,
    rng: XorShift64,
}

/// Drive a client-side future to completion. The Db cores run on their own
/// per-thread compio runtimes; this only drives client-side `R::Storage` ops,
/// which are trivially `RefCell` work — the `InMemoryStorage` futures are
/// immediately ready and never yield. A single-pass no-op executor is enough
/// and avoids the cost (and FD/memory pressure) of building a compio runtime
/// per call.
pub(crate) fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake, Waker};
    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    match fut.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("client storage op yielded unexpectedly"),
    }
}

impl<R: IsRuntime, S: Storage<R>> TestCluster<R, S>
where
    S: Default,
{
    pub(crate) fn start(core_counts: &[u32], module: R::Module) -> Self {
        Self::start_with_worker(core_counts, module, |_| std::future::pending::<()>())
    }

    /// Start the cluster with a user-supplied worker function per core. This is
    /// the realistic access pattern: workers receive `Rc<Core>` and may call
    /// `subscribe_gear`/`read_gear` directly on their pinned core.
    pub(crate) fn start_with_worker<W, F>(
        core_counts: &[u32],
        module: R::Module,
        worker_fn: W,
    ) -> Self
    where
        W: Fn(std::rc::Rc<Core<R, S>>) -> F + Clone + Send + 'static,
        F: std::future::Future<Output = ()> + 'static,
    {
        let num_nodes = core_counts.len();
        assert!(num_nodes > 0, "TestCluster needs at least one node");
        let module = Arc::new(module);

        // Create doorbells upfront: one per core per node.
        let mut all_doorbells: Vec<Vec<(Doorbell, DoorbellHandle)>> = core_counts
            .iter()
            .map(|&nc| (0..nc).map(|_| Doorbell::new()).collect())
            .collect();

        let mut all_peers: Vec<HashMap<NodeId, PeerChannels<R>>> =
            (0..num_nodes).map(|_| HashMap::new()).collect();

        for i in 0..num_nodes {
            for j in (i + 1)..num_nodes {
                let num_channels = core_counts[i].min(core_counts[j]);
                let mut halves_i = Vec::with_capacity(num_channels as usize);
                let mut halves_j = Vec::with_capacity(num_channels as usize);
                for c in 0..num_channels as usize {
                    let doorbell_i = all_doorbells[i][c].1.clone();
                    let doorbell_j = all_doorbells[j][c].1.clone();
                    let (hi, hj) = create_peer_channel_pair::<R>(doorbell_j, doorbell_i);
                    halves_i.push(hi);
                    halves_j.push(hj);
                }
                all_peers[i].insert(
                    NodeId(j as u32),
                    PeerChannels {
                        remote_num_cores: NonZero::new(core_counts[j]).unwrap(),
                        channels: halves_i,
                    },
                );
                all_peers[j].insert(
                    NodeId(i as u32),
                    PeerChannels {
                        remote_num_cores: NonZero::new(core_counts[i]).unwrap(),
                        channels: halves_j,
                    },
                );
            }
        }

        let mut nodes = Vec::with_capacity(num_nodes);
        for (i, &num_cores) in core_counts.iter().enumerate() {
            let doorbells = std::mem::take(&mut all_doorbells[i]);
            let config = DbConfig {
                num_cores: NonZero::new(num_cores).unwrap(),
                node_id: NodeId(i as u32),
                module: module.clone(),
                peers: std::mem::take(&mut all_peers[i]),
                doorbells,
                make_storage: Arc::new(|| S::default()),
            };
            let db = Db::start_with_worker(config, worker_fn.clone()).expect("Db::start failed");
            nodes.push(Node { db });
        }

        let drain_duration = if num_nodes > 1 {
            Duration::from_millis(10)
        } else {
            Duration::ZERO
        };

        Self {
            module,
            nodes,
            client: S::default(),
            next_data_ts: 1,
            drain_duration,
            rng: XorShift64::new(),
        }
    }

    pub(crate) fn add_user(&mut self, pk: SenderPk, uid: UserId) -> LocSenderId {
        block_on(self.client.mk_loc_sender(pk, Some(uid)))
    }

    pub(crate) fn mk_loc_user(&mut self, uid: UserId) -> LocUserId {
        block_on(self.client.mk_loc_user(uid))
    }

    pub(crate) fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) {
        block_on(self.client.mk_loc_group(msg_type, group));
    }

    pub(crate) fn add_data(&mut self, content: R::Data) -> LocDataId {
        let ts = self.next_data_ts;
        self.next_data_ts += 1;
        let hash = R::hash_data(&content, &self.client).expect("hash_data failed");
        let data_id = DataId {
            timestamp: ts,
            hash,
        };
        block_on(self.client.mk_data(data_id, content)).expect("mk_data failed")
    }

    pub(crate) fn post_events(
        &self,
        events: Vec<WireEventBody<R::Group, R::Body>>,
        timestamp: u32,
    ) {
        block_on(async {
            let mut builder = WireLocCtxBuilder::new(&self.client);
            let mut wire_events = Vec::with_capacity(events.len());
            for e in events {
                wire_events.push(
                    builder
                        .remap(e)
                        .await
                        .expect("WireLocCtxBuilder: remap event"),
                );
            }
            let wire_ctx = builder.build();

            let handle = self.random_db();
            handle
                .post_events(wire_ctx, wire_events, timestamp)
                .expect("post_events failed");
        });
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
        block_on(async { self.client.fetch_data(did).await.map(|(d, _)| d) })
            .expect("data not found")
    }

    pub(crate) fn remap_gear(&self, gear: R::GearId) -> (R::GearId, WireLocCtx<R>) {
        let mut builder = WireLocCtxBuilder::new(&self.client);
        let wire_gear = block_on(builder.remap(gear)).expect("WireLocCtxBuilder: remap gear");
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
