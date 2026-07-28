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

pub const MSG_INVITE: LocMsgTypeId = LocMsgTypeId(1);

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum WikiGear {
    InvitesCount { branch: LocDataId },
}

impl Localizable for WikiGear {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::InvitesCount { branch } => Ok(Self::InvitesCount {
                branch: branch.localize(remapper)?,
            }),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CounterCache {
    pub processed_added: usize,
    pub processed_removed: usize,
    pub out: i64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BranchData {
    pub creator: LocUserId,
    pub created_at: i64,
}

impl Localizable for BranchData {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(BranchData {
            creator: self.creator.localize(remapper)?,
            created_at: self.created_at,
        })
    }
}

#[derive(Debug, Clone)]
pub struct WikiCounterRuntime;

impl IsRuntime for WikiCounterRuntime {
    type GearId = WikiGear;
    type GearOut = i64;
    type Module = ();
    type Group = LocDataId;
    type Body = LocUserId;
    type Data = BranchData;
    type GearCache = CounterCache;

    fn hash_data(
        data: &Self::Data,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let resolved_creator = resolver.resolve_user(data.creator)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&resolved_creator.id.to_le_bytes());
        hasher.update(&resolved_creator.identity_server_pk.0);
        hasher.update(&data.created_at.to_le_bytes());
        Ok(*hasher.finalize().as_bytes())
    }

    fn route_group(
        group: &Self::Group,
        resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let resolved = resolver.resolve_data(*group)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&resolved.timestamp.to_le_bytes());
        hasher.update(&resolved.hash);
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &Self::GearId) -> (LocMsgTypeId, Self::Group) {
        match gear {
            WikiGear::InvitesCount { branch } => (MSG_INVITE, *branch),
        }
    }

    fn make_cache(_gear: &Self::GearId) -> Self::GearCache {
        CounterCache {
            processed_added: 0,
            processed_removed: 0,
            out: 0,
        }
    }

    async fn run_step(
        _ctx: &mut GearCtx<Self>,
        group: Option<LocGroupId>,
        cache: &mut Self::GearCache,
    ) -> i64 {
        let Some(group) = group else {
            return cache.out;
        };
        let Some((added_ids, removed_ids)) = _ctx.query_events(
            group,
            (cache.processed_added, cache.processed_removed),
            |a, r| (a.to_vec(), r.to_vec()),
        ) else {
            return cache.out;
        };
        cache.out += added_ids.len() as i64;
        cache.out -= removed_ids.len() as i64;
        cache.processed_added += added_ids.len();
        cache.processed_removed += removed_ids.len();
        cache.out
    }
}

#[test]
fn wiki_engine() {
    let mut tc: TestCluster<WikiCounterRuntime> = TestCluster::start(&[2, 3, 4], ());

    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let bob_uid = UserId {
        id: 2,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let carol_uid = UserId {
        id: 3,
        identity_server_pk: IdentityServerPk([0; 32]),
    };
    let dave_uid = UserId {
        id: 10,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice = tc.add_user(SenderPk([42u8; 32]), alice_uid);

    let alice_loc_uid = tc.loc_ctx.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.loc_ctx.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.loc_ctx.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.loc_ctx.mk_loc_user(dave_uid);

    // Seed branch b0
    let b0 = tc.add_data(BranchData {
        creator: alice_loc_uid,
        created_at: 1,
    });
    tc.loc_ctx.mk_loc_group(MSG_INVITE, b0);

    let gear_0 = WikiGear::InvitesCount { branch: b0 };

    tc.post_events(
        vec![
            WireEventBody {
                sender: alice,
                tx_id: 0,
                msg_type: MSG_INVITE,
                group: b0,
                body: bob_loc_uid,
            },
            WireEventBody {
                sender: alice,
                tx_id: 1,
                msg_type: MSG_INVITE,
                group: b0,
                body: carol_loc_uid,
            },
        ],
        1,
    );

    let count = tc.run_gear(gear_0.clone());
    assert_eq!(count, 2);

    tc.post_events(
        vec![WireEventBody {
            sender: alice,
            tx_id: 2,
            msg_type: MSG_INVITE,
            group: b0,
            body: dave_loc_uid,
        }],
        2,
    );

    let count = tc.run_gear(gear_0.clone());
    assert_eq!(count, 3);

    // Seed branch b1
    let b1 = tc.add_data(BranchData {
        creator: alice_loc_uid,
        created_at: 2,
    });
    tc.loc_ctx.mk_loc_group(MSG_INVITE, b1);

    let gear_1 = WikiGear::InvitesCount { branch: b1 };

    tc.post_events(
        vec![WireEventBody {
            sender: alice,
            tx_id: 3,
            msg_type: MSG_INVITE,
            group: b1,
            body: dave_loc_uid,
        }],
        4,
    );

    let count = tc.run_gear(gear_1);
    assert_eq!(count, 1, "branch 1 has 1 invite");

    let count = tc.run_gear(gear_0);
    assert_eq!(count, 3, "branch 0 still 3");
}
