//! `PagedStorage` gear-view round-trip: staged writes become visible only
//! after `commit`, survive flush + reopen, overwrite and delete behave, and
//! `scan` walks one gear's prefix without leaking another gear's records.
//!
//! The base view is flat — a value must fit one leaf record — so a gear
//! that needs bigger values stores several records itself. This test only
//! exercises the flat contract.

use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime},
        storage::{GearView, IsStorage, PagedStorage},
    },
    types::*,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum BlobGear {
    Blob { key: u64 },
}

impl Localizable for BlobGear {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

#[derive(Debug)]
struct BlobRuntime;

impl IsRuntime for BlobRuntime {
    type GearId = BlobGear;
    type GearOut = ();
    type GearOutShared = ();
    type GearOutLocal = ();
    type Module = ();
    type Group = i64;
    type Body = ();
    type Data = ();

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            BlobGear::Blob { key } => GearMeta::Event {
                msg_type: LocMsgTypeId(1),
                group: i64::try_from(*key).unwrap(),
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

/// The startup GC: a table left by an older view-format version is dropped
/// on open; the current version's table survives.
#[test]
fn paged_storage_gcs_old_versions() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            use dentrado_cache::{Storage, TableSpec, disk::FsDisk};

            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("core-0.dstore");
            let path = file.display().to_string();
            let old_id = postcard::to_stdvec(&("dentrado/gear-view", 1u64)).unwrap();
            let cur_id = postcard::to_stdvec(&("dentrado/gear-view", 0u64)).unwrap();
            // An old-version store with one live entry.
            {
                let old = Storage::open(&FsDisk, &path, 4, &[TableSpec::flat(old_id.clone())])
                    .await
                    .unwrap();
                let mut s = old.session();
                s.put(&old_id, b"gear", b"ancient", &[], &[]).unwrap();
                s.commit().unwrap();
                old.flush().await.unwrap();
            }
            // Opening through PagedStorage drops the orphan…
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.clone());
            let gear = BlobGear::Blob { key: 7 };
            let mut view = st.gear_view(&gear).await;
            assert!(view.get(b"k").await.is_none());
            view.commit();
            st.flush().await.unwrap();
            drop(view);
            drop(st);
            // …and a raw reopen sees only the current version's table.
            let raw = Storage::open(&FsDisk, &path, 4, &[TableSpec::flat(cur_id.clone())])
                .await
                .unwrap();
            let names = raw.tables();
            assert_eq!(names, [cur_id]);
        });
}
