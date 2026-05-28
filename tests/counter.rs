use kolorinko::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
        gear::Runtime,
        loc_ctx::EventContext,
    },
    fadeno::{hash_loc_value, types::LocValue},
    types::*,
    wire::{MergeError, WireEventBody, WireLocCtx},
};
use std::{collections::HashMap, sync::Arc};

mod common;
use common::TestCluster;

const MSG_BRANCH_CREATE: LocMsgTypeId = LocMsgTypeId(0);
const MSG_ATTACH: LocMsgTypeId = LocMsgTypeId(1);

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
struct Branch {
    id: Id,
    name: String,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
struct AttachGroup {
    branch: Id,
    doc: Id,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Clone, Debug)]
struct AttachBody {
    delta: i64,
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

impl Localizable for AttachGroup {
    fn localize<U, S, D, E>(
        &self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        Ok(None)
    }
}

impl Localizable for AttachBody {
    fn localize<U, S, D, E>(
        &self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        Ok(None)
    }
}

impl Localizable for Branch {
    fn localize<U, S, D, E>(
        &self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        Ok(None)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum AnyGearId {
    Doc { branch: Id, doc: Id },
}

impl Localizable for AnyGearId {
    fn localize<U, S, D, E>(
        &self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        Ok(None)
    }
}

#[derive(Clone, Debug)]
struct CounterRuntime;

impl Runtime for CounterRuntime {
    type GearId = AnyGearId;
    type GearOut = i64;
    type Module = ();
    type Group = LocValue;
    type Body = LocValue;
    type Data = LocValue;

    fn hash_data(
        data: &LocValue,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hash_loc_value(data, resolver, &mut hasher)?;
        Ok(*hasher.finalize().as_bytes())
    }

    fn route_group(
        group: &LocValue,
        wire_ctx: &dyn GlobalResolver,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hash_loc_value(group, wire_ctx, &mut hasher)?;
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &AnyGearId) -> (LocMsgTypeId, LocValue) {
        match gear {
            AnyGearId::Doc { branch, doc } => {
                let group = LocValue::List(Arc::new(vec![
                    LocValue::Num(doc.0 as i64),
                    LocValue::Num(branch.0 as i64),
                ]));
                (MSG_ATTACH, group)
            }
        }
    }

    fn make_cache(_gear: &AnyGearId) -> Box<dyn std::any::Any> {
        Box::new(IC {
            input: Query::new(),
            cache: 0i64,
        })
    }

    fn run_step(
        gear: &AnyGearId,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut dyn std::any::Any,
    ) -> i64 {
        let cache = cache
            .downcast_mut::<IC<Query, i64>>()
            .expect("AnyGearId cache type mismatch: expected IC<Query, i64>");
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
                .loc_ctx()
                .get_stored_event(*eid, |e| e.body.clone())
                .expect("counter gear: event not found");
            if let LocValue::Num(delta) = &body {
                cache.cache += delta;
            }
        }
        for eid in &removed_ids {
            let body = core
                .loc_ctx()
                .get_stored_event(*eid, |e| e.body.clone())
                .expect("counter gear: removed event not found");
            if let LocValue::Num(delta) = &body {
                cache.cache -= delta;
            }
        }
        cache.input.processed_added += added_ids.len();
        cache.input.processed_removed += removed_ids.len();
        let _ = gear;
        cache.cache
    }
}

fn empty_record() -> LocValue {
    LocValue::Record {
        tag_set: Arc::new(vec![0]),
        fields: Arc::new(Vec::new()),
    }
}

fn branch_create_wire_event(
    sender: LocSenderId,
    tx_id: u32,
    msg_type: LocMsgTypeId,
) -> WireEventBody<LocValue, LocValue> {
    WireEventBody {
        sender,
        tx_id,
        msg_type,
        group: empty_record(),
        body: empty_record(),
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

    tc.post_events(
        vec![branch_create_wire_event(alice_sid, 0, MSG_BRANCH_CREATE)],
        1,
    );

    let branch_0_id = Id(0);

    let attach_group_42 = LocValue::List(Arc::new(vec![
        LocValue::Num(42),
        LocValue::Num(branch_0_id.0 as i64),
    ]));

    tc.post_events(
        vec![
            WireEventBody {
                sender: alice_sid,
                tx_id: 1,
                msg_type: MSG_ATTACH,
                group: attach_group_42.clone(),
                body: LocValue::Num(5),
            },
            WireEventBody {
                sender: alice_sid,
                tx_id: 2,
                msg_type: MSG_ATTACH,
                group: attach_group_42.clone(),
                body: LocValue::Num(-2),
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
            body: LocValue::Num(7),
        }],
        3,
    );

    let output = tc.run_gear(AnyGearId::Doc {
        branch: branch_0_id,
        doc: Id(42),
    });
    assert_eq!(output, 10);

    let attach_group_99 = LocValue::List(Arc::new(vec![
        LocValue::Num(99),
        LocValue::Num(branch_0_id.0 as i64),
    ]));

    tc.post_events(
        vec![WireEventBody {
            sender: alice_sid,
            tx_id: 4,
            msg_type: MSG_ATTACH,
            group: attach_group_99,
            body: LocValue::Num(42),
        }],
        4,
    );

    let output = tc.run_gear(AnyGearId::Doc {
        branch: branch_0_id,
        doc: Id(99),
    });
    assert_eq!(output, 42);
}

#[test]
fn malformed_wire_ctx_returns_error_not_panic() {
    let alice_pk = SenderPk([42u8; 32]);
    let alice_uid = UserId {
        id: 1,
        identity_server_pk: IdentityServerPk([0; 32]),
    };

    let (doorbell, dbh) = Doorbell::new();
    let db: Db<CounterRuntime> = Db::start(DbConfig {
        num_cores: 1,
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
                    group: LocValue::Num(0),
                    body: LocValue::Num(0),
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
                    group: LocValue::Num(0),
                    body: LocValue::Num(0),
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
                    group: LocValue::Num(0),
                    body: LocValue::KolUserId(LocUserId::new_debug(50)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::UserOutOfBounds { .. }));
    }

    {
        let self_referencing_content = LocValue::List(Arc::new(vec![
            LocValue::KolDataId(LocDataId::new_debug(0)), // self-reference = forward ref
        ]));
        let dummy_data_id = DataId {
            timestamp: 0,
            hash: [0u8; 32],
        };
        let wire_ctx = WireLocCtx {
            users: vec![alice_uid],
            senders: vec![(alice_pk, 0)],
            data: vec![(dummy_data_id, self_referencing_content)],
        };
        let err = db
            .post_events(
                wire_ctx,
                vec![WireEventBody {
                    sender: LocSenderId::new_debug(0),
                    tx_id: 0,
                    msg_type: LocMsgTypeId(0),
                    group: LocValue::Num(0),
                    body: LocValue::KolDataId(LocDataId::new_debug(0)),
                }],
                0,
            )
            .unwrap_err();
        assert!(matches!(err, MergeError::DataForwardReference { .. }));
    }
}
