use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::{EventContext, LocCtx, StoredEvent};
use crate::types::{AnyLocEventId, GlobalCoreId, LocGroupId, SenderPk};
use im::OrdMap;
use std::collections::BTreeMap;

type SG<K, V> = StateGraph<(), (), (), K, V>;

const PK_A: SenderPk = SenderPk([0u8; 32]);
const GCI_0: GlobalCoreId = GlobalCoreId(0);

fn eid(ts: u32, lid: u64) -> SGEventId {
    SGEventId::new(
        SGBucketId {
            timestamp: ts,
            global_core_id: GCI_0,
        },
        AnyLocEventId(lid),
    )
}

const fn lid(id: u64) -> AnyLocEventId {
    AnyLocEventId(id)
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

#[derive(Clone, Debug)]
enum TestEvent {
    SetX(i32),
    CopyXToY,
    CopyYToZ,
}

fn test_handler(event: &TestEvent, ctx: &HandlerCtx<(), (), (), EmptyRuntime, &str, i32>) {
    match event {
        TestEvent::SetX(val) => ctx.update("x", *val),
        TestEvent::CopyXToY => {
            if let Some(x) = ctx.query(&"x") {
                ctx.update("y", x + 1);
            }
        }
        TestEvent::CopyYToZ => {
            if let Some(y) = ctx.query(&"y") {
                ctx.update("z", y + 1);
            }
        }
    }
}

type EventStore<E> = BTreeMap<u64, (u32, E)>;

fn make_resolver<E: Clone>(
    events: &EventStore<E>,
) -> impl Fn(AnyLocEventId) -> (SGEventId, E) + '_ {
    move |local_id: AnyLocEventId| {
        let (ts, e) = events
            .get(&local_id.0)
            .expect("make_resolver: event not found");
        let sg_id = SGEventId::new(
            SGBucketId {
                timestamp: *ts,
                global_core_id: GCI_0,
            },
            local_id,
        );
        (sg_id, e.clone())
    }
}

fn nr() -> &'static dyn Fn(()) -> Timeline<(), ()> {
    &|_| Timeline {
        writes: OrdMap::new(),
    }
}

fn apply_added<E: Clone, H>(
    sg: &mut SG<&'static str, i32>,
    events: &mut EventStore<E>,
    handler: &H,
    r: &dyn Fn(()) -> Timeline<(), ()>,
    ctx: &LocCtx<EmptyRuntime>,
    added: &[(u64, u32, E)],
) where
    H: Fn(&E, &HandlerCtx<(), (), (), EmptyRuntime, &'static str, i32>),
{
    for (local_id, ts, e) in added {
        events.insert(*local_id, (*ts, e.clone()));
    }
    sg.apply(
        handler,
        &make_resolver(events),
        r,
        ctx,
        &DeltaList {
            removed: vec![],
            added: added.iter().map(|(l, _, _)| lid(*l)).collect(),
        },
    );
}

fn apply_removed<E: Clone, H>(
    sg: &mut SG<&'static str, i32>,
    events: &mut EventStore<E>,
    handler: &H,
    r: &dyn Fn(()) -> Timeline<(), ()>,
    ctx: &LocCtx<EmptyRuntime>,
    removed: &[u64],
) where
    H: Fn(&E, &HandlerCtx<(), (), (), EmptyRuntime, &'static str, i32>),
{
    sg.apply(
        handler,
        &make_resolver(events),
        r,
        ctx,
        &DeltaList {
            removed: removed.iter().map(|&id| lid(id)).collect(),
            added: vec![],
        },
    );
}

#[test]
fn single_event_update() {
    let mut sg: SG<&str, i32> = SG::new();
    let mut events = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &test_handler,
        nr(),
        &ctx,
        &[(1, 0, TestEvent::SetX(42))],
    );
    assert_eq!(sg.query(&"x"), Some(&42));
}

#[test]
fn query_and_propagation() {
    let mut sg: SG<&str, i32> = SG::new();
    let mut events = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &test_handler,
        nr(),
        &ctx,
        &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
    );
    assert_eq!(sg.query(&"y"), Some(&11));
    events.insert(1, (0, TestEvent::SetX(20)));
    sg.apply(
        &test_handler,
        &make_resolver(&events),
        nr(),
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(sg.query(&"y"), Some(&21));
}

#[test]
fn transitive_propagation() {
    let mut sg: SG<&str, i32> = SG::new();
    let mut events = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &test_handler,
        nr(),
        &ctx,
        &[
            (1, 0, TestEvent::SetX(10)),
            (2, 0, TestEvent::CopyXToY),
            (3, 0, TestEvent::CopyYToZ),
        ],
    );
    assert_eq!(sg.query(&"z"), Some(&12));
    events.insert(1, (0, TestEvent::SetX(20)));
    sg.apply(
        &test_handler,
        &make_resolver(&events),
        nr(),
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(sg.query(&"z"), Some(&22));
}

#[test]
fn no_propagation_when_value_unchanged() {
    let mut sg: SG<&str, i32> = SG::new();
    let mut events = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &test_handler,
        nr(),
        &ctx,
        &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
    );
    events.insert(1, (0, TestEvent::SetX(10)));
    sg.apply(
        &test_handler,
        &make_resolver(&events),
        nr(),
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(sg.query(&"y"), Some(&11));
}

#[test]
fn remove_event_cascades() {
    let mut sg: SG<&str, i32> = SG::new();
    let mut events = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &test_handler,
        nr(),
        &ctx,
        &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
    );
    apply_removed(&mut sg, &mut events, &test_handler, nr(), &ctx, &[1]);
    assert_eq!(sg.query(&"x"), None);
    assert_eq!(sg.query(&"y"), None);
}

#[test]
fn conditional_write_changes_on_re_evaluation() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        WriteYIfXPositive,
    }
    let handler = |ev: &E, ctx: &HandlerCtx<(), (), (), EmptyRuntime, &str, i32>| match ev {
        E::SetX(val) => ctx.update("x", *val),
        E::WriteYIfXPositive => {
            if let Some(x) = ctx.query(&"x") {
                if x > 0 {
                    ctx.update("y", x * 2);
                }
            }
        }
    };
    let mut sg: SG<&str, i32> = SG::new();
    let mut events: EventStore<E> = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &handler,
        nr(),
        &ctx,
        &[(1, 0, E::SetX(5)), (2, 0, E::WriteYIfXPositive)],
    );
    assert_eq!(sg.query(&"y"), Some(&10));
    events.insert(1, (0, E::SetX(-1)));
    sg.apply(
        &handler,
        &make_resolver(&events),
        nr(),
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(sg.query(&"y"), None);
}

#[test]
fn bounded_propagation_skips_events_after_next_write() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        OverwriteX(i32),
        ReadX(()),
    }
    let handler = |ev: &E, ctx: &HandlerCtx<(), (), (), EmptyRuntime, &str, i32>| match ev {
        E::SetX(val) => ctx.update("x", *val),
        E::OverwriteX(val) => ctx.update("x", *val),
        E::ReadX(_) => {
            if let Some(x) = ctx.query(&"x") {
                ctx.update("out", x);
            }
        }
    };
    let mut sg: SG<&str, i32> = SG::new();
    let mut events: EventStore<E> = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &handler,
        nr(),
        &ctx,
        &[
            (1, 0, E::SetX(10)),
            (2, 0, E::ReadX(())),
            (3, 0, E::ReadX(())),
            (5, 0, E::OverwriteX(99)),
            (7, 0, E::ReadX(())),
        ],
    );
    events.insert(1, (0, E::SetX(20)));
    sg.apply(
        &handler,
        &make_resolver(&events),
        nr(),
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(sg.query_at(&"out", eid(0, 2), &ctx), Some(&20));
    assert_eq!(sg.query_at(&"out", eid(0, 3), &ctx), Some(&20));
    assert_eq!(sg.query_at(&"out", eid(0, 7), &ctx), Some(&99)); // NOT re-processed
}

#[test]
fn handler_query_excludes_own_write() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        WriteAndReadX(i32),
    }
    let handler = |ev: &E, ctx: &HandlerCtx<(), (), (), EmptyRuntime, &str, i32>| match ev {
        E::SetX(val) => ctx.update("x", *val),
        E::WriteAndReadX(val) => {
            ctx.update("x", *val);
            if let Some(prev) = ctx.query(&"x") {
                ctx.update("saw_prev", prev);
            }
        }
    };
    let mut sg: SG<&str, i32> = SG::new();
    let mut events: EventStore<E> = EventStore::new();
    let ctx = make_test_ctx(10);
    apply_added(
        &mut sg,
        &mut events,
        &handler,
        nr(),
        &ctx,
        &[(1, 0, E::SetX(42)), (2, 0, E::WriteAndReadX(99))],
    );
    assert_eq!(sg.query(&"x"), Some(&99));
    assert_eq!(sg.query(&"saw_prev"), Some(&42));
}
