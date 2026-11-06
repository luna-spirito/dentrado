//! The gear-cache image's chunking — userspace chunks over the store's
//! raw surface (the store neither knows nor parses what a page holds;
//! it only walks the schema we declare). A small image rides the
//! record itself ([`ImgRef::Inline`]); a big one takes a chain of
//! pages — `[next u64][payload]` per page, the record's bytes naming
//! the head and length ([`ImgRef::Chain`]). The same shape on both
//! levels is the schema: the pair of functions the store's closure
//! walks (`drop_table`, the partition audit) run on.

use dentrado_cache::{
    Dep, IsFilesystem, PageId, PageSchema, Rec, Storage, StoreError, TableSchema, TableSpec,
    page::PAGE_SIZE,
};

/// The chunk's payload: the page minus the `next` header.
pub(super) const CHUNK_PAYLOAD: usize = PAGE_SIZE - 8;

/// An image at or under this many bytes rides the record itself;
/// bigger ones take the chain (the common cache's whole life stays a
/// single record, chains stay rare).
pub(super) const INLINE_MAX: usize = 512;

/// The record's bytes: the image itself, or the chain's head and
/// length.
#[derive(serde::Serialize, serde::Deserialize)]
enum ImgRef {
    Inline(Vec<u8>),
    Chain { head: PageId, len: u64 },
}

fn chain_deps(page: &[u8]) -> Result<Vec<Dep>, StoreError> {
    let next = u64::from_le_bytes(page[..8].try_into().expect("a page is 4096"));
    Ok(if next == 0 {
        Vec::new()
    } else {
        vec![(next, CHUNK_PAGE)]
    })
}

fn val_deps(mt: &[u8]) -> Result<Vec<Dep>, StoreError> {
    match postcard::from_bytes::<ImgRef>(mt) {
        Ok(ImgRef::Chain { head, .. }) => Ok(vec![(head, CHUNK_PAGE)]),
        Ok(ImgRef::Inline(_)) => Ok(Vec::new()),
        Err(_) => Err(StoreError::CorruptValue("not a gear-cache image")),
    }
}

/// The page half of the cache schema: a chunk names its successor.
pub(super) const CHUNK_PAGE: PageSchema = PageSchema { deps: chain_deps };

/// The table half: a record names its chain's head.
pub(super) const CACHE_TABLE: TableSchema = TableSchema { val_deps: val_deps };

/// The cache table's spec: the caller's identity bytes over the cache
/// schema.
pub(super) fn cache_spec(id: Vec<u8>) -> TableSpec {
    TableSpec {
        id,
        schema: CACHE_TABLE,
    }
}

/// The image back with its pages: `Inline` — none; `Chain` — the
/// walk's page ids, the caller's ledger for the future `free`. A
/// record whose bytes this code cannot decode reads as absent — a
/// stale entry dies at the next overwrite, it does not poison the
/// disk. A chain that lies about its length (short, long, or a
/// successor past the end) is real corruption: an error, the boundary's
/// word.
pub(super) async fn read_image<D: IsFilesystem>(
    store: &Storage<D>,
    table: &[u8],
    key: &[u8],
) -> Result<Option<(Vec<u8>, Vec<PageId>)>, StoreError> {
    let Some(mt) = store.get(table, key).await? else {
        return Ok(None);
    };
    let Ok(r#ref) = postcard::from_bytes::<ImgRef>(&mt) else {
        return Ok(None);
    };
    match r#ref {
        ImgRef::Inline(v) => Ok(Some((v, Vec::new()))),
        ImgRef::Chain { head, len } => {
            let mut out = Vec::new();
            let mut pages = Vec::new();
            let mut left = len;
            let mut p = head;
            while left > 0 {
                if p == 0 {
                    return Err(StoreError::CorruptValue("gear-cache chain runs short"));
                }
                let take = left.min(CHUNK_PAYLOAD as u64) as usize;
                pages.push(p);
                let (next, chunk) = store
                    .read_page(p, move |pg| {
                        (
                            u64::from_le_bytes(pg.0[..8].try_into().expect("a page is 4096")),
                            pg.0[8..8 + take].to_vec(),
                        )
                    })
                    .await?;
                out.extend_from_slice(&chunk);
                left -= take as u64;
                p = next;
            }
            if p != 0 {
                return Err(StoreError::CorruptValue("gear-cache chain runs long"));
            }
            Ok(Some((out, pages)))
        }
    }
}

/// Publish an image: small — one record; big — a chain of drafted
/// pages (`next` at `[0..8]`, a payload slice behind it), the record
/// naming the head. `free` lists the superseded record's pages — the
/// caller's ledger, the store does not traverse. The written pages
/// come back, for the same ledger.
pub(super) async fn put_image<D: IsFilesystem>(
    store: &Storage<D>,
    table: &[u8],
    key: &[u8],
    image: &[u8],
    free: &[PageId],
) -> Result<Vec<PageId>, StoreError> {
    if image.len() <= INLINE_MAX {
        let mt = postcard::to_stdvec(&ImgRef::Inline(image.to_vec())).expect("a vec serializes");
        store
            .put_many(
                table,
                &[Rec::Put {
                    key,
                    val: &mt,
                    take: &[],
                    free,
                }],
            )
            .await?;
        return Ok(Vec::new());
    }
    let n = image.len().div_ceil(CHUNK_PAYLOAD);
    let mut ks = Vec::new();
    let mut pages: Vec<PageId> = Vec::new();
    for _ in 0..n {
        let k = store.alloc_draft_page().await?;
        pages.push(store.draft_page(&k).await?);
        ks.push(k);
    }
    for (i, &k) in ks.iter().enumerate() {
        let next = if i + 1 < n { pages[i + 1] } else { 0 };
        let lo = i * CHUNK_PAYLOAD;
        let chunk = image[lo..(lo + CHUNK_PAYLOAD).min(image.len())].to_vec();
        store
            .write_draft_page(k, move |pg| {
                pg.0[..8].copy_from_slice(&next.to_le_bytes());
                pg.0[8..8 + chunk.len()].copy_from_slice(&chunk);
            })
            .await?;
    }
    let mt = postcard::to_stdvec(&ImgRef::Chain {
        head: pages[0],
        len: image.len() as u64,
    })
    .expect("a pair serializes");
    store
        .put_many(
            table,
            &[Rec::Put {
                key,
                val: &mt,
                take: &ks,
                free,
            }],
        )
        .await?;
    Ok(pages)
}
