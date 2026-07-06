use dentrado::{
    core::{
        core_ctx::Core,
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

    fn run_step(
        _gear: &Self::GearId,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut Self::GearCache,
    ) -> i64 {
        let Some(group) = group else {
            return cache.agg;
        };
        let Some((added_ids, removed_ids)) = core.query_events(
            group,
            (cache.processed_added, cache.processed_removed),
            |a, r| (a.to_vec(), r.to_vec()),
        ) else {
            return cache.agg;
        };
        for &eid in &added_ids {
            let body = core.stored_event(eid).map(|e| e.body).unwrap();
            cache.agg += body.a * body.b;
        }
        for &eid in &removed_ids {
            let body = core.stored_event(eid).map(|e| e.body).unwrap();
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
