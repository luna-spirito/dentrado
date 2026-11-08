use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime, RuntimeTypes},
        storage::{GearView, InMemoryStorage, IsStorage, Watermark},
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
    async fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::InvitesCount { branch } => Ok(Self::InvitesCount {
                branch: branch.localize(remapper).await?,
            }),
        }
    }
}

/// The gear's private state, flattened into the view: `(watermark, out)`.
const CACHE_KEY: &[u8] = b"state";

type CacheState = ((u64, u64), i64);

fn decode(bytes: &[u8]) -> CacheState {
    postcard::from_bytes(bytes).expect("a valid counter cache record")
}

fn encode(state: &CacheState) -> Vec<u8> {
    postcard::to_stdvec(state).expect("a flat tuple serializes")
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BranchData {
    pub creator: LocUserId,
    pub created_at: i64,
}

impl Localizable for BranchData {
    async fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(BranchData {
            creator: self.creator.localize(remapper).await?,
            created_at: self.created_at,
        })
    }
}

impl GlobalHash for BranchData {
    fn global_hash(&self, resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        let resolved_creator = resolver.resolve_user(self.creator)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&resolved_creator.id.to_le_bytes());
        hasher.update(&resolved_creator.identity_server_pk.0);
        hasher.update(&self.created_at.to_le_bytes());
        Ok(*hasher.finalize().as_bytes())
    }
}

#[derive(Debug, Clone)]
pub struct WikiCounterRuntime;

impl RuntimeTypes for WikiCounterRuntime {
    type GearId = WikiGear;
    type GearOut = i64;
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = LocDataId;
    type Body = LocUserId;
    type Data = BranchData;
}

impl IsRuntime for WikiCounterRuntime {
    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            WikiGear::InvitesCount { branch } => GearMeta::Event {
                msg_type: MSG_INVITE,
                group: *branch,
            },
        }
    }

    async fn run_step<S: IsStorage<Self>>(
        ctx: &mut GearCtx<Self, S>,
        input: GearInput<Self>,
        view: &mut S::View<'_>,
    ) -> GearProduce<Self> {
        let ((a, r), mut out) = view
            .get(CACHE_KEY)
            .await
            .map_or(((0, 0), 0), |b| decode(&b));
        let watermark = Watermark(a, r);
        let GearInput::Events(group) = input else {
            return GearProduce::Ship(out);
        };
        let diff = ctx.storage().diff_group(group, watermark).await;
        out += diff.added.len() as i64;
        out -= diff.removed.len() as i64;
        let wm = diff.watermark;
        view.put(CACHE_KEY, &encode(&((wm.0, wm.1), out)));
        GearProduce::Ship(out)
    }
}

#[test]
fn wiki_engine() {
    let mut tc: TestCluster<WikiCounterRuntime, InMemoryStorage<WikiCounterRuntime>> =
        TestCluster::start(&[2, 3, 4], ());

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

    let alice_loc_uid = tc.mk_loc_user(alice_uid);
    let bob_loc_uid = tc.mk_loc_user(bob_uid);
    let carol_loc_uid = tc.mk_loc_user(carol_uid);
    let dave_loc_uid = tc.mk_loc_user(dave_uid);

    // Seed branch b0
    let b0 = tc.add_data(BranchData {
        creator: alice_loc_uid,
        created_at: 1,
    });
    tc.mk_loc_group(MSG_INVITE, b0);

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
    tc.mk_loc_group(MSG_INVITE, b1);

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
