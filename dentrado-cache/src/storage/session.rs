//! The write session — the one door to the store's pages. Allocation,
//! the writes, the reads of one's own pages, the staged records: all
//! live inside a session, and the world changes only at its
//! [`commit`](Session::commit) — one section, the whole batch (every
//! table's deltas, the drafts' publication, the deaths) lands together
//! or not at all. A session dropped uncommitted is a clean abort: its
//! drafts go back to their pools whole, nothing it staged ever ran.
//!
//! A session holds no pins and no snapshots: the store is
//! single-threaded by construction, a commit is atomic in itself, and
//! a pin held across a session's life would stall the death drain for
//! the session's whole duration (every flush of a long session would
//! pile pendings). Uncommitted stages are invisible to the store's
//! reads — the standing world is the only world until the commit.
//!
//! [`DraftPageId`] is an index into the session's own ledger, checked
//! at every use — a dead or twice-taken draft refuses honestly. (A
//! lifetime brand that makes a foreign session's id unexpressible
//! needs generativity machinery the win does not justify; the per-use
//! checks carry the honesty here.)

use std::cell::RefCell;
use std::rc::Rc;

use super::Storage;
use crate::blockcache;
use crate::core::tree::leaf_holds;
use crate::core::{AllocAct, SealBatch, SealRec};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{AlignedPage, PageId};
use crate::persist::PersistentCore;

/// A session's own draft — an index into its ledger, checked at every
/// use. Ids die with their session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DraftPageId {
    i: usize,
}

/// One staged record: the table it belongs to (the id, re-resolved at
/// the commit — a dropped table refuses then), the key, the record's
/// inline bytes (`None` — a deletion), the drafts it publishes, the
/// pages that die with it.
struct Staged {
    t: Vec<u8>,
    k: Vec<u8>,
    put: Option<Vec<u8>>,
    take: Vec<usize>,
    free: Vec<PageId>,
}

/// A write session over the store — see the [module doc](self).
pub struct Session<D: IsFilesystem> {
    inner: Rc<RefCell<PersistentCore<D>>>,
    /// The session's drafts: index → (page, written at least once).
    /// A dead draft (discarded, or a replaced record's take) keeps its
    /// index — the ids already handed out stay honest.
    drafts: Vec<Option<(PageId, bool)>>,
    /// Which ledger slots a staged record has taken — a draft is taken
    /// at most once.
    taken: Vec<bool>,
    /// The staged records — one per (table, key): a later stage of the
    /// same key replaces the earlier (its takes die, the frees union).
    staged: Vec<Staged>,
}

/// A tail-read's rollback, RAII: a Busy tail without its reader bricks
/// the pool — every later alloc parks on a splice that never comes.
/// Defused once the read lands and the splice follows in the same
/// section; drops otherwise (an io failure's `?` or the caller's
/// cancellation between the section and the splice).
struct ReadRollback<D: IsFilesystem> {
    inner: Rc<RefCell<PersistentCore<D>>>,
    ep: usize,
    e: crate::core::freelist::ChainEntry,
    armed: bool,
}

impl<D: IsFilesystem> ReadRollback<D> {
    fn new(
        inner: Rc<RefCell<PersistentCore<D>>>,
        ep: usize,
        e: crate::core::freelist::ChainEntry,
    ) -> Self {
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

impl<D: IsFilesystem> Storage<D> {
    /// A new write session — see [`session`](self).
    pub fn session(&self) -> Session<D> {
        Session {
            inner: self.inner.clone(),
            drafts: Vec::new(),
            taken: Vec::new(),
            staged: Vec::new(),
        }
    }
}

impl<D: IsFilesystem> Session<D> {
    // ## The drafts

    /// Allocate a page — the session's own draft. The pool's slow path
    /// (a head run dry) eats the pool's tail chain first.
    pub async fn alloc(&mut self) -> Result<DraftPageId, StoreError> {
        let bound = {
            let inner = self.inner.borrow();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
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
                if inner.poisoned {
                    return Err(StoreError::Poisoned);
                }
                inner.core.alloc_step()
            }?;
            match act {
                AllocAct::Got(p) => {
                    self.drafts.push(Some((p, false)));
                    self.taken.push(false);
                    return Ok(DraftPageId {
                        i: self.drafts.len() - 1,
                    });
                }
                AllocAct::Park(ln) => ln.await,
                AllocAct::Read { ep, e } => {
                    // The read may die (an io failure's `?`) or be
                    // cancelled (the caller's future dropped mid-await);
                    // either way the Busy tail must not outlive its
                    // reader — the guard rolls it back and wakes the
                    // parked, unless defused at the splice.
                    let rb = ReadRollback::new(self.inner.clone(), ep, e);
                    let (cells, nx) = blockcache::read::<D, _>(&self.inner, e.page, move |b| {
                        (
                            crate::core::freelist::chunk_cells(b, e.off as usize),
                            crate::core::freelist::chunk_next(b),
                        )
                    })
                    .await?;
                    rb.defuse();
                    let mut inner = self.inner.borrow_mut();
                    inner.core.splice(ep, e, cells, nx);
                }
            }
        }
    }

    /// The draft's page — the id a record's own bytes name (a chain
    /// head, a shared closure's member).
    pub fn page(&self, id: DraftPageId) -> Result<PageId, StoreError> {
        self.slot(id).map(|(p, _)| p)
    }

    /// Write into a draft — zero-copy: the closure shapes the page in
    /// the arena, the store writes nothing itself. A fresh draft is
    /// not read from disk (the write lands over the slot's garbage);
    /// a rewrite goes through the page's current bytes.
    pub async fn write<R>(
        &mut self,
        id: DraftPageId,
        f: impl Fn(&mut AlignedPage) -> R + 'static,
    ) -> Result<R, StoreError>
    where
        R: 'static,
    {
        let (p, written) = self.slot(id)?;
        let r = if written {
            blockcache::update::<D, _>(&self.inner, p, f).await?
        } else {
            blockcache::init::<D, _>(&self.inner, p, f).await?
        };
        if !written {
            self.drafts[id.i].as_mut().expect("the slot was alive").1 = true;
        }
        Ok(r)
    }

    /// Read a page by reference — the session's own drafts or a page
    /// its records own (the store does not police the id; reading a
    /// never-written draft is the caller's nonsense — pool noise).
    pub async fn read<R>(
        &self,
        p: PageId,
        f: impl Fn(&AlignedPage) -> R + 'static,
    ) -> Result<R, StoreError>
    where
        R: 'static,
    {
        if self.inner.borrow().poisoned {
            return Err(StoreError::Poisoned);
        }
        blockcache::read::<D, _>(&self.inner, p, f).await
    }

    /// A draft dies before its publication: its page goes back to its
    /// pool whole. A taken draft refuses (a staged record owns it).
    pub fn discard(&mut self, id: DraftPageId) -> Result<(), StoreError> {
        let p = self.slot(id)?.0;
        self.kill(id.i, p);
        Ok(())
    }

    // ## The stages

    /// Stage a record: the value is the caller's inline bytes, `take`
    /// — the drafts this record publishes (its own pages), `free` —
    /// the pages that die with it (the replaced record's closure, the
    /// caller's ledger vouches). A later stage of the same key
    /// replaces the earlier — its takes die, the frees union.
    pub fn put(
        &mut self,
        table: &[u8],
        key: &[u8],
        val: &[u8],
        take: &[DraftPageId],
        free: &[PageId],
    ) -> Result<(), StoreError> {
        self.stage(table, key, Some(val), take, free)
    }

    /// Stage a deletion: `free` — the dying record's pages.
    pub fn del(&mut self, table: &[u8], key: &[u8], free: &[PageId]) -> Result<(), StoreError> {
        self.stage(table, key, None, &[], free)
    }

    fn stage(
        &mut self,
        table: &[u8],
        key: &[u8],
        val: Option<&[u8]>,
        take: &[DraftPageId],
        free: &[PageId],
    ) -> Result<(), StoreError> {
        {
            let inner = self.inner.borrow();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            if inner.core.table_index(table).is_none() {
                return Err(StoreError::NoTable);
            }
        }
        if let Some(v) = val {
            if !leaf_holds(key, v) {
                return Err(StoreError::BadKey);
            }
        }
        // validate every take before any is marked (a refusal leaves
        // the session as it was)
        for id in take {
            self.slot(*id)?;
            if self.taken[id.i] {
                return Err(StoreError::BadSeal("a draft taken twice"));
            }
        }
        let mut ts = Vec::with_capacity(take.len());
        for id in take {
            self.taken[id.i] = true;
            ts.push(id.i);
        }
        // a replaced stage: its takes die, its frees ride along
        let mut free = free.to_vec();
        if let Some(at) = self.staged.iter().position(|s| s.t == table && s.k == key) {
            let old = self.staged.remove(at);
            for i in old.take {
                self.taken[i] = false;
                if let Some((p, _)) = self.drafts[i] {
                    self.kill(i, p);
                }
            }
            free.extend(old.free);
        }
        self.staged.push(Staged {
            t: table.to_vec(),
            k: key.to_vec(),
            put: val.map(|v| v.to_vec()),
            take: ts,
            free,
        });
        Ok(())
    }

    /// Commit: everything staged lands in one atomic section — every
    /// table's deltas, the drafts' publication, the deaths. Every
    /// live draft must be published by some staged record or already
    /// dead (an untaken draft at the commit is the caller's leak —
    /// refused, not laundered). A refusal lands nothing; the session
    /// stays whole for a retry or a clean abort.
    pub fn commit(&mut self) -> Result<(), StoreError> {
        let leak = |(i, d): (usize, &Option<(PageId, bool)>)| d.is_some() && !self.taken[i];
        if self.drafts.iter().enumerate().any(leak) {
            return Err(StoreError::BadSeal("an untaken draft at the commit"));
        }
        let r = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            let batches: Vec<SealBatch> = self
                .staged
                .iter()
                .map(|s| {
                    Ok(SealBatch {
                        t: inner.core.table_index(&s.t).ok_or(StoreError::NoTable)?,
                        recs: vec![SealRec {
                            k: s.k.clone(),
                            put: s.put.clone(),
                            consumed: self.pages_of(&s.take),
                        }],
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            let freed: Vec<PageId> = self
                .staged
                .iter()
                .flat_map(|s| s.free.iter().copied())
                .collect();
            inner.core.seal(&batches, &freed)
        };
        r?;
        self.drafts.clear();
        self.taken.clear();
        self.staged.clear();
        Ok(())
    }

    // ## The ledger

    /// A live slot's page — a dead or foreign index refuses.
    fn slot(&self, id: DraftPageId) -> Result<(PageId, bool), StoreError> {
        self.drafts
            .get(id.i)
            .copied()
            .flatten()
            .ok_or(StoreError::UnknownDraft)
    }

    /// A slot dies: out of the ledger, the page back to its pool.
    fn kill(&mut self, i: usize, p: PageId) {
        self.drafts[i] = None;
        self.inner.borrow_mut().core.dealloc(p);
    }

    /// The staged takes' pages — in stage order.
    fn pages_of(&self, ts: &[usize]) -> Vec<PageId> {
        ts.iter()
            .map(|&i| self.drafts[i].expect("a taken slot is alive").0)
            .collect()
    }
}

impl<D: IsFilesystem> Drop for Session<D> {
    /// The uncommitted session is a clean abort: every live draft goes
    /// back to its pool whole, nothing staged ever ran.
    fn drop(&mut self) {
        let mut inner = self.inner.borrow_mut();
        for d in self.drafts.drain(..) {
            if let Some((p, _)) = d {
                inner.core.dealloc(p);
            }
        }
    }
}
