//! `PagedStorage` gear-view round-trip: staged writes become visible only
//! after `commit`, survive flush + reopen, overwrite and delete behave, and
//! `scan` walks one gear's prefix without leaking another gear's records.
//!
//! The base view is flat — a value must fit one leaf record — so a gear
//! that needs bigger values stores several records itself. This test only
//! exercises the flat contract.

use std::{fmt::Debug, panic::AssertUnwindSafe};

use dentrado::{
    core::{
        api::GEAR_TABLE_PREFIX,
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime, RuntimeTypes},
        storage::{GearView, IsStorage, PagedStorage},
    },
    types::*,
};

/// The two gear kinds this runtime declares, one table each (a manual
/// stand-in for what `#[gears]` generates).
const BLOB_TABLE: &[u8] = b"dentrado/gear-view/0/Blob";
const NOTE_TABLE: &[u8] = b"dentrado/gear-view/0/Note";

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum BlobGear {
    Blob { key: u64 },
    Note { key: u64 },
}

impl Localizable for BlobGear {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Debug)]
struct BlobRuntime;

impl RuntimeTypes for BlobRuntime {
    type GearId = BlobGear;
    type GearOut = ();
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = i64;
    type Body = ();
    type Data = ();

    fn gear_table(gear: &Self::GearId) -> &'static [u8] {
        match gear {
            BlobGear::Blob { .. } => BLOB_TABLE,
            BlobGear::Note { .. } => NOTE_TABLE,
        }
    }

    fn gear_tables() -> &'static [&'static [u8]] {
        &[BLOB_TABLE, NOTE_TABLE]
    }
}

impl IsRuntime for BlobRuntime {
    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        let (msg_type, key) = match gear {
            BlobGear::Blob { key } => (1, key),
            BlobGear::Note { key } => (2, key),
        };
        GearMeta::Event {
            msg_type: LocMsgTypeId(msg_type),
            group: i64::try_from(*key).unwrap(),
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

async fn roundtrip(file: &std::path::Path) {
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let gear = BlobGear::Blob { key: 7 };

    let mut view = st.gear_view(&gear).await;
    assert!(view.get(b"k").await.is_none());

    // A write is staged: the gear's reads still see the old world until
    // `commit` publishes the batch.
    view.put(b"k", b"one");
    assert!(view.get(b"k").await.is_none());
    view.commit();
    assert_eq!(view.get(b"k").await.unwrap(), b"one");

    // A value may fill most of a leaf record (the view does not chunk).
    let big = vec![b'x'; 4000];
    view.put(b"big", &big);
    view.commit();
    st.flush().await.unwrap();
    assert_eq!(view.get(b"big").await.unwrap(), big);

    // Reopen from disk: the record is fetched back on demand.
    drop(view);
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let mut view = st.gear_view(&gear).await;
    assert_eq!(view.get(b"k").await.unwrap(), b"one");
    assert_eq!(view.get(b"big").await.unwrap(), big);

    // Overwrite, reopen: the new value wins.
    view.put(b"k", b"two");
    view.commit();
    st.flush().await.unwrap();
    drop(view);
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let mut view = st.gear_view(&gear).await;
    assert_eq!(view.get(b"k").await.unwrap(), b"two");

    // `scan` returns this gear's records under the prefix, in key order,
    // with the gear prefix stripped.
    view.put(b"row/1", b"a");
    view.put(b"row/2", b"b");
    view.commit();
    assert_eq!(
        view.scan(b"row/").await,
        [
            (b"row/1".to_vec(), b"a".to_vec()),
            (b"row/2".to_vec(), b"b".to_vec()),
        ]
    );

    // Another gear is a separate namespace over the same store.
    let other = BlobGear::Blob { key: 8 };
    let mut other_view = st.gear_view(&other).await;
    assert!(other_view.get(b"k").await.is_none());
    assert!(other_view.scan(b"").await.is_empty());

    // `del` stages a deletion; it lands with the commit.
    view.del(b"k");
    assert_eq!(view.get(b"k").await.unwrap(), b"two");
    view.commit();
    assert!(view.get(b"k").await.is_none());
    st.flush().await.unwrap();
    drop(other_view);
    drop(view);
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let mut view = st.gear_view(&gear).await;
    assert!(view.get(b"k").await.is_none());
    assert_eq!(view.get(b"big").await.unwrap(), big);

    // RAM mode (`file = None`) is the same contract over a `BTreeMap`.
    let ram: PagedStorage<BlobRuntime> = PagedStorage::ram();
    let mut view = ram.gear_view(&gear).await;
    view.put(b"k", b"z");
    assert!(view.get(b"k").await.is_none());
    view.commit();
    assert_eq!(view.get(b"k").await.unwrap(), b"z");
}

#[test]
fn paged_storage_persists_and_resumes() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            roundtrip(&dir.path().join("core-0.dstore")).await;
        });
}

/// A corrupt image is quarantined (renamed aside, kept for inspection) and a
/// fresh store opens in its place — corruption neither crashloops the process
/// nor gets silently rewritten.
#[test]
fn paged_storage_quarantines_corruption() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("core-0.dstore");
            // Two garbage pages where the SB slots live: content without a
            // valid image.
            std::fs::write(&file, [0x5Au8; 8192]).unwrap();
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.clone());
            let gear = BlobGear::Blob { key: 7 };
            let mut view = st.gear_view(&gear).await;
            assert!(view.get(b"k").await.is_none());
            view.put(b"k", b"fresh");
            view.commit();
            st.flush().await.unwrap();
            drop(view);
            drop(st);
            // The corrupt file was renamed aside; the new one is a working
            // store.
            let quarantined: Vec<_> = std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|e| {
                    let name = e.unwrap().file_name().into_string().ok()?;
                    name.starts_with("core-0.dstore.corrupt-").then_some(name)
                })
                .collect();
            assert_eq!(quarantined.len(), 1, "the corrupt file was renamed aside");
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file);
            let mut view = st.gear_view(&gear).await;
            assert_eq!(view.get(b"k").await.unwrap(), b"fresh");
        });
}

/// The startup GC: gear-view tables this layout no longer declares — and the
/// pre-per-kind table — are dropped on open; the current kinds and unrelated
/// tables survive.
#[test]
fn paged_storage_gcs_old_gear_tables() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            use dentrado_cache::{Storage, TableSpec, disk::FsDisk};

            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("core-0.dstore");
            let path = file.display().to_string();
            let legacy = postcard::to_stdvec(&("dentrado/gear-view", 0u64)).unwrap();
            let orphan = b"dentrado/gear-view/0/Gone".to_vec();
            let foreign = b"unrelated/table".to_vec();
            // An older image with one live entry in every odd table.
            {
                let specs = [legacy.clone(), orphan.clone(), foreign.clone()].map(TableSpec::flat);
                let old = Storage::open(&FsDisk, &path, 4, &specs).await.unwrap();
                let mut s = old.session();
                for id in [&legacy, &orphan, &foreign] {
                    s.put(id, b"gear", b"ancient", &[], &[]).unwrap();
                }
                s.commit().unwrap();
                old.flush().await.unwrap();
            }
            // Opening through PagedStorage drops our orphans…
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.clone());
            let gear = BlobGear::Blob { key: 7 };
            let mut view = st.gear_view(&gear).await;
            view.put(b"k", b"kept");
            view.commit();
            st.flush().await.unwrap();
            drop(view);
            drop(st);
            // …and a raw reopen sees the current kinds (one table each) plus
            // the unrelated table.
            let raw = Storage::open(&FsDisk, &path, 4, &[]).await.unwrap();
            let mut names = raw.tables();
            names.sort();
            let mut expected = [BLOB_TABLE.to_vec(), NOTE_TABLE.to_vec(), foreign];
            expected.sort();
            assert_eq!(names, expected);
            drop(raw);
            // GC never touches the live kinds' records.
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file);
            let mut view = st.gear_view(&gear).await;
            assert_eq!(view.get(b"k").await.unwrap(), b"kept");
        });
}

/// One table per gear kind: each kind reads back independently across a
/// reopen, and the runtime's table ids all carry the GC's shared prefix.
#[test]
fn paged_storage_separates_gear_kinds() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            assert!(
                BlobRuntime::gear_tables()
                    .iter()
                    .all(|id| id.starts_with(GEAR_TABLE_PREFIX)),
                "the startup GC recognizes tables by this prefix"
            );

            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("core-0.dstore");
            let blob = BlobGear::Blob { key: 1 };
            let note = BlobGear::Note { key: 1 };
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.clone());
            {
                let mut view = st.gear_view(&blob).await;
                view.put(b"k", b"blob");
                view.commit();
            }
            {
                let mut view = st.gear_view(&note).await;
                assert!(view.get(b"k").await.is_none());
                view.put(b"k", b"note");
                view.commit();
            }
            st.flush().await.unwrap();
            drop(st);

            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file);
            let mut view = st.gear_view(&blob).await;
            assert_eq!(view.get(b"k").await.unwrap(), b"blob");
            let mut view = st.gear_view(&note).await;
            assert_eq!(view.get(b"k").await.unwrap(), b"note");
        });
}

/// Both backends refuse a record no leaf can hold. The disk view would fail
/// the whole flush, so the RAM view must refuse at the same point instead of
/// silently accepting more than the disk ever could.
#[test]
fn paged_storage_refuses_oversized_records() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            let gear = BlobGear::Blob { key: 7 };
            let oversized = vec![0u8; 8192];
            fn refuses(view: &mut impl GearView, value: &[u8]) -> bool {
                std::panic::catch_unwind(AssertUnwindSafe(|| view.put(b"k", value))).is_err()
            }

            let dir = tempfile::tempdir().unwrap();
            let st: PagedStorage<BlobRuntime> =
                PagedStorage::on_disk(dir.path().join("core-0.dstore"));
            let mut disk = st.gear_view(&gear).await;
            assert!(refuses(&mut disk, &oversized), "disk view refuses");

            let ram: PagedStorage<BlobRuntime> = PagedStorage::ram();
            let mut ram_view = ram.gear_view(&gear).await;
            assert!(refuses(&mut ram_view, &oversized), "RAM view refuses");
        });
}
