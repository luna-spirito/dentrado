//! The record surface — the store's tables as key-addressed records:
//! point reads, prefix scans, and strictly atomic batched writes. A
//! record's value lives inline in its leaf while it fits
//! ([`INLINE_MAX`]); a bigger one becomes a chain of value pages with
//! the little-endian length as its inline bytes — the store does the
//! blobbing, the caller only ever hands bytes. The record's dependent
//! pages (the schema's half — Phase B) join the same flows through the
//! closure walks.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use super::{Guard, Storage};
use crate::blockcache;
use crate::core::tree::{leaf_find, leaf_holds, parse_node, route};
use crate::core::val::{VAL_PAYLOAD, link_val, val_split};
use crate::core::{CacheValue, DraftId, SealRec, StepHit};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;
use crate::persist::PersistentCore;
use crate::persist::walks::{walk_deps, walk_range, walk_value};
use crate::schema::TableSchema;

/// A record's inline bound: a value at most this lives in the leaf's
/// record; a bigger one becomes a chain of pages. Records stay cheap
/// to graft, and a leaf holds dozens of them.
pub const INLINE_MAX: usize = 512;

/// One record of a `put_many` batch: a value going in, or a key going
/// out.
#[derive(Clone, Copy, Debug)]
pub enum Rec<'a> {
    Put { key: &'a [u8], val: &'a [u8] },
    Del { key: &'a [u8] },
}

/// The table's slot and the world's page bound — one section.
fn table_at<D: IsFilesystem>(s: &Storage<D>, id: &[u8]) -> Result<(usize, u64), StoreError> {
    let inner = s.inner.borrow();
    if inner.poisoned {
        return Err(StoreError::Poisoned);
    }
    let Some(t) = inner.core.table_index(id) else {
        return Err(StoreError::NoTable);
    };
    Ok((t, inner.core.bound()))
}

impl<D: IsFilesystem> Storage<D> {
    /// Read one record's value whole: the delta first, the standing
    /// tree second — a chained value is assembled from its pages on
    /// the fly. The EBR pin lives inside the walk; the answer is owned
    /// bytes, no page references out.
    pub async fn get(&self, id: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let (t, bound) = table_at(self, id)?;
        let (bucket, hit, root) = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            let Some((b, hit, root)) = inner.core.get_step(t, key) else {
                return Err(StoreError::NoTable);
            };
            (b, hit, root)
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket,
        };
        let r = match hit {
            StepHit::Put(cv) => assemble::<D>(&self.inner, cv.mt, cv.pages).await.map(Some),
            StepHit::Del => Ok(None),
            StepHit::Tree => match descend::<D>(&self.inner, root, key, bound).await? {
                None => Ok(None),
                Some((mt, vr)) => {
                    let pages = chain_of::<D>(&self.inner, vr, bound).await?;
                    assemble::<D>(&self.inner, mt, pages).await.map(Some)
                }
            },
        };
        drop(guard);
        r
    }

    /// Scan a table: the records under `prefix`, strictly past `after`,
    /// in key order — the standing tree pruned to the range, the delta
    /// overlaid on it (a delta put replaces, a delta deletion hides).
    /// The EBR pin lives inside the walk; the answer is owned bytes.
    pub async fn scan(
        &self,
        id: &[u8],
        prefix: &[u8],
        after: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        let (t, bound) = table_at(self, id)?;
        let end = prefix_end(prefix);
        let lo: &[u8] = if after > prefix { after } else { prefix };
        let (bucket, root, overlay) = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            (
                inner.core.pin_step(),
                inner.core.table_root(t),
                inner.core.table_delta_range(t, prefix, end.as_deref()),
            )
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket,
        };
        let mut merged: BTreeMap<Vec<u8>, Option<CacheValue>> = BTreeMap::new();
        for (k, mt, vr) in walk_range::<D>(&self.inner, root, lo, end.as_deref(), bound).await? {
            let pages = chain_of::<D>(&self.inner, vr, bound).await?;
            merged.insert(k, Some(CacheValue { mt, pages }));
        }
        for (k, v) in overlay {
            merged.insert(
                k,
                match v {
                    crate::core::DeltaV::Put(cv) => Some(cv),
                    crate::core::DeltaV::Del => None,
                },
            );
        }
        let mut out = Vec::new();
        for (k, cv) in merged {
            if k.as_slice() <= after {
                continue;
            }
            if let Some(cv) = cv {
                out.push((k, assemble::<D>(&self.inner, cv.mt, cv.pages).await?));
            }
        }
        drop(guard);
        Ok(out)
    }

    /// Write a batch of records, atomically: the store itself computes
    /// every replaced record's closure (its pages die), a value over
    /// [`INLINE_MAX`] becomes a chain of fresh pages (the record its
    /// little-endian length), and the whole batch — the deltas, the
    /// draft consumption, the deaths — lands in one section:
    /// RAM-atomic, and disk-atomic within the flush round that
    /// engraves it. A key no leaf could hold refuses the batch whole;
    /// a batch interrupted mid-flight leaves its taken drafts held
    /// (unpublished, harmless — they die with the store).
    pub async fn put_many(&self, id: &[u8], batch: &[Rec<'_>]) -> Result<(), StoreError> {
        let (t, schema, bound) = {
            let inner = self.inner.borrow();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            let Some(t) = inner.core.table_index(id) else {
                return Err(StoreError::NoTable);
            };
            (t, inner.core.table_schema(t), inner.core.bound())
        };
        // The last record per key wins; the batch runs in key order.
        let mut ops: BTreeMap<Vec<u8>, Option<&[u8]>> = BTreeMap::new();
        for r in batch {
            match *r {
                Rec::Put { key, val } => ops.insert(key.to_vec(), Some(val)),
                Rec::Del { key } => ops.insert(key.to_vec(), None),
            };
        }
        if ops.is_empty() {
            return Ok(());
        }
        let mut recs: Vec<SealRec> = Vec::with_capacity(ops.len());
        let mut freed: Vec<PageId> = Vec::new();
        for (k, val) in &ops {
            freed.extend(self.old_closure(t, schema, k, bound).await?);
            let (put, consumed) = match val {
                Some(v) if v.len() > INLINE_MAX => {
                    let cs = match self.blob(v).await {
                        Ok(cs) => cs,
                        Err(e) => return Err(e),
                    };
                    (Some(v.len().to_le_bytes().to_vec()), cs)
                }
                Some(v) => (Some(v.to_vec()), Vec::new()),
                None => (None, Vec::new()),
            };
            if !leaf_holds(k, put.as_deref().unwrap_or(&[])) {
                self.give_back(&consumed).await;
                return Err(StoreError::BadKey);
            }
            recs.push(SealRec {
                k: k.clone(),
                put,
                consumed,
            });
        }
        let r = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                Err(StoreError::Poisoned)
            } else {
                inner.core.seal_batch(t, &recs, &freed)
            }
        };
        if r.is_err() {
            for rec in &recs {
                self.give_back(&rec.consumed).await;
            }
        }
        r
    }

    /// The record's standing closure — the pages that die when this
    /// batch's record for `k` lands: the delta entry's pages and
    /// schema-named pages, or the tree record's value chain and
    /// schema-named pages. A delta deletion or a miss names none
    /// (their closure died at their own seal, or never was).
    async fn old_closure(
        &self,
        t: usize,
        schema: &'static dyn TableSchema,
        k: &[u8],
        bound: u64,
    ) -> Result<Vec<PageId>, StoreError> {
        let (bucket, hit, root) = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            let Some((b, hit, root)) = inner.core.get_step(t, k) else {
                return Err(StoreError::NoTable);
            };
            (b, hit, root)
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket,
        };
        let r = match hit {
            StepHit::Put(cv) => {
                let mut ps = cv.pages;
                ps.extend(walk_deps::<D>(&self.inner, schema, &cv.mt, 0, bound).await?);
                ps
            }
            StepHit::Del => Vec::new(),
            StepHit::Tree => match descend::<D>(&self.inner, root, k, bound).await? {
                None => Vec::new(),
                Some((mt, vr)) => walk_deps::<D>(&self.inner, schema, &mt, vr, bound).await?,
            },
        };
        drop(guard);
        Ok(r)
    }

    /// A value's chain of fresh drafts: the bytes split over value
    /// pages, the pages stitched in order. A failure at any step gives
    /// the taken drafts back.
    async fn blob(&self, val: &[u8]) -> Result<Vec<DraftId>, StoreError> {
        let n = val.len().div_ceil(VAL_PAYLOAD);
        let mut consumed: Vec<DraftId> = Vec::with_capacity(n);
        for _ in 0..n {
            match self.alloc_draft_page().await {
                Ok(k) => consumed.push(k),
                Err(e) => {
                    self.give_back(&consumed).await;
                    return Err(e);
                }
            }
        }
        for (i, &k) in consumed.iter().enumerate() {
            let upto = ((i + 1) * VAL_PAYLOAD).min(val.len());
            let chunk = val[i * VAL_PAYLOAD..upto].to_vec();
            if let Err(e) = self.write_draft_page(k, move |_| (chunk.clone(), ())).await {
                self.give_back(&consumed).await;
                return Err(e);
            }
        }
        let pages = {
            let inner = self.inner.borrow();
            inner
                .core
                .pages_of(&consumed)
                .ok_or(StoreError::UnknownDraft)?
        };
        if let Err(e) = self.link_all(&pages).await {
            self.give_back(&consumed).await;
            return Err(e);
        }
        Ok(consumed)
    }

    /// The taken drafts back into their pool (a failed batch's
    /// rollback).
    async fn give_back(&self, taken: &[DraftId]) {
        for c in taken {
            let _ = self.dealloc_draft_page(*c).await;
        }
    }

    /// Stitch a value's chain: the pages' next headers in order.
    async fn link_all(&self, pages: &[PageId]) -> Result<(), StoreError> {
        for (i, &p) in pages.iter().enumerate() {
            let next = pages.get(i + 1).copied().unwrap_or(0);
            let last = i + 1 == pages.len();
            blockcache::update::<D, _>(&self.inner, p, move |b| link_val(b, next, last)).await?;
        }
        Ok(())
    }
}

/// The standing tree's record for `key` — a root-to-leaf descent, its
/// inline bytes and chain head. The fuel is the world's pages: a
/// deeper tree can only be corruption.
async fn descend<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    key: &[u8],
    bound: u64,
) -> Result<Option<(Vec<u8>, PageId)>, StoreError> {
    if root == 0 {
        return Ok(None);
    }
    let mut p = root;
    let mut fuel = bound;
    let hit = loop {
        if fuel == 0 {
            return Err(StoreError::Steps("tree: steps"));
        }
        fuel -= 1;
        let node = blockcache::read::<D, _>(inner, p, parse_node).await?;
        if node.level == 0 {
            break leaf_find(&node, key).map(|(mt, vr)| (mt.to_vec(), vr));
        }
        match route(&node, key) {
            Some(q) => p = q,
            None => break None, // below the tree's smallest key
        }
    };
    Ok(hit)
}

/// A chain head's pages (`0` — an inline value, none).
async fn chain_of<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    vr: PageId,
    bound: u64,
) -> Result<Vec<PageId>, StoreError> {
    if vr == 0 {
        Ok(Vec::new())
    } else {
        walk_value::<D>(inner, vr, bound).await
    }
}

/// A record's value from its halves: inline bytes alone, or the chain
/// read page by page and cut to the length header. (The two shapes
/// never mix: a chained record's inline bytes are exactly its length.)
async fn assemble<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    mt: Vec<u8>,
    pages: Vec<PageId>,
) -> Result<Vec<u8>, StoreError> {
    if pages.is_empty() {
        return Ok(mt);
    }
    let n = u64::from_le_bytes(
        mt.as_slice()
            .try_into()
            .expect("a chained record holds its length"),
    ) as usize;
    let mut val = Vec::with_capacity(n);
    for p in pages {
        let chunk = blockcache::read::<D, _>(inner, p, |b| val_split(b).1.to_vec()).await?;
        val.extend_from_slice(&chunk);
    }
    val.truncate(n);
    Ok(val)
}

/// The key just past the prefix's span: the prefix with its last
/// non-`0xFF` byte incremented, the `0xFF` tail dropped (`None` — an
/// all-`0xFF` prefix runs to the end of the key space).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut e = prefix.to_vec();
    while matches!(e.last(), Some(&0xFF)) {
        e.pop();
    }
    match e.last_mut() {
        Some(b) => {
            *b += 1;
            Some(e)
        }
        None => None,
    }
}
