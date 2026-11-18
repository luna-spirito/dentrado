//! `PagedStorage` in journal mode, end to end: localization and events land in
//! the journal, the fold rebuilds them on reopen, event bodies come back from
//! the cache, and the gear views in the same page store survive too.
//!
//! A cold start with the cache deleted is the strongest check of the split:
//! the journal is the source of truth, so losing the whole cache must cost
//! nothing but the re-materialization work.

use dentrado::{
    core::{
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime, RuntimeTypes},
        loc_ctx::StoredEvent,
        storage::{GearView, IsStorage, PagedStorage, Watermark},
    },
    types::*,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
enum TestGroup {
    /// Routes through a data loc id: `global_hash` needs the fold's resolver.
    Branch(LocDataId),
    Doc(u64),
}

impl Localizable for TestGroup {
    async fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(match self {
            Self::Branch(data) => Self::Branch(data.localize(remapper).await?),
            Self::Doc(doc) => Self::Doc(doc),
        })
    }
}

impl GlobalHash for TestGroup {
    fn global_hash(&self, resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        match self {
            Self::Branch(data) => {
                hasher.update(&data.global_hash(resolver)?);
            }
            Self::Doc(doc) => {
                hasher.update(&doc.to_le_bytes());
            }
        }
        Ok(*hasher.finalize().as_bytes())
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize, Shape)]
struct TestBody {
    text: String,
    revision: u64,
}

impl Localizable for TestBody {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize, Shape)]
struct TestData {
    creator: LocUserId,
}

impl Localizable for TestData {
    async fn localize<Rm: Remapper>(self, remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(Self {
            creator: self.creator.localize(remapper).await?,
        })
    }
}

impl GlobalHash for TestData {
    fn global_hash(&self, resolver: &dyn GlobalResolver) -> Result<[u8; 32], GroupRouteError> {
        let creator = resolver.resolve_user(self.creator)?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(&creator.id.to_le_bytes());
        hasher.update(&creator.identity_server_pk.0);
        Ok(*hasher.finalize().as_bytes())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum TestGear {
    Note { key: u64 },
}

impl Localizable for TestGear {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Debug)]
struct TestRuntime;

impl RuntimeTypes for TestRuntime {
    type GearId = TestGear;
    type GearOut = ();
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = TestGroup;
    type Body = TestBody;
    type Data = TestData;

    fn gear_kind(_gear: &Self::GearId) -> &'static str {
        "Note"
    }

    fn gear_kinds() -> &'static [&'static str] {
        &["Note"]
    }
}

impl IsRuntime for TestRuntime {
    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            TestGear::Note { key } => GearMeta::Event {
                msg_type: LocMsgTypeId(1),
                group: TestGroup::Doc(*key),
            },
        }
    }

    async fn run_step<S: IsStorage<Self>>(
        _ctx: &mut GearCtx<Self, S>,
        _input: GearInput<Self>,
        _view: &mut S::View<'_>,
    ) -> GearProduce<Self> {
        unreachable!("this test drives storage directly")
    }
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
    branch_hash: [u8; 32],
}

fn body(text: &str, revision: u64) -> TestBody {
    TestBody {
        text: text.to_owned(),
        revision,
    }
}

/// Writes one user, sender, group, data value, event and gear view, then
/// flushes.
async fn write(st: &PagedStorage<TestRuntime>) -> Fixture {
    let user = st.mk_loc_user(UID).await;
    let sender = st.mk_loc_sender(PK, Some(UID)).await;

    let data_id = DataId {
        timestamp: 42,
        hash: st
            .global_hash(&TestData { creator: user })
            .await
            .expect("hash the data"),
    };
    let data = st
        .mk_data(data_id, TestData { creator: user })
        .await
        .expect("intern the data");

    let group = st
        .mk_loc_group(LocMsgTypeId(1), TestGroup::Branch(data))
        .await;
    assert_eq!(
        st.mk_loc_group(LocMsgTypeId(1), TestGroup::Branch(data))
            .await,
        group,
        "group localization is idempotent"
    );

    let first = StoredEvent {
        sender,
        tx_id: 1,
        timestamp: 10,
        source_node: NodeId(1),
        body: body("one", 1),
    };
    let inserted = st.store_event(group, first).await.expect("a new event");
    assert_eq!(inserted.added, Some(GroupEventId(0)));
    assert_eq!(inserted.removed, None);

    let branch_hash = st
        .global_hash(&TestGroup::Branch(data))
        .await
        .expect("route through the data loc id");

    let mut view = st.gear_view(&TestGear::Note { key: 1 }).await;
    view.put(b"note", b"gear-view value");
    view.commit();

    st.flush().await.expect("flush the journal and the cache");

    Fixture {
        user,
        sender,
        group,
        data,
        data_id,
        branch_hash,
    }
}

/// Read every layer back and check it against what `write` recorded.
async fn verify(st: &PagedStorage<TestRuntime>, fixture: &Fixture) {
    verify_journal(st, fixture).await;
    let mut view = st.gear_view(&TestGear::Note { key: 1 }).await;
    assert_eq!(
        view.get(b"note").await.as_deref(),
        Some(&b"gear-view value"[..])
    );
}

/// The journal-side layers: localization, data, events, changelog.
async fn verify_journal(st: &PagedStorage<TestRuntime>, fixture: &Fixture) {
    assert_eq!(st.user_by_local(fixture.user).await, Some(UID));
    assert_eq!(st.sender_user(fixture.sender).await, Some(fixture.user));
    assert_eq!(st.sender_pk(fixture.sender).await, Some(PK));
    assert_eq!(st.find_data(&fixture.data_id).await, Some(fixture.data));
    assert_eq!(
        st.fetch_data(fixture.data).await,
        Some((
            fixture.data_id,
            TestData {
                creator: fixture.user
            }
        ))
    );
    assert_eq!(
        st.global_hash(&TestGroup::Branch(fixture.data)).await,
        Ok(fixture.branch_hash),
        "the fold still resolves the group's data loc id"
    );

    // The body is still fetchable after a reopen; a slot is never deleted.
    let latest = st
        .fetch_event(fixture.group, GroupEventId(0))
        .await
        .expect("the live slot");
    assert_eq!(latest.body, body("one", 1));
    assert!(
        st.fetch_event(fixture.group, GroupEventId(1))
            .await
            .is_none(),
        "no other observation became a slot"
    );

    let diff = st.diff_group(fixture.group, Watermark::default()).await;
    assert_eq!(diff.added, [GroupEventId(0)]);
    assert!(diff.removed.is_empty());
    assert_eq!(
        diff.watermark,
        Watermark {
            added: 1,
            removed: 0
        }
    );

    let tail = st
        .diff_group(
            fixture.group,
            Watermark {
                added: 1,
                removed: 0,
            },
        )
        .await;
    assert!(tail.added.is_empty());
    assert!(tail.removed.is_empty());
    assert_eq!(tail.watermark, diff.watermark);
}

fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .expect("a runtime")
        .block_on(f)
}

#[test]
fn a_journaled_store_reopens_with_every_layer_intact() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core-0.dstore");
        let journal = file.with_extension("journal");

        let fixture = {
            let st: PagedStorage<TestRuntime> = PagedStorage::on_disk(file.clone());
            let fixture = write(&st).await;
            verify(&st, &fixture).await;
            fixture
        };
        assert!(journal.is_dir(), "the journal lives beside the cache");
        assert!(file.is_file(), "the cache image was written");

        let st: PagedStorage<TestRuntime> = PagedStorage::on_disk(file.clone());
        verify(&st, &fixture).await;

        // A fresh event lands after the replayed ones: the dedup index and the
        // slot counter came back from the fold, not from the cache.
        let next = StoredEvent {
            sender: fixture.sender,
            tx_id: 2,
            timestamp: 20,
            source_node: NodeId(3),
            body: body("next", 2),
        };
        let inserted = st
            .store_event(fixture.group, next)
            .await
            .expect("a new key");
        assert_eq!(inserted.added, Some(GroupEventId(1)));
        assert_eq!(inserted.removed, None);
        assert_eq!(
            st.fetch_event(fixture.group, GroupEventId(1))
                .await
                .map(|ev| ev.body),
            Some(body("next", 2))
        );
        st.flush().await.unwrap();
    });
}

#[test]
fn the_journal_alone_rebuilds_a_lost_cache() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core-0.dstore");

        let fixture = {
            let st: PagedStorage<TestRuntime> = PagedStorage::on_disk(file.clone());
            write(&st).await
        };

        // The cache is recomputable: losing the whole image costs the gear
        // views (a gear rebuilds them on its next run) but not localization,
        // events, or their bodies.
        std::fs::remove_file(&file).unwrap();

        let st: PagedStorage<TestRuntime> = PagedStorage::on_disk(file);
        verify_journal(&st, &fixture).await;
        let mut view = st.gear_view(&TestGear::Note { key: 1 }).await;
        assert!(
            view.get(b"note").await.is_none(),
            "layer 3 is cache: a new image starts a gear from scratch"
        );
    });
}

/// Supersede is a `todo!()`: the decided tiebreak needs a group's target
/// homeserver, which nothing records yet. This pins the delta it must produce
/// once observations can be ranked by authority.
#[test]
#[ignore = "supersede is a todo!(); see Fold::store_event"]
fn a_superseding_observation_removes_the_stored_slot() {
    run(async {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("core-0.dstore");
        let st: PagedStorage<TestRuntime> = PagedStorage::on_disk(file);
        let fixture = write(&st).await;

        // Same `(sender, tx_id)`, earlier timestamp: the earlier observation
        // lands in a new slot, the stored one becomes a `removed` entry.
        let earlier = StoredEvent {
            sender: fixture.sender,
            tx_id: 1,
            timestamp: 5,
            source_node: NodeId(2),
            body: body("earlier", 0),
        };
        let delta = st
            .store_event(fixture.group, earlier)
            .await
            .expect("a supersede");
        assert_eq!(delta.added, Some(GroupEventId(1)));
        assert_eq!(delta.removed, Some(GroupEventId(0)));

        let diff = st.diff_group(fixture.group, Watermark::default()).await;
        assert_eq!(diff.added, [GroupEventId(0), GroupEventId(1)]);
        assert_eq!(diff.removed, [GroupEventId(0)]);
        assert_eq!(
            diff.watermark,
            Watermark {
                added: 2,
                removed: 1
            }
        );
    });
}
