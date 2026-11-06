//! `PagedStorage` round-trip: gear-cache images survive flush + reopen
//! (small ones as their records, big ones as page chains), are fetched
//! back on demand, overwrites vouch their predecessors' pages free,
//! and a cache that stops persisting erases its record.

use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime},
        storage::{CacheSer, IsStorage, PagedStorage, Watermark},
    },
    types::*,
};

#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize)]
enum BlobGear {
    Blob { key: u64 },
}

impl Localizable for BlobGear {
    async fn localize<Rm: Remapper>(self, _remapper: &mut Rm) -> Result<Self, Rm::Err> {
        Ok(self)
    }
}

/// A cache whose image is the blob verbatim; an empty blob means "do not
/// persist" (the None image → tombstone path).
#[derive(Debug, Clone)]
struct BlobCache {
    watermark: Watermark,
    blob: String,
}

impl CacheSer for BlobCache {
    fn ser_image(&self) -> Option<Vec<u8>> {
        (!self.blob.is_empty()).then(|| self.blob.clone().into_bytes())
    }

    fn de_image(&mut self, bytes: &[u8]) -> bool {
        match String::from_utf8(bytes.to_vec()) {
            Ok(blob) => {
                self.blob = blob;
                true
            }
            Err(_) => false,
        }
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
    type GearCache = BlobCache;

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            BlobGear::Blob { key } => GearMeta::Event {
                msg_type: LocMsgTypeId(1),
                group: i64::try_from(*key).unwrap(),
            },
        }
    }

    fn make_cache(_gear: &Self::GearId) -> Self::GearCache {
        BlobCache {
            watermark: Watermark::default(),
            blob: String::new(),
        }
    }

    async fn run_step<S: IsStorage<Self>>(
        _ctx: &mut GearCtx<Self, S>,
        _input: GearInput<Self>,
        _cache: &mut Self::GearCache,
    ) -> GearProduce<Self> {
        unreachable!("this test drives storage directly")
    }
}

async fn roundtrip(file: &std::path::Path) {
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let gear = BlobGear::Blob { key: 7 };

    // Absent before the first write.
    assert!(st.get_cache(&gear).await.is_none());

    // An image spanning three chunks (2×4088 payload + a tail) rides a
    // page chain: chunking is this layer's job now.
    let big = format!("{}+tail", "x".repeat(9000));
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: Watermark(0, 0),
            blob: big.clone(),
        },
    )
    .await;
    st.flush().await.unwrap();
    assert_eq!(st.get_cache(&gear).await.unwrap().blob, big);

    // Reopen from disk: the image is fetched back on demand.
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    assert_eq!(st.get_cache(&gear).await.unwrap().blob, big);

    // Overwrite, reopen: the new image wins (the old chain's pages ride
    // the ledger's `free` list).
    let small = "y".to_string();
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: Watermark(1, 0),
            blob: small.clone(),
        },
    )
    .await;
    st.flush().await.unwrap();
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    assert_eq!(st.get_cache(&gear).await.unwrap().blob, small);

    // A cache that stops persisting (empty image) erases its record.
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: Watermark(2, 0),
            blob: String::new(),
        },
    )
    .await;
    st.flush().await.unwrap();
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    assert!(st.get_cache(&gear).await.is_none());

    // RAM mode (`file = None`) stays a plain in-memory backend.
    let ram: PagedStorage<BlobRuntime> = PagedStorage::ram();
    ram.put_cache(
        gear,
        BlobCache {
            watermark: Watermark(0, 0),
            blob: "z".into(),
        },
    )
    .await;
    assert_eq!(
        ram.get_cache(&BlobGear::Blob { key: 7 })
            .await
            .unwrap()
            .blob,
        "z"
    );
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
            assert!(st.get_cache(&gear).await.is_none());
            st.put_cache(
                gear.clone(),
                BlobCache {
                    watermark: Watermark::default(),
                    blob: "fresh".into(),
                },
            )
            .await;
            st.flush().await.unwrap();
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
            assert_eq!(st.get_cache(&gear).await.unwrap().blob, "fresh");
        });
}

/// The startup GC: a table left by an older cache-format version is dropped
/// on open; the current version's table survives.
#[test]
fn paged_storage_gcs_old_versions() {
    compio::runtime::RuntimeBuilder::new()
        .build()
        .unwrap()
        .block_on(async {
            use dentrado_cache::{Rec, Storage, TableSpec, disk::FsDisk};

            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("core-0.dstore");
            let path = file.display().to_string();
            let old_id = postcard::to_stdvec(&("dentrado/gear-cache", 1u64)).unwrap();
            let cur_id = postcard::to_stdvec(&("dentrado/gear-cache", 0u64)).unwrap();
            // An old-version store with one live entry.
            {
                let old = Storage::open(&FsDisk, &path, 4, &[TableSpec::flat(old_id.clone())])
                    .await
                    .unwrap();
                old.put_many(
                    &old_id,
                    &[Rec::Put {
                        key: b"gear",
                        val: b"ancient",
                        take: &[],
                        free: &[],
                    }],
                )
                .await
                .unwrap();
                old.flush().await.unwrap();
            }
            // Opening through PagedStorage drops the orphan…
            let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.clone());
            let gear = BlobGear::Blob { key: 7 };
            assert!(st.get_cache(&gear).await.is_none());
            st.flush().await.unwrap();
            drop(st);
            // …and a raw reopen sees only the current version's table.
            let raw = Storage::open(&FsDisk, &path, 4, &[TableSpec::flat(cur_id.clone())])
                .await
                .unwrap();
            let names = raw.tables();
            assert_eq!(names, [cur_id]);
        });
}
