//! `PagedStorage` round-trip: gear-cache images survive flush + reopen as
//! value pages, are fetched back on demand, overwrites free their
//! predecessors, and a cache that stops persisting erases its entry.

use std::fmt::Debug;

use dentrado::{
    core::{
        core_ctx::GearCtx,
        gear::{GearInput, GearMeta, GearProduce, IsRuntime},
        storage::{CacheSer, PageId, PagedStorage, Storage},
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
struct BlobCache<W> {
    watermark: W,
    blob: String,
}

impl<W> CacheSer for BlobCache<W> {
    fn page_roots(&self) -> &[PageId] {
        &[]
    }

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
    type GearCache<W>
        = BlobCache<W>
    where
        W: Debug + Clone + 'static;

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            BlobGear::Blob { key } => GearMeta::Event {
                msg_type: LocMsgTypeId(1),
                group: i64::try_from(*key).unwrap(),
            },
        }
    }

    fn make_cache<W: Debug + Clone + Default + 'static>(
        _gear: &Self::GearId,
    ) -> Self::GearCache<W> {
        BlobCache {
            watermark: W::default(),
            blob: String::new(),
        }
    }

    async fn run_step<S: Storage<Self>>(
        _ctx: &mut GearCtx<Self, S>,
        _input: GearInput<Self>,
        _cache: &mut Self::GearCache<S::Watermark>,
    ) -> GearProduce<Self> {
        unreachable!("this test drives storage directly")
    }
}

async fn roundtrip(file: &std::path::Path) {
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    let gear = BlobGear::Blob { key: 7 };

    // Absent before the first write.
    assert!(st.get_cache(&gear).await.is_none());

    // An image larger than one page payload (4080 B) spans a value chain.
    let big = format!("{}+tail", "x".repeat(9000));
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: (0, 0),
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

    // Overwrite, reopen: the new image wins (the old chain is freed inside
    // the store's own accounting).
    let small = "y".to_string();
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: (1, 0),
            blob: small.clone(),
        },
    )
    .await;
    st.flush().await.unwrap();
    drop(st);
    let st: PagedStorage<BlobRuntime> = PagedStorage::on_disk(file.to_path_buf());
    assert_eq!(st.get_cache(&gear).await.unwrap().blob, small);

    // A cache that stops persisting (empty image) tombstones its entry.
    st.put_cache(
        gear.clone(),
        BlobCache {
            watermark: (2, 0),
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
            watermark: (0, 0),
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
