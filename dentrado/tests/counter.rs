use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
        gear::IsRuntime,
        loc_ctx::EventStore,
    },
    types::*,
    wire::{MergeError, WireEventBody, WireLocCtx},
};
use std::{collections::HashMap, num::NonZero, sync::Arc};

mod common;
use common::TestCluster;

const MSG_BRANCH_CREATE: LocMsgTypeId = LocMsgTypeId(0);
const MSG_ATTACH: LocMsgTypeId = LocMsgTypeId(1);

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
struct Branch {
    id: Id,
    name: String,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
struct CounterGroup {
    doc: Id,
    branch: Id,
}

impl Localizable for CounterGroup {
    fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Clone, Copy, Debug)]
struct Query {
    processed_added: usize,
    processed_removed: usize,
}

impl Query {
    fn new() -> Self {
        Self {
            processed_added: 0,
            processed_removed: 0,
        }
    }
}

#[derive(Debug)]
struct IC<I, C> {
    input: I,
    cache: C,
}

impl<I: Clone, C: Clone> Clone for IC<I, C> {
    fn clone(&self) -> Self {
        Self {
            input: self.input.clone(),
            cache: self.cache.clone(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum AnyGearId {
    Doc { branch: Id, doc: Id },
}

impl Localizable for AnyGearId {
    fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Clone, Debug)]
struct CounterRuntime;

impl IsRuntime for CounterRuntime {
    type GearId = AnyGearId;
    type GearOut = i64;
    type Module = ();
    type Group = CounterGroup;
    type Body = i64;
    type Data = ();
    type GearCache = IC<Query, i64>;

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let hash = *blake3::Hasher::new().finalize().as_bytes();
        Ok(hash)
    }

    fn route_group(
        group: &Self::Group,
        _wire_ctx: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&group.doc.0.to_le_bytes());
        hasher.update(&group.branch.0.to_le_bytes());
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &AnyGearId) -> (LocMsgTypeId, Self::Group) {
        match gear {
            AnyGearId::Doc { branch, doc } => (
                MSG_ATTACH,
                CounterGroup {
                    doc: *doc,
                    branch: *branch,
                },
            ),
        }
    }

    fn make_cache(_gear: &AnyGearId) -> Self::GearCache {
        IC {
            input: Query::new(),
            cache: 0i64,
        }
    }

    fn run_step(
        _gear: &AnyGearId,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut Self::GearCache,
    ) -> i64 {
        let Some(group) = group else {
            return cache.cache;
        };
        let Some((added_ids, removed_ids)) = core.query_events(
            group,
            (cache.input.processed_added, cache.input.processed_removed),
            |a, r| (a.to_vec(), r.to_vec()),
        ) else {
            return cache.cache;
        };
        for eid in &added_ids {
            let body = core
                .stored_event(*eid)
                .map(|e| e.body)
                .expect("counter gear: event not found");
            cache.cache += body;
        }
        for eid in &removed_ids {
            let body = core
                .stored_event(*eid)
                .map(|e| e.body)
                .expect("counter gear: removed event not found");
            cache.cache -= body;
        }
        cache.input.processed_added += added_ids.len();
        cache.input.processed_removed += removed_ids.len();
        cache.cache
    }
}

#[test]
fn doc_counter() {
    let mut tc: TestCluster<CounterRuntime> = TestCluster::start(&[2, 3, 4], ());

    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let alice_sid = tc.add_user(alice_pk, alice_uid);

    let branch_0_id = Id(0);
    let attach_group_42 = CounterGroup {
        doc: Id(42),
        branch: branch_0_id,
    };

    tc.post_events(
        vec![
            WireEventBody {
                sender: alice_sid,
                tx_id: 1,
                msg_type: MSG_ATTACH,
                group: attach_group_42.clone(),
                body: 5,
            },
            WireEventBody {
                sender: alice_sid,
                tx_id: 2,
                msg_type: MSG_ATTACH,
                group: attach_group_42.clone(),
                body: -2,
            },
        ],
        2,
    );

    let output = tc.run_gear(AnyGearId::Doc {
        branch: branch_0_id,
        doc: Id(42),
    });
    assert_eq!(output, 3);

    tc.post_events(
        vec![WireEventBody {
            sender: alice_sid,
            tx_id: 3,
            msg_type: MSG_ATTACH,
            group: attach_group_42.clone(),
            body: 7,
        }],
        3,
    );

    let output = tc.run_gear(AnyGearId::Doc {
        branch: branch_0_id,
        doc: Id(42),
    });
    assert_eq!(output, 10);

    let attach_group_99 = CounterGroup {
        doc: Id(99),
        branch: branch_0_id,
    };

    tc.post_events(
        vec![WireEventBody {
            sender: alice_sid,
            tx_id: 4,
            msg_type: MSG_ATTACH,
            group: attach_group_99,
            body: 42,
        }],
        4,
    );

    let output = tc.run_gear(AnyGearId::Doc {
        branch: branch_0_id,
        doc: Id(99),
    });
    assert_eq!(output, 42);
}

#[derive(Clone, Debug)]
enum TestRefBody {
    User(LocUserId),
    Data(LocDataId),
}

impl Localizable for TestRefBody {
    fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        match self {
            Self::User(u) => Ok(Self::User(u.localize(remapper)?)),
            Self::Data(d) => Ok(Self::Data(d.localize(remapper)?)),
        }
    }
}

#[derive(Clone, Debug)]
struct TestRefRuntime;
impl IsRuntime for TestRefRuntime {
    type GearId = ();
    type GearOut = ();
    type Module = ();
    type Group = ();
    type Body = TestRefBody;
    type Data = LocDataId;
    type GearCache = ();

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        Ok([0; 32])
    }
    fn route_group(
        _key: &Self::Group,
        _resolver: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        Ok(GlobalCoreId(0))
    }
    fn meta(_gear: &Self::GearId) -> (LocMsgTypeId, Self::Group) {
        (LocMsgTypeId(0), ())
    }
    fn make_cache(_gear: &Self::GearId) -> Self::GearCache {}
    fn run_step(
        _gear: &Self::GearId,
        _core: &Core<Self>,
        _group: Option<LocGroupId>,
        _cache: &mut Self::GearCache,
    ) -> Self::GearOut {
    }
}

#[test]
fn malformed_wire_ctx_returns_error_not_panic() {
    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let (doorbell, dbh) = Doorbell::new();
    let db: Db<TestRefRuntime> = Db::start(DbConfig {
        num_cores: NonZero::new(1).unwrap(),
        node_id: NodeId(0),
        module: Arc::new(()),
        peers: HashMap::new(),
        doorbells: vec![(doorbell, dbh)],
    })
    .unwrap();

    {
        let wire_ctx = WireLocCtx {
            users: vec![],
            senders: vec![(alice_pk, 99)],
            ..Default::default()
        };
        let err = db
            .post_events(
                wire_ctx,
                vec![WireEventBody {
                    sender: LocSenderId::new_debug(0),
                    tx_id: 0,
                    msg_type: LocMsgTypeId(0),
                    group: (),
                    body: TestRefBody::User(LocUserId::new_debug(0)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::UserOutOfBounds { .. }));
    }

    {
        let wire_ctx = WireLocCtx {
            users: vec![alice_uid],
            senders: vec![(alice_pk, 0)],
            ..Default::default()
        };
        let err = db
            .post_events(
                wire_ctx,
                vec![WireEventBody {
                    sender: LocSenderId::new_debug(5),
                    tx_id: 0,
                    msg_type: LocMsgTypeId(0),
                    group: (),
                    body: TestRefBody::User(LocUserId::new_debug(0)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::SenderOutOfBounds { .. }));
    }

    {
        let wire_ctx = WireLocCtx {
            users: vec![alice_uid],
            senders: vec![(alice_pk, 0)],
            ..Default::default()
        };
        let err = db
            .post_events(
                wire_ctx,
                vec![WireEventBody {
                    sender: LocSenderId::new_debug(0),
                    tx_id: 0,
                    msg_type: LocMsgTypeId(0),
                    group: (),
                    body: TestRefBody::User(LocUserId::new_debug(50)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::UserOutOfBounds { .. }));
    }

    {
        let self_referencing_content = LocDataId::new_debug(1); // self-reference = forward ref
        let dummy_data_id = DataId {
            timestamp: 0,
            hash: [0u8; 32],
        };
        let dummy_data_id2 = DataId {
            timestamp: 0,
            hash: [1u8; 32],
        };
        let wire_ctx = WireLocCtx {
            users: vec![alice_uid],
            senders: vec![(alice_pk, 0)],
            data: vec![
                (dummy_data_id, self_referencing_content),
                (dummy_data_id2, LocDataId::new_debug(0)),
            ],
        };
        let err = db
            .post_events(
                wire_ctx,
                vec![WireEventBody {
                    sender: LocSenderId::new_debug(0),
                    tx_id: 0,
                    msg_type: LocMsgTypeId(0),
                    group: (),
                    body: TestRefBody::Data(LocDataId::new_debug(0)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::DataForwardReference { .. }));
    }
}
