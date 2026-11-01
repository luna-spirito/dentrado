//! The store's async surface: drafts (allocation, the writes, the
//! publication) and the cache (guarded reads — the delta before the
//! tree). Under it: [`crate::core`] — the flush-agnostic machines the
//! sections drive, [`crate::persist`] — the persistence overlay, the
//! flush round, recovery.
//!
//! The state is the [`PersistentCore`] under a single `RefCell` (the
//! store is single-threaded by construction — compio thread-per-core,
//! `!Sync`); an atomic section is a short `borrow_mut` with no await
//! inside (the analogue of the model's `Op.state`). The disk is the
//! [`Storage`] trait; the file handle (`Clone`, positional io) lives
//! in the cache's disk arena ([`crate::blockcache`]). Handles
//! outliving an operation hold their world through an `Rc` — the read
//! guard ([`Guard`]) the store, a flight's lease the arena's pages;
//! the pool's slow paths are the same sections plus the flight
//! driver and the round agent — tasks that hold the store and die by
//! `Store`'s own drop.
//!
//! Loop bounds (fuel) are estimates derived in the model; running out
//! is a visible [`StoreError::Steps`] error (image corruption).

use std::cell::RefCell;
use std::rc::Rc;

use compio::runtime::JoinHandle;

use crate::blockcache;
use crate::core::freelist::{ChainEntry, chunk_cells, chunk_next};
use crate::core::tree::{leaf_find, parse_node, route};
use crate::core::val::{link_val, val_split, write_val};
use crate::core::{AllocAct, CacheValue, DraftId};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{Page, PageId};
use crate::persist::PersistentCore;

// ## The read guard

/// The `get_cache` guard — an EBR pin (the subscription bucket). Death
/// is RAII: dropping it unsubscribes and advances the emptied bucket.
pub struct Guard<D: IsFilesystem> {
    inner: Rc<RefCell<PersistentCore<D>>>,
    bucket: usize,
}

impl<D: IsFilesystem> Drop for Guard<D> {
    fn drop(&mut self) {
        self.inner.borrow_mut().core.drop_step(self.bucket);
    }
}

/// A tail-read's rollback, RAII: a Busy tail without its reader bricks
/// the pool — every later alloc parks on a splice that never comes.
/// Defused once the read lands and the splice follows in the same
/// section; drops otherwise (an io failure's `?` or the caller's
/// cancellation between the section and the splice).
struct ReadRollback<D: IsFilesystem> {
    inner: Rc<RefCell<PersistentCore<D>>>,
    ep: usize,
    e: ChainEntry,
    armed: bool,
}

impl<D: IsFilesystem> ReadRollback<D> {
    fn new(inner: Rc<RefCell<PersistentCore<D>>>, ep: usize, e: ChainEntry) -> Self {
        Self {
            inner,
            ep,
            e,
            armed: true,
        }
    }

    /// The read landed — the splice follows in the same section.
    fn defuse(mut self) {
        self.armed = false;
    }
}

impl<D: IsFilesystem> Drop for ReadRollback<D> {
    fn drop(&mut self) {
        if self.armed {
            self.inner.borrow_mut().core.abort_read(self.ep, self.e);
        }
    }
}

// ## Store

/// An atomic cache store over the disk `D`. The driver's and the round
/// agent's handles are the store's own death switch: dropping the
/// store drops them, which cancels the tasks (compio's `JoinHandle`
/// cancels on drop, it does not detach) — and their `Rc`s are the
/// store's last references, so the store dies with them, not before
/// them.
pub struct Storage<D: IsFilesystem> {
    pub(crate) inner: Rc<RefCell<PersistentCore<D>>>,
    pub(crate) _driver: JoinHandle<!>,
    pub(crate) _flusher: JoinHandle<!>,
}

/// State inspection at rest (tests, debugging).
#[derive(Debug, Clone)]
pub struct DebugState {
    pub delta_len: usize,
    pub heads: [Vec<PageId>; 2],
    pub free_nexts: [PageId; 2],
    pub r#gen: u64,
    pub flush_busy: bool,
    pub dying_len: usize,
}

/// A page-partition violation — [`Store::partition_exact`]'s report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// An in-world page no role covers.
    Missing(PageId),
    /// An illegal combination of roles (a duplicate listing included)
    /// on one page: `tree`, `value`, `chunk`, `front`-cell, `free`-cell,
    /// `head`, `draft`, `held`, `spent`, `delta`.
    Roles(PageId, Vec<&'static str>),
    /// The image itself does not walk: a value chain broke past a valid
    /// SB, or a runtime bookmark sits off its pool's committed chain.
    Broken(&'static str),
}

impl<D: IsFilesystem> Storage<D> {
    pub fn debug_state(&self) -> DebugState {
        let (r#gen, flush_busy, ram) = {
            let inner = self.inner.borrow();
            (
                inner.r#gen,
                !inner.mail.is_empty() || inner.core.round_live(),
                inner.core.ram(),
            )
        };
        DebugState {
            delta_len: ram.delta,
            heads: ram.heads,
            free_nexts: ram.nexts,
            r#gen,
            flush_busy,
            dying_len: ram.dying,
        }
    }

    /// Draft allocation: own pool's head → the tail chain (a single
    /// reader, the rest park) → growing the world.
    pub async fn alloc_draft_page(&self) -> Result<DraftId, StoreError> {
        let bound = {
            let inner = self.inner.borrow();
            2 * inner.core.bound()
        };
        let mut fuel = bound;
        loop {
            if fuel == 0 {
                return Err(StoreError::Steps("alloc: steps"));
            }
            fuel -= 1;
            let act = {
                let mut inner = self.inner.borrow_mut();
                inner.core.alloc_step()
            }?;
            match act {
                AllocAct::Got(k) => return Ok(k),
                AllocAct::Park(ln) => ln.await,
                AllocAct::Read { ep, e } => {
                    // The read may die (an io failure's `?`) or be
                    // cancelled (the caller's future dropped mid-await);
                    // either way the Busy tail must not outlive its
                    // reader — the guard rolls it back and wakes the
                    // parked, unless defused at the splice.
                    let rb = ReadRollback::new(self.inner.clone(), ep, e);
                    let (cells, nx) = blockcache::read::<D, _>(&self.inner, e.page, move |b| {
                        (chunk_cells(b, e.off as usize), chunk_next(b))
                    })
                    .await?;
                    rb.defuse();
                    let mut inner = self.inner.borrow_mut();
                    inner.core.splice(ep, e, cells, nx);
                }
            }
        }
    }

    /// A draft's death: back into its own pool's head, no gate.
    pub async fn dealloc_draft_page(&self, k: DraftId) -> Result<(), StoreError> {
        let ok = {
            let mut inner = self.inner.borrow_mut();
            inner.core.dealloc_step(k)
        };
        if ok {
            Ok(())
        } else {
            Err(StoreError::UnknownDraft)
        }
    }

    /// A write into a draft: `f` — over the payload (the children's
    /// header is ours); a fresh page is not read from disk (we write
    /// over the slot's garbage).
    pub async fn write_draft_page<R>(
        &self,
        k: DraftId,
        f: impl Fn(&[u8]) -> (Vec<u8>, R) + 'static,
    ) -> Result<R, StoreError>
    where
        R: 'static,
    {
        let (p, fresh) = {
            let inner = self.inner.borrow();
            inner.core.draft(&k).ok_or(StoreError::UnknownDraft)?
        };
        let g = move |b: &mut Page| {
            let (pl, r) = f(val_split(b).1);
            write_val(b, 0, true, &pl);
            r
        };
        let r = if fresh {
            blockcache::init::<D, _>(&self.inner, p, g).await?
        } else {
            blockcache::update::<D, _>(&self.inner, p, g).await?
        };
        if fresh {
            let mut inner = self.inner.borrow_mut();
            inner.core.defresh(&k);
        }
        Ok(r)
    }

    /// Overwrite a draft over the slot's current contents, no read.
    pub async fn overwrite_draft_page<R>(
        &self,
        k: DraftId,
        f: impl Fn(&[u8]) -> (Vec<u8>, R) + 'static,
    ) -> Result<R, StoreError>
    where
        R: 'static,
    {
        let p = {
            let inner = self.inner.borrow();
            inner.core.draft(&k).ok_or(StoreError::UnknownDraft)?.0
        };
        blockcache::init::<D, _>(&self.inner, p, move |b| {
            let (pl, r) = f(val_split(b).1);
            write_val(b, 0, true, &pl);
            r
        })
        .await
    }

    /// A page read under a guard (the usual discipline: the guard is
    /// from `get_cache`; the runtime does not check it — the reader is
    /// obliged to hold it).
    pub async fn read_page_guarded<R>(
        &self,
        _g: &Guard<D>,
        p: PageId,
        f: impl Fn(&Page) -> R + 'static,
    ) -> Result<R, StoreError>
    where
        R: 'static,
    {
        blockcache::read::<D, _>(&self.inner, p, f).await
    }

    /// A cache read: an EBR pin (the guard), the delta first, the tree
    /// second — a root-to-leaf descent. A miss and an error release
    /// the guard (RAII).
    pub async fn get_cache(
        &self,
        table: usize,
        key: &[u8],
    ) -> Result<Option<(Guard<D>, CacheValue)>, StoreError> {
        let (g, dv, root, bound) = {
            let mut inner = self.inner.borrow_mut();
            let Some((g, dv, root)) = inner.core.get_step(table, key) else {
                return Err(StoreError::NoTable);
            };
            (g, dv, root, inner.core.bound())
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket: g,
        };
        match dv {
            Some(v) => Ok(Some((guard, v))),
            None => {
                if root == 0 {
                    return Ok(None);
                }
                // The descent: a page a level, the fuel the world's
                // pages — a deeper tree can only be corruption.
                let mut p = root;
                let mut fuel = bound;
                let hit = loop {
                    if fuel == 0 {
                        return Err(StoreError::Steps("tree: steps"));
                    }
                    fuel -= 1;
                    let node = blockcache::read::<D, _>(&self.inner, p, parse_node).await?;
                    if node.level == 0 {
                        break leaf_find(&node, key).map(|(mt, vr)| (mt.to_vec(), vr));
                    }
                    match route(&node, key) {
                        Some(q) => p = q,
                        None => break None, // below the tree's smallest key
                    }
                };
                match hit {
                    None => Ok(None),
                    Some((mt, 0)) => Ok(Some((
                        guard,
                        CacheValue {
                            mt,
                            pages: Vec::new(),
                        },
                    ))),
                    Some((mt, vr)) => {
                        let pages = self.walk_value(vr, bound).await?;
                        Ok(Some((guard, CacheValue { mt, pages })))
                    }
                }
            }
        }
    }

    /// Walk a value's chain (pages by their `next` headers). The bound
    /// is the world's pages: a longer value chain can only be
    /// corruption.
    pub(crate) async fn walk_value(
        &self,
        first: PageId,
        bound: u64,
    ) -> Result<Vec<PageId>, StoreError> {
        let mut out = Vec::new();
        let mut p = first;
        let mut fuel = bound;
        loop {
            if fuel == 0 {
                return Err(StoreError::Steps("value: steps"));
            }
            fuel -= 1;
            out.push(p);
            let next = blockcache::read::<D, _>(&self.inner, p, |b| val_split(b).0).await?;
            if next == 0 {
                return Ok(out);
            }
            p = next;
        }
    }

    /// Publication: stitch the value's pages, then the seal — the delta
    /// and the deaths in one borrow (a two-step death = corruption). A
    /// refused seal (a disagreed freed list, an unknown table) registers
    /// nothing; the drafts stay held.
    pub async fn put_cache(
        &self,
        table: usize,
        key: &[u8],
        mt: &[u8],
        consumed: &[DraftId],
        freed: &[PageId],
    ) -> Result<(), StoreError> {
        let pages = {
            let inner = self.inner.borrow();
            inner
                .core
                .pages_of(consumed)
                .ok_or(StoreError::UnknownDraft)?
        };
        self.link_all(&pages).await?;
        let mut inner = self.inner.borrow_mut();
        inner.core.seal_step(table, key, mt, consumed, freed)
    }

    /// Stitch a value's chain: the pages' next headers in consumed
    /// order.
    async fn link_all(&self, pages: &[PageId]) -> Result<(), StoreError> {
        for (i, &p) in pages.iter().enumerate() {
            let next = pages.get(i + 1).copied().unwrap_or(0);
            let last = i + 1 == pages.len();
            blockcache::update::<D, _>(&self.inner, p, move |b| {
                link_val(b, next, last);
            })
            .await?;
        }
        Ok(())
    }
}
