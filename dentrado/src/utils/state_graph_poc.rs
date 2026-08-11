use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::StoredEvent;
use crate::core::storage::{GroupStore, InMemoryStorage, Storage};
use crate::types::{GroupEventId, LocGroupId, LocMsgTypeId, NodeId, SenderPk};
use imbl::OrdMap;
use proptest::prelude::*;
use std::collections::BTreeMap;

type Store = InMemoryStorage<EmptyRuntime>;

const PK_A: SenderPk = SenderPk([0u8; 32]);

/// Drive an I/O-free future to completion. state_graph futures only do
/// `RefCell` work and never yield, so a single-pass no-op executor suffices
/// (a real compio runtime per call OOMs under parallel test load).
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
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
        Poll::Pending => panic!("future yielded unexpectedly"),
    }
}

fn gs(ctx: &Store) -> GroupStore<'_, EmptyRuntime, Store> {
    GroupStore::new(ctx, LocGroupId(0))
}

fn eid(local_id: u64) -> SGEventId {
    SGEventId::new(SGBucketId { timestamp: 0 }, GroupEventId(local_id))
}

fn make_test_ctx(num_events: u64) -> Store {
    let ctx = InMemoryStorage::<EmptyRuntime>::default();
    let sid_a = block_on(ctx.mk_loc_sender(PK_A, None));
    block_on(ctx.mk_loc_group(LocMsgTypeId(0), ()));
    for i in 0..num_events {
        block_on(ctx.store_event(
            LocGroupId(0),
            StoredEvent {
                sender: sid_a,
                tx_id: i as u32,
                timestamp: 0,
                source_node: NodeId(0),
                body: (),
            },
        ));
    }
    ctx
}

#[allow(dead_code)]
const fn lid(id: u64) -> GroupEventId {
    GroupEventId(id)
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

async fn site_handler<R: async FnMut(&SGEventId) -> Timeline<(), ()>>(
    event: &SiteEvent,
    ctx: &mut HandlerCtx<'_, SGEventId, (), (), EmptyRuntime, Store, SGEventId, SiteAccessLevel, R>,
) {
    match event {
        SiteEvent::AdminSetAccessLevel {
            admin,
            target,
            level,
        } => {
            let has_access = match admin {
                None => true,
                Some(id) => matches!(ctx.query(id).await, Some(SiteAccessLevel::Admin)),
            };
            if has_access {
                ctx.update(*target, level.clone());
            }
        }
        SiteEvent::CreateUser => {}
    }
}

async fn oneshot(
    events: &[(SGEventId, SiteEvent)],
    ctx: &GroupStore<'_, EmptyRuntime, Store>,
) -> StateGraph<SGEventId, (), (), SGEventId, SiteAccessLevel> {
    let mut sg = StateGraph::new();
    let store: BTreeMap<u64, (u32, SiteEvent)> = events
        .iter()
        .map(|(eid, e)| (eid.1.0 as u64, (eid.0.timestamp, e.clone())))
        .collect();

    let mut r = async |_: &SGEventId| Timeline::<(), ()> {
        writes: OrdMap::new(),
    };
    let added: Vec<GroupEventId> = events.iter().map(|(eid, _)| eid.1).collect();
    let mut h = site_handler;

    sg.apply(
        &mut h,
        &async |local_id: GroupEventId| {
            let (ts, e) = store
                .get(&(local_id.0 as u64))
                .expect("poc: event not found");
            let sg_id = SGEventId::new(SGBucketId { timestamp: *ts }, local_id);
            (sg_id, e.clone())
        },
        &mut r,
        ctx,
        &DeltaList {
            removed: vec![],
            added,
        },
    )
    .await;

    sg
}

async fn multishot(
    events: &[(SGEventId, SiteEvent)],
    ctx: &GroupStore<'_, EmptyRuntime, Store>,
) -> StateGraph<SGEventId, (), (), SGEventId, SiteAccessLevel> {
    let mut sg = StateGraph::new();
    let store: BTreeMap<u64, (u32, SiteEvent)> = events
        .iter()
        .map(|(eid, e)| (eid.1.0 as u64, (eid.0.timestamp, e.clone())))
        .collect();

    let mut r = async |_: &SGEventId| Timeline::<(), ()> {
        writes: OrdMap::new(),
    };
    let resolver = async |local_id: GroupEventId| {
        let (ts, e) = store
            .get(&(local_id.0 as u64))
            .expect("poc: event not found");
        let sg_id = SGEventId::new(SGBucketId { timestamp: *ts }, local_id);
        (sg_id, e.clone())
    };

    let mut h = site_handler;
    for (eid, _) in events.iter() {
        sg.apply(
            &mut h,
            &resolver,
            &mut r,
            ctx,
            &DeltaList {
                removed: vec![],
                added: vec![eid.1],
            },
        )
        .await;
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
    let loc = make_test_ctx(11);
    let ctx = gs(&loc);
    let result = sg_to_lists(&block_on(oneshot(&test1_events(), &ctx)));
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
        let loc = make_test_ctx(11); let ctx = gs(&loc);
        let events = test1_events();
        let shuffled = shuffle_events(&events, seed);
        let oneshot_result = sg_to_lists(&block_on(oneshot(&shuffled, &ctx)));
        let multishot_result = sg_to_lists(&block_on(multishot(&shuffled, &ctx)));
        prop_assert_eq!(oneshot_result, multishot_result);
    }
}
