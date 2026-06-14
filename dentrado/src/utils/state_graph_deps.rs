use super::{DeltaList, HandlerCtx, SGBucketId, SGEventId, StateGraph, Timeline};
use crate::core::gear::EmptyRuntime;
use crate::core::loc_ctx::{EventContext, LocCtx, StoredEvent};
use crate::types::{AnyLocEventId, GlobalCoreId, LocGroupId, SenderPk};
use im::OrdMap;
use std::collections::BTreeMap;

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

type DocSG = StateGraph<u64, u64, bool, &'static str, i32>;
type InviteSG = StateGraph<u64, u64, bool, u64, bool>;

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

fn invite_resolver() -> &'static dyn Fn(u64) -> Timeline<u64, bool> {
    &|_| Timeline {
        writes: OrdMap::new(),
    }
}

#[test]
fn dep_query_basic() {
    let ctx = make_test_ctx(20);

    let mut invite_sg: InviteSG = InviteSG::new();
    let mut invite_events: EventStore<u64> = EventStore::new();
    invite_events.insert(1, (0, 100));
    let ih = |user_id: &u64, ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool>| {
        ctx.update(*user_id, true);
    };
    let ir = invite_resolver();
    invite_sg.apply(
        &ih,
        &make_resolver(&invite_events),
        ir,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );
    assert_eq!(invite_sg.query(&100), Some(&true));

    let mut doc_sg: DocSG = DocSG::new();
    let mut doc_events: EventStore<&str> = EventStore::new();
    doc_events.insert(10, (0, "write"));

    let invite_writes = invite_sg.as_writes();
    let dep_resolver = |_: u64| invite_writes.clone();
    let doc_handler = |_ev: &&str, ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, &str, i32>| {
        if let Some(invited) = ctx.dep_query(&0, &100u64) {
            if invited {
                ctx.update("content", 42);
            }
        }
    };
    doc_sg.apply(
        &doc_handler,
        &make_resolver(&doc_events),
        &dep_resolver,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(10)],
        },
    );
    assert_eq!(doc_sg.query(&"content"), Some(&42));
}

#[test]
fn dep_change_detection_and_propagation() {
    let ctx = make_test_ctx(20);

    let mut invite_10: InviteSG = InviteSG::new();
    let ir = invite_resolver();
    let mut doc_sg: DocSG = DocSG::new();
    let mut doc_events: EventStore<(u64, u64)> = EventStore::new();
    let doc_handler =
        |ev: &(u64, u64), ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, &str, i32>| {
            let (branch, user) = *ev;
            if let Some(invited) = ctx.dep_query(&branch, &user) {
                if invited {
                    ctx.update("content", (branch ^ user) as i32);
                }
            }
        };

    doc_events.insert(10, (0, (10, 5)));
    let mut w = invite_10.as_writes();
    {
        let dr = |_dep: u64| w.clone();
        doc_sg.apply(
            &doc_handler,
            &make_resolver(&doc_events),
            &dr,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(10)],
            },
        );
    }
    assert_eq!(doc_sg.query(&"content"), None);

    let mut invite_events: EventStore<u64> = EventStore::new();
    invite_events.insert(5, (0, 5));
    let ih = |user_id: &u64, ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool>| {
        ctx.update(*user_id, true);
    };
    invite_10.apply(
        &ih,
        &make_resolver(&invite_events),
        ir,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(5)],
        },
    );

    w = invite_10.as_writes();
    let dr = |_dep: u64| w.clone();
    doc_sg.apply(
        &doc_handler,
        &make_resolver(&doc_events),
        &dr,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![],
        },
    );
    assert_eq!(doc_sg.query(&"content"), Some(&{ 10 ^ 5 }));
}

#[test]
fn dep_isolation_between_branches() {
    let ctx = make_test_ctx(20);

    let mut invite_10: InviteSG = InviteSG::new();
    let mut invite_20: InviteSG = InviteSG::new();
    let ir = invite_resolver();
    let ih = |user_id: &u64, ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool>| {
        ctx.update(*user_id, true);
    };

    let mut ev10: EventStore<u64> = EventStore::new();
    ev10.insert(1, (0, 5));
    invite_10.apply(
        &ih,
        &make_resolver(&ev10),
        ir,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );

    let mut ev20: EventStore<u64> = EventStore::new();
    ev20.insert(1, (0, 7));
    invite_20.apply(
        &ih,
        &make_resolver(&ev20),
        ir,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );

    let mut doc_sg: DocSG = DocSG::new();
    let mut doc_events: EventStore<(u64, u64)> = EventStore::new();
    let doc_handler =
        |ev: &(u64, u64), ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, &str, i32>| {
            let (branch, user) = *ev;
            if let Some(invited) = ctx.dep_query(&branch, &user) {
                if invited {
                    ctx.update("content", (branch ^ user) as i32);
                }
            }
        };
    doc_events.insert(10, (0, (10, 5)));
    doc_events.insert(11, (0, (20, 7)));

    let mut w10 = invite_10.as_writes();
    let mut w20 = invite_20.as_writes();
    {
        let dr = |dep: u64| -> Timeline<u64, bool> {
            match dep {
                10 => w10.clone(),
                20 => w20.clone(),
                _ => Timeline {
                    writes: OrdMap::new(),
                },
            }
        };
        doc_sg.apply(
            &doc_handler,
            &make_resolver(&doc_events),
            &dr,
            &ctx,
            &DeltaList {
                removed: vec![],
                added: vec![lid(10), lid(11)],
            },
        );
    }
    assert_eq!(doc_sg.query(&"content"), Some(&{ 20 ^ 7 }));

    let revoke = |user_id: &u64, ctx: &HandlerCtx<u64, u64, bool, EmptyRuntime, u64, bool>| {
        ctx.update(*user_id, false);
    };
    ev10.insert(1, (0, 5));
    invite_10.apply(
        &revoke,
        &make_resolver(&ev10),
        ir,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![lid(1)],
        },
    );

    w10 = invite_10.as_writes();
    w20 = invite_20.as_writes();
    let dr = |dep: u64| -> Timeline<u64, bool> {
        match dep {
            10 => w10.clone(),
            20 => w20.clone(),
            _ => Timeline {
                writes: OrdMap::new(),
            },
        }
    };
    doc_sg.apply(
        &doc_handler,
        &make_resolver(&doc_events),
        &dr,
        &ctx,
        &DeltaList {
            removed: vec![],
            added: vec![],
        },
    );
    assert_eq!(doc_sg.query_at(&"content", eid(0, 10), &ctx), None);
    assert_eq!(
        doc_sg.query_at(&"content", eid(0, 11), &ctx),
        Some(&{ 20 ^ 7 })
    );
}
