use dentrado::{
    core::{
        core_ctx::{Core, GearCtx},
        gear::IsRuntime,
        loc_ctx::{EventContext, EventStore},
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
    fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
pub struct MulSumBody {
    pub a: i64,
    pub b: i64,
}

impl Localizable for MulSumBody {
    fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Debug, Clone)]
pub struct MulSumCache {
    pub processed_added: usize,
    pub processed_removed: usize,
    pub agg: i64,
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
    type GearCache = MulSumCache;

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

    fn meta(gear: &Self::GearId) -> (LocMsgTypeId, Self::Group) {
        match gear {
            MulSumGear::MulSum { bucket } => (MSG_MULSUM, *bucket),
        }
    }

    fn make_cache(_gear: &Self::GearId) -> Self::GearCache {
        MulSumCache {
            processed_added: 0,
            processed_removed: 0,
            agg: 0,
        }
    }

    async fn run_step(
        _ctx: &mut GearCtx<Self>,
        group: Option<LocGroupId>,
        cache: &mut Self::GearCache,
    ) -> i64 {
        let Some(group) = group else {
            return cache.agg;
        };
        let Some((added_ids, removed_ids)) = _ctx.query_events(
            group,
            (cache.processed_added, cache.processed_removed),
            |a, r| (a.to_vec(), r.to_vec()),
        ) else {
            return cache.agg;
        };
        for &eid in &added_ids {
            let body = _ctx.stored_event(eid).map(|e| e.body).unwrap();
            cache.agg += body.a * body.b;
        }
        for &eid in &removed_ids {
            let body = _ctx.stored_event(eid).map(|e| e.body).unwrap();
            cache.agg -= body.a * body.b;
        }
        cache.processed_added += added_ids.len();
        cache.processed_removed += removed_ids.len();
        cache.agg
    }
}

#[test]
fn mulsum_engine() {
    let mut tc: TestCluster<MulSumRuntime> = TestCluster::start(&[2, 3, 4], ());

    let bucket: i64 = 0;
    tc.loc_ctx.mk_loc_group(MSG_MULSUM, bucket);

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

    let worker = move |core: std::rc::Rc<Core<MulSumRuntime>>| {
        let cmd_rx = cmd_rx.clone();
        let mut held: Vec<dentrado::core::core_ctx::Subscription<MulSumRuntime>> = Vec::new();
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

    let mut tc: TestCluster<MulSumRuntime> = TestCluster::start_with_worker(&[1], (), worker);

    let bucket: i64 = 0;
    tc.loc_ctx.mk_loc_group(MSG_MULSUM, bucket);
    let gear = MulSumGear::MulSum { bucket };

    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
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
    cmd_tx.send(WorkerCmd::Read(gear.clone(), rtx)).unwrap();
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
    cmd_tx.send(WorkerCmd::Read(gear.clone(), rtx)).unwrap();
    assert_eq!(rrx.recv().unwrap(), 22);

    // subscribe once → initial value 22; hold it.
    let (stx, srx) = flume::bounded(1);
    cmd_tx
        .send(WorkerCmd::Subscribe(gear.clone(), stx))
        .unwrap();
    assert_eq!(srx.recv().unwrap(), 22);

    // Dropping the subscription must not panic and leaves the gear in limbo.
    // A subsequent read re-activates it from limbo.
    cmd_tx.send(WorkerCmd::DropSub).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));

    let (rtx, rrx) = flume::bounded(1);
    cmd_tx.send(WorkerCmd::Read(gear.clone(), rtx)).unwrap();
    assert_eq!(rrx.recv().unwrap(), 22);
}
