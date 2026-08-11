use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::StoredEvent;
use crate::core::storage::{GroupStore, InMemoryStorage, Storage};
use crate::types::{GroupEventId, LocGroupId, LocMsgTypeId, NodeId, SenderPk};
use imbl::OrdMap;
use std::collections::BTreeMap;

type SG<K, V> = StateGraph<(), (), (), K, V>;
type Store = InMemoryStorage<EmptyRuntime>;

const PK_A: SenderPk = SenderPk([0u8; 32]);

/// Drive a future on a throwaway compio runtime (state_graph is async but I/O-free).
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

fn eid(ts: u32, lid: u64) -> SGEventId {
    SGEventId::new(SGBucketId { timestamp: ts }, GroupEventId(lid))
}

const fn lid(id: u64) -> GroupEventId {
    GroupEventId(id)
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

#[derive(Clone, Debug)]
enum TestEvent {
    SetX(i32),
    CopyXToY,
    CopyYToZ,
}

async fn test_handler<R: async FnMut(&()) -> Timeline<(), ()>>(
    event: &TestEvent,
    ctx: &mut HandlerCtx<'_, (), (), (), EmptyRuntime, Store, &'static str, i32, R>,
) {
    match event {
        TestEvent::SetX(val) => ctx.update("x", *val),
        TestEvent::CopyXToY => {
            if let Some(x) = ctx.query(&"x").await {
                ctx.update("y", x + 1);
            }
        }
        TestEvent::CopyYToZ => {
            if let Some(y) = ctx.query(&"y").await {
                ctx.update("z", y + 1);
            }
        }
    }
}

type EventStore<E> = BTreeMap<u64, (u32, E)>;

fn make_resolver<E: Clone>(
    events: &EventStore<E>,
) -> impl async Fn(GroupEventId) -> (SGEventId, E) + '_ {
    async move |local_id: GroupEventId| {
        let (ts, e) = events
            .get(&(local_id.0 as u64))
            .expect("make_resolver: event not found");
        let sg_id = SGEventId::new(SGBucketId { timestamp: *ts }, local_id);
        (sg_id, e.clone())
    }
}

async fn apply_added<E: Clone, H, R>(
    sg: &mut SG<&'static str, i32>,
    events: &mut EventStore<E>,
    handler: &mut H,
    r: &mut R,
    store: &GroupStore<'_, EmptyRuntime, Store>,
    added: &[(u64, u32, E)],
) where
    H: async FnMut(&E, &mut HandlerCtx<'_, (), (), (), EmptyRuntime, Store, &'static str, i32, R>),
    R: async FnMut(&()) -> Timeline<(), ()>,
{
    for (local_id, ts, e) in added {
        events.insert(*local_id, (*ts, e.clone()));
    }
    sg.apply(
        handler,
        &make_resolver(events),
        r,
        store,
        &DeltaList {
            removed: vec![],
            added: added.iter().map(|(l, _, _)| lid(*l)).collect(),
        },
    )
    .await;
}

async fn apply_removed<E: Clone, H, R>(
    sg: &mut SG<&'static str, i32>,
    events: &mut EventStore<E>,
    handler: &mut H,
    r: &mut R,
    store: &GroupStore<'_, EmptyRuntime, Store>,
    removed: &[u64],
) where
    H: async FnMut(&E, &mut HandlerCtx<'_, (), (), (), EmptyRuntime, Store, &'static str, i32, R>),
    R: async FnMut(&()) -> Timeline<(), ()>,
{
    let _ = events;
    sg.apply(
        handler,
        &make_resolver(events),
        r,
        store,
        &DeltaList {
            removed: removed.iter().map(|&id| lid(id)).collect(),
            added: vec![],
        },
    )
    .await;
}

#[test]
fn single_event_update() {
    block_on(async {
        let mut sg: SG<&str, i32> = SG::new();
        let mut events = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut handler = test_handler;
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        apply_added(
            &mut sg,
            &mut events,
            &mut handler,
            &mut r,
            &ctx,
            &[(1, 0, TestEvent::SetX(42))],
        )
        .await;
        assert_eq!(sg.query(&"x"), Some(&42));
    });
}

#[test]
fn query_and_propagation() {
    block_on(async {
        let mut sg: SG<&str, i32> = SG::new();
        let mut events = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut handler = test_handler;
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        apply_added(
            &mut sg,
            &mut events,
            &mut handler,
            &mut r,
            &ctx,
            &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
        )
        .await;
        assert_eq!(sg.query(&"y"), Some(&11));
        events.insert(1, (0, TestEvent::SetX(20)));
        sg.apply(
            &mut handler,
            &make_resolver(&events),
            &mut r,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(1)],
            },
        )
        .await;
        assert_eq!(sg.query(&"y"), Some(&21));
    });
}

#[test]
fn transitive_propagation() {
    block_on(async {
        let mut sg: SG<&str, i32> = SG::new();
        let mut events = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut handler = test_handler;
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        apply_added(
            &mut sg,
            &mut events,
            &mut handler,
            &mut r,
            &ctx,
            &[
                (1, 0, TestEvent::SetX(10)),
                (2, 0, TestEvent::CopyXToY),
                (3, 0, TestEvent::CopyYToZ),
            ],
        )
        .await;
        assert_eq!(sg.query(&"z"), Some(&12));
        events.insert(1, (0, TestEvent::SetX(20)));
        sg.apply(
            &mut handler,
            &make_resolver(&events),
            &mut r,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(1)],
            },
        )
        .await;
        assert_eq!(sg.query(&"z"), Some(&22));
    });
}

#[test]
fn no_propagation_when_value_unchanged() {
    block_on(async {
        let mut sg: SG<&str, i32> = SG::new();
        let mut events = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut handler = test_handler;
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        apply_added(
            &mut sg,
            &mut events,
            &mut handler,
            &mut r,
            &ctx,
            &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
        )
        .await;
        events.insert(1, (0, TestEvent::SetX(10)));
        sg.apply(
            &mut handler,
            &make_resolver(&events),
            &mut r,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(1)],
            },
        )
        .await;
        assert_eq!(sg.query(&"y"), Some(&11));
    });
}

#[test]
fn remove_event_cascades() {
    block_on(async {
        let mut sg: SG<&str, i32> = SG::new();
        let mut events = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut handler = test_handler;
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        apply_added(
            &mut sg,
            &mut events,
            &mut handler,
            &mut r,
            &ctx,
            &[(1, 0, TestEvent::SetX(10)), (2, 0, TestEvent::CopyXToY)],
        )
        .await;
        apply_removed(&mut sg, &mut events, &mut handler, &mut r, &ctx, &[1]).await;
        assert_eq!(sg.query(&"x"), None);
        assert_eq!(sg.query(&"y"), None);
    });
}

#[test]
fn conditional_write_changes_on_re_evaluation() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        WriteYIfXPositive,
    }
    block_on(async {
        let handler = async |ev: &E,
                             ctx: &mut HandlerCtx<
            '_,
            (),
            (),
            (),
            EmptyRuntime,
            Store,
            &str,
            i32,
            _,
        >| {
            match ev {
                E::SetX(val) => ctx.update("x", *val),
                E::WriteYIfXPositive => {
                    if let Some(x) = ctx.query(&"x").await {
                        if x > 0 {
                            ctx.update("y", x * 2);
                        }
                    }
                }
            }
        };
        let mut sg: SG<&str, i32> = SG::new();
        let mut events: EventStore<E> = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        let mut h = handler;
        apply_added(
            &mut sg,
            &mut events,
            &mut h,
            &mut r,
            &ctx,
            &[(1, 0, E::SetX(5)), (2, 0, E::WriteYIfXPositive)],
        )
        .await;
        assert_eq!(sg.query(&"y"), Some(&10));
        events.insert(1, (0, E::SetX(-1)));
        sg.apply(
            &mut h,
            &make_resolver(&events),
            &mut r,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(1)],
            },
        )
        .await;
        assert_eq!(sg.query(&"y"), None);
    });
}

#[test]
fn bounded_propagation_skips_events_after_next_write() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        OverwriteX(i32),
        ReadX(()),
    }
    block_on(async {
        let handler = async |ev: &E,
                             ctx: &mut HandlerCtx<
            '_,
            (),
            (),
            (),
            EmptyRuntime,
            Store,
            &str,
            i32,
            _,
        >| {
            match ev {
                E::SetX(val) => ctx.update("x", *val),
                E::OverwriteX(val) => ctx.update("x", *val),
                E::ReadX(_) => {
                    if let Some(x) = ctx.query(&"x").await {
                        ctx.update("out", x);
                    }
                }
            }
        };
        let mut sg: SG<&str, i32> = SG::new();
        let mut events: EventStore<E> = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        let mut h = handler;
        apply_added(
            &mut sg,
            &mut events,
            &mut h,
            &mut r,
            &ctx,
            &[
                (1, 0, E::SetX(10)),
                (2, 0, E::ReadX(())),
                (3, 0, E::ReadX(())),
                (5, 0, E::OverwriteX(99)),
                (7, 0, E::ReadX(())),
            ],
        )
        .await;
        events.insert(1, (0, E::SetX(20)));
        sg.apply(
            &mut h,
            &make_resolver(&events),
            &mut r,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(1)],
            },
        )
        .await;
        assert_eq!(sg.query_at(&"out", eid(0, 2), &ctx).await, Some(&20));
        assert_eq!(sg.query_at(&"out", eid(0, 3), &ctx).await, Some(&20));
        assert_eq!(sg.query_at(&"out", eid(0, 7), &ctx).await, Some(&99)); // NOT re-processed
    });
}

#[test]
fn handler_query_excludes_own_write() {
    #[derive(Clone)]
    enum E {
        SetX(i32),
        WriteAndReadX(i32),
    }
    block_on(async {
        let handler = async |ev: &E,
                             ctx: &mut HandlerCtx<
            '_,
            (),
            (),
            (),
            EmptyRuntime,
            Store,
            &str,
            i32,
            _,
        >| {
            match ev {
                E::SetX(val) => ctx.update("x", *val),
                E::WriteAndReadX(val) => {
                    ctx.update("x", *val);
                    if let Some(prev) = ctx.query(&"x").await {
                        ctx.update("saw_prev", prev);
                    }
                }
            }
        };
        let mut sg: SG<&str, i32> = SG::new();
        let mut events: EventStore<E> = EventStore::new();
        let loc = make_test_ctx(10);
        let ctx = gs(&loc);
        let mut r = async |_: &()| Timeline::<(), ()> {
            writes: OrdMap::new(),
        };
        let mut h = handler;
        apply_added(
            &mut sg,
            &mut events,
            &mut h,
            &mut r,
            &ctx,
            &[(1, 0, E::SetX(42)), (2, 0, E::WriteAndReadX(99))],
        )
        .await;
        assert_eq!(sg.query(&"x"), Some(&99));
        assert_eq!(sg.query(&"saw_prev"), Some(&42));
    });
}
