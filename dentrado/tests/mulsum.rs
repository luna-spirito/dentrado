use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::{Core, GearCtx},
        gear::{GearInput, GearMeta, IsRuntime},
        storage::{CacheSer, InMemoryStorage, PageId, Storage},
    },
    types::*,
    wire::WireEventBody,
};

mod common;
use common::TestCluster;

pub const MSG_MULSUM: LocMsgTypeId = LocMsgTypeId(1);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum MulSumGear {
    MulSum { bucket: i64 },
}

impl Localizable for MulSumGear {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
pub struct MulSumBody {
    pub a: i64,
    pub b: i64,
}

impl Localizable for MulSumBody {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct MulSumCache<W> {
    pub watermark: W,
    pub agg: i64,
}

impl<W> CacheSer for MulSumCache<W> {
    fn page_roots(&self) -> &[PageId] {
        &[]
    }
}

#[derive(Debug, Clone)]
pub struct MulSumRuntime;

impl IsRuntime for MulSumRuntime {
    type GearId = MulSumGear;
    type GearOut = i64;
    type Module = ();
    type Group = i64;
    type Body = MulSumBody;
    type Data = ();
    type GearCache<W>
        = MulSumCache<W>
    where
        W: Debug + Clone + 'static;

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let hash = *blake3::Hasher::new().finalize().as_bytes();
        Ok(hash)
    }

    fn route_group(
        group: &Self::Group,
        _resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&group.to_le_bytes());
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            MulSumGear::MulSum { bucket } => GearMeta::Event {
                msg_type: MSG_MULSUM,
                group: *bucket,
            },
        }
    }

    fn make_cache<W: Debug + Clone + Default + 'static>(
        _gear: &Self::GearId,
    ) -> Self::GearCache<W> {
        MulSumCache {
            watermark: W::default(),
            agg: 0,
        }
    }

    async fn run_step<S: Storage<Self>>(
        ctx: &mut GearCtx<Self, S>,
        input: GearInput,
        cache: &mut Self::GearCache<S::Watermark>,
    ) -> i64 {
        let GearInput::Events(group) = input else {
            return cache.agg;
        };
        let core = ctx.core();
        let diff = ctx
            .storage()
            .diff_group(group, cache.watermark.clone())
            .await;
        let store = core.group_store(group);

        for eid in &diff.added {
            let body = store.stored_event(*eid).await.unwrap().body;
            cache.agg += body.a * body.b;
        }
        for eid in &diff.removed {
            let body = store.stored_event(*eid).await.unwrap().body;
            cache.agg -= body.a * body.b;
        }
        cache.watermark = diff.watermark;
        cache.agg
    }
}

#[test]
fn mulsum_engine() {
    let mut tc: TestCluster<MulSumRuntime, InMemoryStorage<MulSumRuntime>> =
        TestCluster::start(&[2, 3, 4], ());

    let bucket: i64 = 0;
    tc.mk_loc_group(MSG_MULSUM, bucket);

    let gear = MulSumGear::MulSum { bucket };

    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let alice = tc.add_user(alice_pk, alice_uid);

    let body1 = MulSumBody { a: 3, b: 4 };
    let body2 = MulSumBody { a: 5, b: 2 };

    tc.post_events(
        vec![
            WireEventBody {
                sender: alice,
                tx_id: 0,
                msg_type: MSG_MULSUM,
                group: bucket,
                body: body1,
            },
            WireEventBody {
                sender: alice,
                tx_id: 1,
                msg_type: MSG_MULSUM,
                group: bucket,
                body: body2,
            },
        ],
        1,
    );

    let sum = tc.run_gear(gear.clone());
    assert_eq!(sum, 22);

    let body3 = MulSumBody { a: 7, b: 1 };

    tc.post_events(
        vec![WireEventBody {
            sender: alice,
            tx_id: 2,
            msg_type: MSG_MULSUM,
            group: bucket,
            body: body3,
        }],
        2,
    );

    let sum = tc.run_gear(gear);
    assert_eq!(sum, 29);
}

// --- Phase 2: subscription API (local gears) ---------------------------------

/// Commands a test worker can perform against its pinned `Rc<Core>`.
enum WorkerCmd {
    /// `read_gear` (subscribe + borrow + drop) and reply with the value.
    Read(MulSumGear, flume::Sender<i64>),
    /// `subscribe_gear`, hold the subscription, reply with the initial value.
    Subscribe(MulSumGear, flume::Sender<i64>),
    /// Drop the most recently held subscription (LIFO).
    DropSub,
}

/// Exercises `subscribe_gear`/`read_gear` via the realistic worker_fn path.
/// Single-core cluster ⟹ every gear is local to its querying worker.
#[test]
fn local_gear_read_and_subscribe_via_worker() {
    let (cmd_tx, cmd_rx) = flume::unbounded::<WorkerCmd>();

    let worker = move |core: std::rc::Rc<Core<MulSumRuntime, InMemoryStorage<MulSumRuntime>>>| {
        let cmd_rx = cmd_rx.clone();
        let mut held: Vec<
            dentrado::core::core_ctx::Subscription<MulSumRuntime, InMemoryStorage<MulSumRuntime>>,
        > = Vec::new();
        async move {
            while let Ok(cmd) = cmd_rx.recv_async().await {
                match cmd {
                    WorkerCmd::Read(gear, reply) => {
                        let v = core.read_gear(gear).await;
                        let _ = reply.send(v);
                    }
                    WorkerCmd::Subscribe(gear, reply) => {
                        let sub = core.subscribe_gear(gear).await;
                        let v = sub.current();
                        held.push(sub);
                        let _ = reply.send(v);
                    }
                    WorkerCmd::DropSub => {
                        held.pop();
                    }
                }
            }
        }
    };

    let mut tc: TestCluster<MulSumRuntime, InMemoryStorage<MulSumRuntime>> =
        TestCluster::start_with_worker(&[1], (), worker);

    let bucket: i64 = 0;
    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    tc.mk_loc_group(MSG_MULSUM, bucket);
    let alice = tc.add_user(alice_pk, alice_uid);

    // agg += 3*4 = 12
    tc.post_events(
        vec![WireEventBody {
            sender: alice,
            tx_id: 0,
            msg_type: MSG_MULSUM,
            group: bucket,
            body: MulSumBody { a: 3, b: 4 },
        }],
        1,
    );

    // read_gear recomputes fresh each call.
    let (rtx, rrx) = flume::bounded(1);
    cmd_tx
        .send(WorkerCmd::Read(MulSumGear::MulSum { bucket }, rtx))
        .unwrap();
    assert_eq!(rrx.recv().unwrap(), 12);

    // agg += 5*2 = 10 → 22
    tc.post_events(
        vec![WireEventBody {
            sender: alice,
            tx_id: 1,
            msg_type: MSG_MULSUM,
            group: bucket,
            body: MulSumBody { a: 5, b: 2 },
        }],
        2,
    );

    let (rtx, rrx) = flume::bounded(1);
    cmd_tx
        .send(WorkerCmd::Read(MulSumGear::MulSum { bucket }, rtx))
        .unwrap();
    assert_eq!(rrx.recv().unwrap(), 22);

    // subscribe once → initial value 22; hold it.
    let (stx, srx) = flume::bounded(1);
    cmd_tx
        .send(WorkerCmd::Subscribe(MulSumGear::MulSum { bucket }, stx))
        .unwrap();
    assert_eq!(srx.recv().unwrap(), 22);

    // Dropping the subscription must not panic and leaves the gear in limbo.
    // A subsequent read re-activates it from limbo.
    cmd_tx.send(WorkerCmd::DropSub).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let (rtx, rrx) = flume::bounded(1);
    cmd_tx
        .send(WorkerCmd::Read(MulSumGear::MulSum { bucket }, rtx))
        .unwrap();
    assert_eq!(rrx.recv().unwrap(), 22);
}
