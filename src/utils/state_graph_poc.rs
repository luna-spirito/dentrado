use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, StateGraphOut};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::{EventContext, LocCtx, StoredEvent};
use crate::types::{AnyLocEventId, GlobalCoreId, LocGroupId, SenderPk};
use im::OrdMap;
use proptest::prelude::*;
use std::collections::BTreeMap;

const PK_A: SenderPk = SenderPk([0u8; 32]);
const GCI_0: GlobalCoreId = GlobalCoreId(0);

fn eid(local_id: u64) -> SGEventId {
    SGEventId::new(
        SGBucketId {
            timestamp: 0,
            global_core_id: GCI_0,
        },
        AnyLocEventId(local_id),
    )
}

fn make_test_ctx(num_events: u64) -> LocCtx<EmptyRuntime> {
    let mut ctx = LocCtx::new();
    let sid_a = ctx.mk_loc_sender(PK_A, None);
    for i in 0..num_events {
        ctx.store_event(StoredEvent {
            group: LocGroupId(0),
            sender: sid_a,
            global_core_id: GCI_0,
            tx_id: i as u32,
            timestamp: 0,
            source_node: crate::types::NodeId(0),
            body: (),
        });
    }
    ctx
}

#[allow(dead_code)]
const fn lid(id: u64) -> AnyLocEventId {
    AnyLocEventId(id)
}

#[allow(dead_code)]
type UserId = SGEventId;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum SiteAccessLevel {
    None,
    #[allow(dead_code)]
    User,
    Moderator,
    Admin,
}

#[derive(Clone, Debug)]
enum SiteEvent {
    CreateUser,
    AdminSetAccessLevel {
        admin: Option<SGEventId>,
        target: SGEventId,
        level: SiteAccessLevel,
    },
}

fn site_handler(
    event: &SiteEvent,
    ctx: &HandlerCtx<SGEventId, (), (), EmptyRuntime, SGEventId, SiteAccessLevel>,
) {
    match event {
        SiteEvent::AdminSetAccessLevel {
            admin,
            target,
            level,
        } => {
            let has_access = match admin {
                None => true,
                Some(id) => matches!(ctx.query(id), Some(SiteAccessLevel::Admin)),
            };
            if has_access {
                ctx.update(*target, level.clone());
            }
        }
        SiteEvent::CreateUser => {}
    }
}

fn oneshot(
    events: &[(SGEventId, SiteEvent)],
    ctx: &LocCtx<EmptyRuntime>,
) -> StateGraph<SGEventId, (), (), SGEventId, SiteAccessLevel> {
    let mut sg = StateGraph::new();
    let store: BTreeMap<u64, (u32, SiteEvent)> = events
        .iter()
        .map(|(eid, e)| (eid.1 .0, (eid.0.timestamp, e.clone())))
        .collect();

    let r = |_: SGEventId| StateGraphOut::<(), ()> {
        writes: OrdMap::new(),
    };
    let added: Vec<AnyLocEventId> = events.iter().map(|(eid, _)| eid.1).collect();

    sg.apply(
        &site_handler,
        &|local_id: AnyLocEventId| {
            let (ts, e) = store.get(&local_id.0).expect("poc: event not found");
            let sg_id = SGEventId::new(
                SGBucketId {
                    timestamp: *ts,
                    global_core_id: GCI_0,
                },
                local_id,
            );
            (sg_id, e.clone())
        },
        &r,
        ctx,
        &DeltaList {
            removed: vec![],
            added,
        },
    );

    sg
}

fn multishot(
    events: &[(SGEventId, SiteEvent)],
    ctx: &LocCtx<EmptyRuntime>,
) -> StateGraph<SGEventId, (), (), SGEventId, SiteAccessLevel> {
    let mut sg = StateGraph::new();
    let store: BTreeMap<u64, (u32, SiteEvent)> = events
        .iter()
        .map(|(eid, e)| (eid.1 .0, (eid.0.timestamp, e.clone())))
        .collect();

    let r = |_: SGEventId| StateGraphOut::<(), ()> {
        writes: OrdMap::new(),
    };
    let resolver = |local_id: AnyLocEventId| {
        let (ts, e) = store.get(&local_id.0).expect("poc: event not found");
        let sg_id = SGEventId::new(
            SGBucketId {
                timestamp: *ts,
                global_core_id: GCI_0,
            },
            local_id,
        );
        (sg_id, e.clone())
    };

    for (eid, _) in events.iter() {
        sg.apply(
            &site_handler,
            &resolver,
            &r,
            ctx,
            &DeltaList {
                removed: vec![],
                added: vec![eid.1],
            },
        );
    }

    sg
}

fn sg_to_lists(
    sg: &StateGraph<SGEventId, (), (), SGEventId, SiteAccessLevel>,
) -> Vec<(SGEventId, Vec<(SGEventId, SiteAccessLevel)>)> {
    let mut r: Vec<_> = sg
        .keys()
        .map(|k| {
            (
                *k,
                sg.timeline_for(k).map(|(e, v)| (e, v.clone())).collect(),
            )
        })
        .collect();
    r.sort_by_key(|(k, _)| *k);
    r
}

fn test1_events() -> Vec<(SGEventId, SiteEvent)> {
    vec![
        (eid(0), SiteEvent::CreateUser),
        (eid(1), SiteEvent::CreateUser),
        (eid(2), SiteEvent::CreateUser),
        (eid(3), SiteEvent::CreateUser),
        (
            eid(4),
            SiteEvent::AdminSetAccessLevel {
                admin: None,
                target: eid(0),
                level: SiteAccessLevel::Admin,
            },
        ),
        (
            eid(5),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(0)),
                target: eid(1),
                level: SiteAccessLevel::Moderator,
            },
        ),
        (
            eid(6),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(1)),
                target: eid(1),
                level: SiteAccessLevel::Admin,
            },
        ),
        (
            eid(7),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(1)),
                target: eid(3),
                level: SiteAccessLevel::Moderator,
            },
        ),
        (
            eid(8),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(0)),
                target: eid(2),
                level: SiteAccessLevel::Admin,
            },
        ),
        (
            eid(9),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(2)),
                target: eid(1),
                level: SiteAccessLevel::None,
            },
        ),
        (
            eid(10),
            SiteEvent::AdminSetAccessLevel {
                admin: Some(eid(2)),
                target: eid(4),
                level: SiteAccessLevel::Moderator,
            },
        ),
    ]
}

#[test]
fn poc_model_test1() {
    let ctx = make_test_ctx(11);
    let result = sg_to_lists(&oneshot(&test1_events(), &ctx));
    let expected = vec![
        (eid(0), vec![(eid(4), SiteAccessLevel::Admin)]),
        (
            eid(1),
            vec![
                (eid(5), SiteAccessLevel::Moderator),
                (eid(9), SiteAccessLevel::None),
            ],
        ),
        (eid(2), vec![(eid(8), SiteAccessLevel::Admin)]),
        (eid(4), vec![(eid(10), SiteAccessLevel::Moderator)]),
    ];
    assert_eq!(result, expected);
}

fn shuffle_events(events: &[(SGEventId, SiteEvent)], seed: u64) -> Vec<(SGEventId, SiteEvent)> {
    let mut v = events.to_vec();
    let mut rng = seed;
    for i in (1..v.len()).rev() {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let j = (rng >> 33) as usize % (i + 1);
        v.swap(i, j);
    }
    v
}

proptest! {
    #[test]
    fn multishot_converges(seed in 0u64..1000) {
        let ctx = make_test_ctx(11);
        let events = test1_events();
        let shuffled = shuffle_events(&events, seed);
        let oneshot_result = sg_to_lists(&oneshot(&shuffled, &ctx));
        let multishot_result = sg_to_lists(&multishot(&shuffled, &ctx));
        prop_assert_eq!(oneshot_result, multishot_result);
    }
}
