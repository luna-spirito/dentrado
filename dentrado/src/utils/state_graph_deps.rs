use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::{EventContext, GroupStore, LocCtx, StoredEvent};
use crate::types::{GroupEventId, LocGroupId, SenderPk};
use im::OrdMap;
use std::collections::BTreeMap;

const PK_A: SenderPk = SenderPk([0u8; 32]);

/// Drive a future on a throwaway compio runtime.
fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .expect("compio runtime build failed")
        .block_on(fut)
}

fn gs(ctx: &LocCtx<EmptyRuntime>) -> GroupStore<'_, EmptyRuntime> {
    GroupStore::new(ctx, LocGroupId(0))
}

fn eid(ts: u32, lid: u64) -> SGEventId {
    SGEventId::new(SGBucketId { timestamp: ts }, GroupEventId(lid))
}

const fn lid(id: u64) -> GroupEventId {
    GroupEventId(id)
}

fn make_test_ctx(num_events: u64) -> LocCtx<EmptyRuntime> {
    let mut ctx = LocCtx::new();
    let sid_a = ctx.mk_loc_sender(PK_A, None);
    for i in 0..num_events {
        ctx.store_event(
            LocGroupId(0),
            StoredEvent {
                sender: sid_a,
                tx_id: i as u32,
                timestamp: 0,
                source_node: crate::types::NodeId(0),
                body: (),
            },
        );
    }
    ctx
}

type DocSG = StateGraph<u64, u64, bool, &'static str, i32>;
type InviteSG = StateGraph<u64, u64, bool, u64, bool>;

type EventStore<E> = BTreeMap<u64, (u32, E)>;

fn make_resolver<E: Clone>(events: &EventStore<E>) -> impl Fn(GroupEventId) -> (SGEventId, E) + '_ {
    move |local_id: GroupEventId| {
        let (ts, e) = events
            .get(&(local_id.0 as u64))
            .expect("make_resolver: event not found");
        let sg_id = SGEventId::new(SGBucketId { timestamp: *ts }, local_id);
        (sg_id, e.clone())
    }
}

#[test]
fn dep_query_basic() {
    block_on(async {
        let loc = make_test_ctx(20);
        let ctx = gs(&loc);

        let mut invite_sg: InviteSG = InviteSG::new();
        let mut invite_events: EventStore<u64> = EventStore::new();
        invite_events.insert(1, (0, 100));
        let ih =
            async |user_id: &u64,
                   ctx: &mut HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool, _>| {
                ctx.update(*user_id, true);
            };
        let mut ir = async |_: &u64| Timeline::<u64, bool> {
            writes: OrdMap::new(),
        };
        invite_sg
            .apply(
                &mut { ih },
                &make_resolver(&invite_events),
                &mut ir,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(1)],
                },
            )
            .await;
        assert_eq!(invite_sg.query(&100), Some(&true));

        let mut doc_sg: DocSG = DocSG::new();
        let mut doc_events: EventStore<&str> = EventStore::new();
        doc_events.insert(10, (0, "write"));

        let invite_writes = invite_sg.as_writes();
        let mut dep_resolver = async move |_: &u64| invite_writes.clone();
        let doc_handler =
            async |_ev: &&str, ctx: &mut HandlerCtx<u64, u64, bool, EmptyRuntime, &str, i32, _>| {
                if let Some(invited) = ctx.dep_query(&0, &100u64).await {
                    if invited {
                        ctx.update("content", 42);
                    }
                }
            };
        let mut dh = doc_handler;
        doc_sg
            .apply(
                &mut dh,
                &make_resolver(&doc_events),
                &mut dep_resolver,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(10)],
                },
            )
            .await;
        assert_eq!(doc_sg.query(&"content"), Some(&42));
    });
}

#[test]
fn dep_change_detection_and_propagation() {
    block_on(async {
        let loc = make_test_ctx(20);
        let ctx = gs(&loc);

        async fn doc_handler<D: async FnMut(&u64) -> Timeline<u64, bool>>(
            ev: &(u64, u64),
            ctx: &mut HandlerCtx<'_, u64, u64, bool, EmptyRuntime, &str, i32, D>,
        ) {
            let (branch, user) = *ev;
            if let Some(invited) = ctx.dep_query(&branch, &user).await {
                if invited {
                    ctx.update("content", (branch ^ user) as i32);
                }
            }
        }

        let mut invite_10: InviteSG = InviteSG::new();
        let mut ir = async |_: &u64| Timeline::<u64, bool> {
            writes: OrdMap::new(),
        };
        let mut doc_sg: DocSG = DocSG::new();
        let mut doc_events: EventStore<(u64, u64)> = EventStore::new();

        doc_events.insert(10, (0, (10, 5)));
        let mut w = invite_10.as_writes();
        {
            let mut dr = async move |_dep: &u64| w.clone();
            doc_sg
                .apply(
                    &mut doc_handler,
                    &make_resolver(&doc_events),
                    &mut dr,
                    &ctx,
                    &DeltaList {
                        removed: vec![],
                        added: vec![lid(10)],
                    },
                )
                .await;
        }
        assert_eq!(doc_sg.query(&"content"), None);

        let mut invite_events: EventStore<u64> = EventStore::new();
        invite_events.insert(5, (0, 5));
        let ih = async |user_id: &u64,
                        ctx: &mut HandlerCtx<
            '_,
            u64,
            u64,
            bool,
            EmptyRuntime,
            u64,
            bool,
            _,
        >| {
            ctx.update(*user_id, true);
        };
        invite_10
            .apply(
                &mut { ih },
                &make_resolver(&invite_events),
                &mut ir,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(5)],
                },
            )
            .await;

        w = invite_10.as_writes();
        let mut dr = async move |_dep: &u64| w.clone();
        doc_sg
            .apply(
                &mut doc_handler,
                &make_resolver(&doc_events),
                &mut dr,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![],
                },
            )
            .await;
        assert_eq!(doc_sg.query(&"content"), Some(&{ 10 ^ 5 }));
    });
}

#[test]
fn dep_isolation_between_branches() {
    block_on(async {
        let loc = make_test_ctx(20);
        let ctx = gs(&loc);

        let mut invite_10: InviteSG = InviteSG::new();
        let mut invite_20: InviteSG = InviteSG::new();
        let mut ir = async |_: &u64| Timeline::<u64, bool> {
            writes: OrdMap::new(),
        };
        let ih =
            async |user_id: &u64,
                   ctx: &mut HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool, _>| {
                ctx.update(*user_id, true);
            };

        let mut ev10: EventStore<u64> = EventStore::new();
        ev10.insert(1, (0, 5));
        invite_10
            .apply(
                &mut { ih },
                &make_resolver(&ev10),
                &mut ir,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(1)],
                },
            )
            .await;

        let mut ev20: EventStore<u64> = EventStore::new();
        ev20.insert(1, (0, 7));
        invite_20
            .apply(
                &mut { ih },
                &make_resolver(&ev20),
                &mut ir,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(1)],
                },
            )
            .await;

        let mut doc_sg: DocSG = DocSG::new();
        let mut doc_events: EventStore<(u64, u64)> = EventStore::new();
        async fn doc_handler<D: async FnMut(&u64) -> Timeline<u64, bool>>(
            ev: &(u64, u64),
            ctx: &mut HandlerCtx<'_, u64, u64, bool, EmptyRuntime, &str, i32, D>,
        ) {
            let (branch, user) = *ev;
            if let Some(invited) = ctx.dep_query(&branch, &user).await {
                if invited {
                    ctx.update("content", (branch ^ user) as i32);
                }
            }
        }
        doc_events.insert(10, (0, (10, 5)));
        doc_events.insert(11, (0, (20, 7)));

        let mut w10 = invite_10.as_writes();
        let mut w20 = invite_20.as_writes();
        {
            let mut dr = async move |dep: &u64| -> Timeline<u64, bool> {
                match *dep {
                    10 => w10.clone(),
                    20 => w20.clone(),
                    _ => Timeline {
                        writes: OrdMap::new(),
                    },
                }
            };
            doc_sg
                .apply(
                    &mut doc_handler,
                    &make_resolver(&doc_events),
                    &mut dr,
                    &ctx,
                    &DeltaList {
                        removed: vec![],
                        added: vec![lid(10), lid(11)],
                    },
                )
                .await;
        }
        assert_eq!(doc_sg.query(&"content"), Some(&{ 20 ^ 7 }));

        let revoke = async |user_id: &u64,
                            ctx: &mut HandlerCtx<
            '_,
            u64,
            u64,
            bool,
            EmptyRuntime,
            u64,
            bool,
            _,
        >| {
            ctx.update(*user_id, false);
        };
        ev10.insert(1, (0, 5));
        invite_10
            .apply(
                &mut { revoke },
                &make_resolver(&ev10),
                &mut ir,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![lid(1)],
                },
            )
            .await;

        w10 = invite_10.as_writes();
        w20 = invite_20.as_writes();
        let mut dr = async move |dep: &u64| -> Timeline<u64, bool> {
            match *dep {
                10 => w10.clone(),
                20 => w20.clone(),
                _ => Timeline {
                    writes: OrdMap::new(),
                },
            }
        };
        doc_sg
            .apply(
                &mut doc_handler,
                &make_resolver(&doc_events),
                &mut dr,
                &ctx,
                &DeltaList {
                    removed: vec![],
                    added: vec![],
                },
            )
            .await;
        assert_eq!(doc_sg.query_at(&"content", eid(0, 10), &ctx), None);
        assert_eq!(
            doc_sg.query_at(&"content", eid(0, 11), &ctx),
            Some(&{ 20 ^ 7 })
        );
    });
}
