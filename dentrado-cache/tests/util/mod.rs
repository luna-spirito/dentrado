//! The cache tests' shared shorthand: the runtime, the flat table
//! set, inline record builders — and the test-side chunk format the
//! big-value tests ride (chunking is userspace's job now, the store
//! only knows the schema): a record's `mt` is the chain's head id
//! little-endian, a page is `[next u64][payload]`, the schema walks
//! the same shape on both levels.

// every test crate compiles this module and uses its own slice of it
#![allow(dead_code)]

use dentrado_cache::core::freelist::{ChainEntry, write_chunk};
use dentrado_cache::core::sb::write_sb;
use dentrado_cache::page::fnv_id;
use dentrado_cache::{
    Dep, IsFilesystem, MemDisk, Page, PageId, PageSchema, Rec, Storage, StoreError, TableSchema,
    TableSpec,
};

/// The chain page's payload: the page minus the `next` header.
pub const PAYLOAD: usize = dentrado_cache::page::PAGE_SIZE - 8;

pub fn run<F: Future>(f: F) -> F::Output {
    compio::runtime::Runtime::new().unwrap().block_on(f)
}

/// The flat set: live names, no schema-named pages.
pub fn specs(ids: &[&[u8]]) -> Vec<TableSpec> {
    ids.iter().map(|i| TableSpec::flat(i.to_vec())).collect()
}

pub fn keys(recs: &[(Vec<u8>, Vec<u8>)]) -> Vec<Vec<u8>> {
    recs.iter().map(|(k, _)| k.clone()).collect()
}

/// The tests' shorthand: an inline record, no pages on either list.
pub fn put<'a>(key: &'a [u8], val: &'a [u8]) -> Rec<'a> {
    Rec::Put {
        key,
        val,
        take: &[],
        free: &[],
    }
}

pub fn del(key: &[u8]) -> Rec<'_> {
    Rec::Del { key, free: &[] }
}

// ## The chain format: userspace chunking over the raw surface

fn chain_deps(b: &[u8]) -> Result<Vec<Dep>, StoreError> {
    let next = u64::from_le_bytes(b[..8].try_into().unwrap());
    Ok(if next == 0 {
        Vec::new()
    } else {
        vec![(next, CHAIN_PAGE)]
    })
}

fn chain_val_deps(mt: &[u8]) -> Result<Vec<Dep>, StoreError> {
    Ok(if mt.len() == 8 {
        let head = u64::from_le_bytes(mt.try_into().unwrap());
        if head == 0 {
            Vec::new()
        } else {
            vec![(head, CHAIN_PAGE)]
        }
    } else {
        Vec::new() // инлайн-запись: байты не нашего формата
    })
}

const CHAIN_PAGE: PageSchema = PageSchema { deps: chain_deps };
const CHAIN: TableSchema = TableSchema {
    val_deps: chain_val_deps,
};

/// The chain-format table set — for tests that ride chunked values.
pub fn chain_specs(ids: &[&[u8]]) -> Vec<TableSpec> {
    ids.iter()
        .map(|i| TableSpec {
            id: i.to_vec(),
            schema: CHAIN,
        })
        .collect()
}

/// Publish a big value as a chain of test chunks: the pages are
/// allocated, written (`next` at `[0..8]`, a payload slice at
/// `[8..]`) and taken by one record whose `mt` names the head. The
/// chain's pages come back — the caller lists them in `free` when a
/// replacement makes them die.
pub async fn put_big<D: IsFilesystem>(
    st: &Storage<D>,
    table: &[u8],
    key: &[u8],
    payload: &[u8],
    free: &[PageId],
) -> Result<Vec<PageId>, StoreError> {
    let n = payload.len().div_ceil(PAYLOAD).max(1);
    let mut ks = Vec::new();
    let mut pages = Vec::new();
    for _ in 0..n {
        let k = st.alloc_draft_page().await?;
        pages.push(st.draft_page(&k).await?);
        ks.push(k);
    }
    for (i, k) in ks.iter().enumerate() {
        let next = if i + 1 < n { pages[i + 1] } else { 0 };
        let lo = i * PAYLOAD;
        let chunk = payload[lo..payload.len().min(lo + PAYLOAD)].to_vec();
        st.write_draft_page(*k, move |pg| {
            pg.0[..8].copy_from_slice(&next.to_le_bytes());
            pg.0[8..8 + chunk.len()].copy_from_slice(&chunk);
        })
        .await?;
    }
    st.put_many(
        table,
        &[Rec::Put {
            key,
            val: &pages[0].to_le_bytes(),
            take: &ks,
            free,
        }],
    )
    .await?;
    Ok(pages)
}

/// Read a chain back: the record's `mt` names the head, the pages
/// hand out their payloads by reference, the answer is owned bytes.
pub async fn read_big<D: IsFilesystem>(
    st: &Storage<D>,
    table: &[u8],
    key: &[u8],
) -> Result<Vec<u8>, StoreError> {
    let mt = st.get(table, key).await?.expect("ключ есть");
    let mut head = u64::from_le_bytes(mt[..8].try_into().unwrap());
    let mut out = Vec::new();
    while head != 0 {
        let (next, chunk) = st
            .read_page(head, |pg| {
                (
                    u64::from_le_bytes(pg.0[..8].try_into().unwrap()),
                    pg.0[8..].to_vec(),
                )
            })
            .await?;
        out.extend_from_slice(&chunk);
        head = next;
    }
    Ok(out)
}
// ## Рукодельный образ со входом-смещением

/// Рукодельный образ мира из 14 страниц: chunk-страница 2 держит слоты
/// [4, 6, 8, 10, 12], вход чётного пула — ⟨2, off 2⟩, root нет. Живы
/// чётные слоты [8, 10, 12]; мёртвые [4, 6] впереди не выдаются и в
/// счётчики не попадают.
pub async fn offset_image(disk: &MemDisk) {
    let f = <MemDisk as IsFilesystem>::open(disk, "db").await.unwrap();
    let mut ch = AlignedPage::zeroed();
    write_chunk(&mut ch, 0, &[4, 6, 8, 10, 12]);
    <MemDisk as IsFilesystem>::write_page(f.clone(), 2, ch.0.to_vec())
        .await
        .0
        .unwrap();
    let z = AlignedPage::zeroed().0.to_vec();
    for id in 3..14 {
        <MemDisk as IsFilesystem>::write_page(f.clone(), id, z.clone())
            .await
            .0
            .unwrap();
    }
    let mut sbp = AlignedPage::zeroed();
    // мир из 14 страниц: чётный счётчик 14 (0 — SB, 2 — chunk, слоты
    // 4..12), нечётный 15 (1 — SB, 3..13 — в мире, но не в цепях)
    write_sb(
        &mut sbp,
        1,
        14,
        15,
        &[(b"t0".to_vec(), 0)],
        ChainEntry { page: 2, off: 2 },
        ChainEntry::default(),
        3,
        0,
        fnv_id(&[8, 10, 12]),
        fnv_id(&[]),
    );
    <MemDisk as IsFilesystem>::write_page(f.clone(), 0, sbp.0.to_vec())
        .await
        .0
        .unwrap();
}
