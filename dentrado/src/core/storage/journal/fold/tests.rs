use std::future::Future;

use dentrado_journal::Decl;

use super::*;
use crate::{
    core::{
        api::RuntimeTypes,
        storage::{Watermark, journal::schema},
    },
    types::{GroupEventId, IdentityServerPk},
};

#[derive(Debug)]
struct TestRuntime;

impl RuntimeTypes for TestRuntime {
    type GearId = ();
    type GearOut = ();
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = ();
    type Body = ();
    type Data = ();

    fn gear_kind(_gear: &Self::GearId) -> &'static str {
        "test"
    }

    fn gear_kinds() -> &'static [&'static str] {
        &["test"]
    }
}

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

async fn intern<T: Shape>(journal: &mut Journal, name: &str) -> SchemaId {
    let shape = T::shape();
    journal
        .intern(Decl {
            name,
            shape: &shape,
        })
        .await
        .unwrap()
}

async fn append<T: serde::Serialize>(journal: &mut Journal, schema: SchemaId, record: &T) {
    let bytes = schema::encode(record).unwrap();
    journal.append(&[Frame::new(schema, &bytes)]).await.unwrap();
}

async fn append_event(
    journal: &mut Journal,
    schema: SchemaId,
    group: LocGroupId,
    ev: &StoredEvent<()>,
    body_schema: SchemaId,
) {
    let body = postcard::to_stdvec(&()).unwrap();
    let record = Event {
        group: group.0,
        sender: ev.sender.0,
        tx_id: ev.tx_id,
        timestamp: ev.timestamp,
        source_node: ev.source_node.0,
        body_schema: body_schema.0,
        body: &body,
    };
    let bytes = record.encode();
    journal.append(&[Frame::new(schema, &bytes)]).await.unwrap();
}

const UID: UserId = UserId {
    id: 7,
    identity_server_pk: IdentityServerPk([9; 32]),
};
const PK: SenderPk = SenderPk([3; 32]);

/// A journal holding one user, sender, group and data value, plus the live
/// fold that recorded them — the starting point for the replay tests.
struct Seeded {
    journal: Journal,
    schemas: schema::Schemas,
    live: Fold<TestRuntime, ()>,
    value_schema: SchemaId,
    user: LocUserId,
    sender: LocSenderId,
    group: LocGroupId,
    data: LocDataId,
    data_id: DataId,
}

async fn seed(dir: &std::path::Path) -> Seeded {
    let mut journal = Journal::open(dir).await.unwrap();
    let schemas = schema::Schemas::declare::<TestRuntime>(&mut journal)
        .await
        .unwrap();
    let value_schema = schemas.group_value;
    let mut live = Fold::<TestRuntime, ()>::new();

    let user = live.mk_loc_user(UID);
    append(
        &mut journal,
        schemas.user,
        &LocUser {
            loc: user.0,
            id: UID.id,
            identity_server_pk: UID.identity_server_pk.0,
        },
    )
    .await;

    let sender = live.mk_loc_sender(PK, Some(UID));
    append(
        &mut journal,
        schemas.sender,
        &LocSender {
            loc: sender.0,
            pk: PK.0,
            user: Some(user.0),
        },
    )
    .await;

    let msg_type = LocMsgTypeId(5);
    let group = live.mk_loc_group(msg_type, ());
    append(
        &mut journal,
        schemas.group,
        &LocGroup {
            loc: group.0,
            msg_type: msg_type.0,
            group_schema: value_schema.0,
            group: postcard::to_stdvec(&()).unwrap(),
        },
    )
    .await;

    let data_id = DataId {
        timestamp: 42,
        hash: [0; 32],
    };
    let data = live.mk_data(data_id, ()).unwrap();
    append(
        &mut journal,
        schemas.data,
        &LocData {
            loc: data.0,
            timestamp: data_id.timestamp,
            hash: data_id.hash,
            content_schema: value_schema.0,
            content: postcard::to_stdvec(&()).unwrap(),
        },
    )
    .await;

    Seeded {
        journal,
        schemas,
        live,
        value_schema,
        user,
        sender,
        group,
        data,
        data_id,
    }
}

/// The fold replay builds is the fold the live path built: same loc ids,
/// same dedup, same slots, same changelog.
#[test]
fn replay_rebuilds_the_live_fold() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let Seeded {
            mut journal,
            schemas,
            mut live,
            value_schema,
            user,
            sender,
            group,
            data,
            data_id,
        } = seed(dir.path()).await;

        let first = StoredEvent {
            sender,
            tx_id: 1,
            timestamp: 10,
            source_node: NodeId(1),
            body: (),
        };
        append_event(&mut journal, schemas.event, group, &first, value_schema).await;
        let delta = live.store_event(group, first).expect("a new event");
        assert_eq!(delta.added, Some(GroupEventId(0)));
        assert_eq!(delta.removed, None);

        journal.sync().await.unwrap();
        journal.identity().await.unwrap();
        drop(journal);
        let reopened = Journal::open(dir.path()).await.unwrap();

        let mut rebuilt = Fold::<TestRuntime, ()>::new();
        let at = replay(
            &reopened,
            &schemas,
            Cursor::START,
            &mut rebuilt,
            |_, _, _| {},
        )
        .await
        .unwrap();
        assert_eq!(at, reopened.head());

        assert_eq!(rebuilt.user_by_local(user), Some(UID));
        assert_eq!(rebuilt.sender_pk(sender), Some(PK));
        assert_eq!(rebuilt.sender_user(sender), Some(user));
        assert_eq!(rebuilt.find_data(&data_id), Some(data));
        assert_eq!(rebuilt.fetch_data(data), Some((data_id, ())));

        let live_diff = live.diff_group(group, Watermark::default());
        let rebuilt_diff = rebuilt.diff_group(group, Watermark::default());
        assert_eq!(live_diff.added, rebuilt_diff.added);
        assert_eq!(live_diff.removed, rebuilt_diff.removed);
        assert_eq!(live_diff.watermark, rebuilt_diff.watermark);
        assert_eq!(rebuilt_diff.added, vec![GroupEventId(0)]);
        assert!(rebuilt_diff.removed.is_empty());
        assert_eq!(
            rebuilt_diff.watermark,
            Watermark {
                added: 1,
                removed: 0
            }
        );

        let live_ev = live.fetch_event(group, GroupEventId(0)).unwrap();
        let rebuilt_ev = rebuilt.fetch_event(group, GroupEventId(0)).unwrap();
        assert_eq!(live_ev.sender, rebuilt_ev.sender);
        assert_eq!(live_ev.tx_id, rebuilt_ev.tx_id);
        assert_eq!(live_ev.timestamp, rebuilt_ev.timestamp);
        assert_eq!(live_ev.source_node, rebuilt_ev.source_node);
        assert!(
            rebuilt.fetch_event(group, GroupEventId(1)).is_none(),
            "no second observation became a slot"
        );
    });
}

/// Supersede is a `todo!()`. This pins the replay it must produce once
/// observations can be ranked: the earlier observation lands in a new slot,
/// the stored one becomes a `removed` entry, and both bodies stay fetchable.
#[test]
#[ignore = "supersede is a todo!(); see Fold::store_event"]
fn replay_restores_a_superseded_slot() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let Seeded {
            mut journal,
            schemas,
            mut live,
            value_schema,
            sender,
            group,
            ..
        } = seed(dir.path()).await;

        let first = StoredEvent {
            sender,
            tx_id: 1,
            timestamp: 10,
            source_node: NodeId(1),
            body: (),
        };
        append_event(&mut journal, schemas.event, group, &first, value_schema).await;
        assert!(live.store_event(group, first).is_some());

        // Earlier observation of the same `(sender, tx_id)` supersedes.
        let earlier = StoredEvent {
            sender,
            tx_id: 1,
            timestamp: 5,
            source_node: NodeId(2),
            body: (),
        };
        append_event(&mut journal, schemas.event, group, &earlier, value_schema).await;
        let delta = live.store_event(group, earlier).unwrap();
        assert_eq!(delta.added, Some(GroupEventId(1)));
        assert_eq!(delta.removed, Some(GroupEventId(0)));

        journal.sync().await.unwrap();
        drop(journal);
        let reopened = Journal::open(dir.path()).await.unwrap();

        let mut rebuilt = Fold::<TestRuntime, ()>::new();
        replay(
            &reopened,
            &schemas,
            Cursor::START,
            &mut rebuilt,
            |_, _, _| {},
        )
        .await
        .unwrap();

        let diff = rebuilt.diff_group(group, Watermark::default());
        assert_eq!(diff.added, vec![GroupEventId(0), GroupEventId(1)]);
        assert_eq!(diff.removed, vec![GroupEventId(0)]);
        assert_eq!(
            diff.watermark,
            Watermark {
                added: 2,
                removed: 1
            }
        );
    });
}

#[test]
fn replay_refuses_a_foreign_record() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).await.unwrap();
        let schemas = schema::Schemas::declare::<TestRuntime>(&mut journal)
            .await
            .unwrap();
        let foreign = intern::<u64>(&mut journal, "test/foreign").await;
        append(&mut journal, foreign, &7u64).await;

        let mut fold = Fold::<TestRuntime, ()>::new();
        let err = replay(&journal, &schemas, Cursor::START, &mut fold, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ReplayError::Visit(FoldError::UnexpectedRecord { id, .. }) if id == foreign.0
            ),
            "{err:?}"
        );
    });
}

#[test]
fn replay_refuses_a_nested_layout_mismatch() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).await.unwrap();
        let schemas = schema::Schemas::declare::<TestRuntime>(&mut journal)
            .await
            .unwrap();
        let wrong = intern::<u64>(&mut journal, "test/group").await;
        append(
            &mut journal,
            schemas.group,
            &LocGroup {
                loc: 0,
                msg_type: 0,
                group_schema: wrong.0,
                group: 7u64.to_le_bytes().to_vec(),
            },
        )
        .await;

        let mut fold = Fold::<TestRuntime, ()>::new();
        let err = replay(&journal, &schemas, Cursor::START, &mut fold, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ReplayError::Visit(FoldError::WrongShape { what: "group", .. })
            ),
            "{err:?}"
        );
    });
}

#[test]
fn replay_refuses_a_loc_gap() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(dir.path()).await.unwrap();
        let schemas = schema::Schemas::declare::<TestRuntime>(&mut journal)
            .await
            .unwrap();
        append(
            &mut journal,
            schemas.user,
            &LocUser {
                loc: 3,
                id: 7,
                identity_server_pk: [9; 32],
            },
        )
        .await;

        let mut fold = Fold::<TestRuntime, ()>::new();
        let err = replay(&journal, &schemas, Cursor::START, &mut fold, |_, _, _| {})
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                ReplayError::Visit(FoldError::Restore {
                    source: RestoreError::OutOfOrder { kind: "user", .. },
                    ..
                })
            ),
            "{err:?}"
        );
    });
}
