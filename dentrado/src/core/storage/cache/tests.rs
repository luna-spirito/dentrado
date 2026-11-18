//! The cache-backed layers 1–2 against a real journal: replay, a warm
//! restart, a lost cache, and a replaced log.

use std::future::Future;

use super::*;
use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::StoredEvent,
        storage::{IsStorage, Watermark, journal::schema},
    },
    types::{
        DataId, GroupEventId, IdentityServerPk, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId,
        LocUserId, NodeId, SenderPk, UserId,
    },
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

const UID: UserId = UserId {
    id: 7,
    identity_server_pk: IdentityServerPk([9; 32]),
};
const PK: SenderPk = SenderPk([3; 32]);

struct Fixture {
    user: LocUserId,
    sender: LocSenderId,
    group: LocGroupId,
    data: LocDataId,
    data_id: DataId,
}

fn body() -> StoredEvent<()> {
    StoredEvent {
        sender: LocSenderId(0),
        tx_id: 1,
        timestamp: 10,
        source_node: NodeId(1),
        body: (),
    }
}

async fn write(st: &CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk>) -> Fixture {
    let user = st.mk_loc_user(UID).await;
    let sender = st.mk_loc_sender(PK, Some(UID)).await;
    let data_id = DataId {
        timestamp: 42,
        hash: [0; 32],
    };
    let data = st.mk_data(data_id, ()).await.expect("intern the data");
    let group = st.mk_loc_group(LocMsgTypeId(1), ()).await;
    let mut ev = body();
    ev.sender = sender;
    assert_eq!(
        st.store_event(group, ev).await.expect("a new event").added,
        Some(GroupEventId(0))
    );
    st.flush().await.expect("flush");
    Fixture {
        user,
        sender,
        group,
        data,
        data_id,
    }
}

async fn verify(st: &CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk>, fixture: &Fixture) {
    assert_eq!(st.user_by_local(fixture.user).await, Some(UID));
    assert_eq!(st.sender_user(fixture.sender).await, Some(fixture.user));
    assert_eq!(st.sender_pk(fixture.sender).await, Some(PK));
    assert_eq!(st.find_data(&fixture.data_id).await, Some(fixture.data));
    assert_eq!(
        st.fetch_data(fixture.data).await,
        Some((fixture.data_id, ()))
    );
    let ev = st
        .fetch_event(fixture.group, GroupEventId(0))
        .await
        .expect("the live slot");
    assert_eq!(ev.sender, fixture.sender);
    assert_eq!(ev.tx_id, 1);
    assert_eq!(
        st.diff_group(fixture.group, Watermark::default())
            .await
            .added,
        [GroupEventId(0)]
    );
}

#[test]
fn a_warm_cache_restarts_without_visiting_the_journal() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core.dstore");
        let fixture = {
            let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
                CacheStorage::on_disk(file.clone());
            write(&st).await
        };

        let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
            CacheStorage::on_disk(file);
        assert_eq!(st.replayed().await, 0, "the image is complete");
        verify(&st, &fixture).await;

        // The slot counter came back from the cache, not from the journal.
        let next = StoredEvent {
            sender: fixture.sender,
            tx_id: 2,
            timestamp: 20,
            source_node: NodeId(3),
            body: (),
        };
        assert_eq!(
            st.store_event(fixture.group, next).await.unwrap().added,
            Some(GroupEventId(1))
        );
    });
}

#[test]
fn a_lost_cache_is_refilled_from_the_journal() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core.dstore");
        let fixture = {
            let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
                CacheStorage::on_disk(file.clone());
            write(&st).await
        };
        std::fs::remove_file(&file).unwrap();

        let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
            CacheStorage::on_disk(file);
        assert!(st.replayed().await > 0, "the journal was replayed");
        verify(&st, &fixture).await;
    });
}

#[test]
fn a_replaced_journal_wipes_the_stale_cache() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core.dstore");
        let fixture = {
            let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
                CacheStorage::on_disk(file.clone());
            write(&st).await
        };

        // A different log: its identity does not match the cache's meta.
        std::fs::remove_dir_all(file.with_extension("journal")).unwrap();

        let st: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
            CacheStorage::on_disk(file);
        assert_eq!(st.replayed().await, 0, "the new log is empty");
        assert_eq!(
            st.user_by_local(fixture.user).await,
            None,
            "old loc ids do not survive a different log"
        );
        let fresh = StoredEvent {
            sender: st.mk_loc_sender(PK, Some(UID)).await,
            tx_id: 1,
            timestamp: 1,
            source_node: NodeId(1),
            body: (),
        };
        let group = st.mk_loc_group(LocMsgTypeId(1), ()).await;
        assert_eq!(
            st.store_event(group, fresh).await.unwrap().added,
            Some(GroupEventId(0))
        );
    });
}

#[test]
fn replay_refuses_a_foreign_record() {
    run(async {
        use crate::types::Shape;
        use dentrado_journal::{Decl, Frame, Journal};

        let dir = tempfile::tempdir().unwrap();
        let journal_dir = dir.path().join("core.journal");
        let mut journal = Journal::open(&journal_dir).await.unwrap();
        let shape = u64::shape();
        let foreign = journal
            .intern(Decl {
                name: "test/foreign",
                shape: &shape,
            })
            .await
            .unwrap();
        journal
            .append(&[Frame::new(foreign, &7u64.to_le_bytes())])
            .await
            .unwrap();
        journal.sync().await.unwrap();
        drop(journal);

        let cache: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
            CacheStorage::on_disk(dir.path().join("core.dstore"));
        let err = cache.open().await.err().expect("the open must fail");
        assert!(err.contains("is not a dentrado record"), "{err}");
    });
}

#[test]
fn a_nested_layout_mismatch_is_refused() {
    run(async {
        use dentrado_journal::Journal;

        let dir = tempfile::tempdir().unwrap();
        let journal_dir = dir.path().join("core.journal");
        let mut journal = Journal::open(&journal_dir).await.unwrap();
        let schemas = schema::Schemas::declare::<TestRuntime>(&mut journal)
            .await
            .unwrap();
        let wrong = {
            use crate::types::Shape;
            use dentrado_journal::Decl;
            let shape = u64::shape();
            journal
                .intern(Decl {
                    name: "test/group",
                    shape: &shape,
                })
                .await
                .unwrap()
        };
        let record = schema::encode(&crate::core::storage::journal::record::LocGroup {
            loc: 0,
            msg_type: 0,
            group_schema: wrong.0,
            group: 7u64.to_le_bytes().to_vec(),
        })
        .unwrap();
        journal
            .append(&[dentrado_journal::Frame::new(schemas.group, &record)])
            .await
            .unwrap();
        journal.sync().await.unwrap();
        drop(journal);

        let cache: CacheStorage<TestRuntime, dentrado_cache::disk::FsDisk> =
            CacheStorage::on_disk(dir.path().join("core.dstore"));
        let err = cache.open().await.err().expect("the open must fail");
        assert!(err.contains("has shape"), "{err}");
    });
}
