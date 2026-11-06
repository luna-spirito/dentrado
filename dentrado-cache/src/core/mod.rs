//! The store's core — the flush-agnostic half of the single lock's
//! content. The five machines it composes are its own: nobody outside
//! reaches a field — the layers (the store's surface
//! [`crate::storage`], the persistence overlay [`crate::persist`], the
//! blockcache's desk) drive the core through its verbs, each an
//! atomic section: a pure transition, one decision and one mutation
//! in a piece. The small machines live each in its own module
//! ([`freelist`], [`ebr`], [`tables`], [`drafts`]); the store's own
//! page codecs beside them ([`tree`], [`sb`], `freelist::chunk`); the
//! flush round's sections in [`flush_round`]. The lock's other half —
//! the persistence overlay — in [`crate::persist`].

mod drafts;
mod ebr;
pub mod flush_round;
pub mod freelist;
pub mod sb;
pub mod tables;
pub mod tree;

#[cfg(test)]
mod tests;

use std::cell::UnsafeCell;
use std::rc::Rc;
use std::task::Context;

use synchrony::unsync::event::EventListener;

pub use self::drafts::DraftId;
use self::drafts::Drafts;
use self::ebr::Deaths;
use self::freelist::{ChainEntry, FreeAct, Frees};
use self::sb::Sb;
use self::tables::Tables;
pub(crate) use self::tables::{Delta, DeltaV};
use crate::blockcache::{BlockCache, Reply, Submit};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{AlignedPage, PageId};
use crate::schema::{TableSchema, TableSpec};

// ## Types

/// One draft-allocation step: the outcome the async surface acts on.
/// `Park` carries the listener (the parking decision and the
/// registration share the section).
pub(crate) enum AllocAct {
    Got(DraftId),
    Park(EventListener),
    Read { ep: usize, e: ChainEntry },
}

/// The delta's answer for one key — [`Core::get_step`]'s second half.
pub(crate) enum StepHit {
    /// A delta entry — a record's inline bytes on their way in.
    Put(Vec<u8>),
    /// A delta deletion — the key is gone even if the tree still has
    /// it.
    Del,
    /// Nothing in the delta — the standing tree decides.
    Tree,
}

/// One record of a `put_many` batch at its seal: the key, the record's
/// inline bytes (`None` — a deletion), and the drafts it publishes
/// (the caller's own — empty for an inline-only record or a deletion).
pub(crate) struct SealRec {
    pub(crate) k: Vec<u8>,
    pub(crate) put: Option<Vec<u8>>,
    pub(crate) consumed: Vec<DraftId>,
}

/// The store's core — the flush-agnostic half of the lock's content.
pub(crate) struct Core<D: IsFilesystem> {
    // REVIEW 9: Оно чем-то кроме BlockCache используется вообще? Может, перевести в BlockCache?
    // ANSWER: by no one — the flush's `sync` was the last consumer; the
    // handle now lives in the cache's disk arena, `BlockCache::file()`
    // lends the clone.
    /// The raw block cache — a field, not a neighboring lock.
    bc: BlockCache<D>,
    /// The world's page dispenser (two pools, the epoch between them).
    frees: Frees,
    /// Draft pages awaiting publication.
    drafts: Drafts,
    /// Page deaths: EBR plus the pile the flush seals away.
    deaths: Deaths,
    /// The tables: each table's tree root plus the delta before it.
    tables: Tables,
}

/// The RAM evidence at rest — the invariants' half that lives in the
/// lock: the pools' heads, counters and taken flags, the live drafts'
/// pages, the deaths still held (the pile and the undrained pendings),
/// the sizes the debug surface reports.
pub(crate) struct Ram {
    pub(crate) heads: [Vec<PageId>; 2],
    pub(crate) nexts: [PageId; 2],
    pub(crate) taken: [bool; 2],
    /// The pools' live bookmarks — where each pool's uneaten chain
    /// begins (the partition's region boundary: front above, free run
    /// at-and-below).
    pub(crate) tails: [ChainEntry; 2],
    pub(crate) drafts: Vec<PageId>,
    pub(crate) held: Vec<PageId>,
    /// The task-eaten containers awaiting the next round's listing.
    pub(crate) spent: Vec<PageId>,
    pub(crate) delta: usize,
    pub(crate) dying: usize,
}

impl<D: IsFilesystem> Core<D> {
    pub(crate) fn fresh(file: D::File, cap: usize, specs: &[TableSpec]) -> Self {
        // The arena's pages, born here at the top and shared downward: a
        // flight's lease is an `Rc` to these very pages, so the bytes
        // stay put for as long as the kernel holds them — through any
        // store section, and past the store's own death: a cancelled
        // op's buffer stays compio's until the op is reaped.
        let pages: Rc<[UnsafeCell<AlignedPage>]> = (0..cap)
            .map(|_| UnsafeCell::new(AlignedPage::zeroed()))
            .collect();
        Self {
            bc: BlockCache::new(file, pages),
            frees: Frees::fresh(),
            drafts: Drafts::fresh(),
            deaths: Deaths::fresh(),
            tables: Tables::fresh(specs),
        }
    }

    /// Recovery over a live store: the image's fields go on top; the
    /// block cache is not touched (the validation chains are read
    /// directly). The tables are the union of the image's set and the
    /// opener's — see [`Tables::of_image`].
    pub(crate) fn of_sb(&mut self, sb: &Sb, opener: &[TableSpec]) {
        self.frees = Frees::of_sb(sb.e0, sb.next0, sb.e1, sb.next1);
        self.drafts = Drafts::fresh();
        self.deaths = Deaths::fresh();
        self.tables = Tables::of_image(&sb.tables, opener);
    }

    // ## Atomic sections: pure transitions

    /// One draft-allocation step: the decision and the mutation in one
    /// piece (the pools' half is [`Frees::alloc`]'s own).
    pub(crate) fn alloc_step(&mut self) -> Result<AllocAct, StoreError> {
        match self.frees.alloc()? {
            FreeAct::Got(p) => Ok(AllocAct::Got(self.drafts.take(p))),
            FreeAct::Park(ln) => Ok(AllocAct::Park(ln)),
            FreeAct::Read { ep, e } => Ok(AllocAct::Read { ep, e }),
        }
    }

    /// Splice the read chunk into the task pool: the cells into the
    /// head, the tail onward, the container into `spent`; the parked
    /// steps re-run on the wake.
    pub(crate) fn splice(&mut self, ep: usize, e: ChainEntry, cells: Vec<PageId>, nx: PageId) {
        self.frees.splice(ep, e, cells, nx)
    }

    /// An abandoned tail-read's rollback (the reader's io failed or the
    /// reader itself was cancelled): the Busy tail is Free again — see
    /// [`Frees::unbusy`].
    pub(crate) fn abort_read(&mut self, ep: usize, e: ChainEntry) {
        self.frees.unbusy(ep, e)
    }

    /// A draft's death: unpublished — no gate needed, back into its own
    /// pool's head. An unknown key — an honest refusal.
    pub(crate) fn dealloc_step(&mut self, k: DraftId) -> bool {
        match self.drafts.remove(&k) {
            Some((p, _)) => {
                self.frees.retire(p);
                true
            }
            None => false,
        }
    }

    /// The `put_many` seal: a whole batch's deltas, draft consumption
    /// and death registration — one section, so a batch lands whole or
    /// not at all. The freed pages are the caller's word (the replaced
    /// records' closures — the caller knows them, the store does not
    /// traverse), yet not a free hand: each must be a published page —
    /// never a draft, never dying, never back in a pool, never
    /// conjured — and no page twice. A refusal registers nothing; the
    /// taken drafts stay held (the caller's, to dealloc or reuse).
    pub(crate) fn seal_batch(
        &mut self,
        t: usize,
        recs: &[SealRec],
        freed: &[PageId],
    ) -> Result<(), StoreError> {
        if t >= self.tables.roots.len() {
            return Err(StoreError::NoTable);
        }
        let mut owned = Vec::with_capacity(recs.len());
        for r in recs {
            let Some(pages) = self.drafts.pages_of(&r.consumed) else {
                return Err(StoreError::ConsumedVanished);
            };
            owned.push(pages);
        }
        let bound = self.frees.bound();
        for (i, p) in freed.iter().enumerate() {
            let why = if owned.iter().any(|ps| ps.contains(p)) {
                "a page both consumed and freed"
            } else if self.drafts.has_page(*p) {
                "a freed page still held as a draft"
            } else if freed[..i].contains(p) {
                "a page freed twice"
            } else if self.deaths.holds(*p) {
                "a freed page already dying"
            } else if self.frees.holds(*p) {
                "a freed page back in a pool"
            } else if *p < 2 || *p >= bound {
                "a freed page outside the issued range"
            } else {
                continue;
            };
            return Err(StoreError::BadSeal(why));
        }
        self.deaths.reg(freed);
        for (r, _) in recs.iter().zip(owned) {
            match r.put {
                Some(ref mt) => self.tables.put(t, &r.k, mt.clone()),
                None => self.tables.del(t, &r.k),
            }
            for c in &r.consumed {
                self.drafts.remove(c);
            }
        }
        Ok(())
    }

    /// The pin + the `get` decision: the guard — the subscription
    /// bucket; the delta before the tree, a delta deletion before the
    /// descent. `None` — no such table.
    pub(crate) fn get_step(&mut self, t: usize, k: &[u8]) -> Option<(usize, StepHit, PageId)> {
        let root = *self.tables.roots.get(t)?;
        let hit = match self.tables.get(t, k) {
            Some(DeltaV::Put(mt)) => StepHit::Put(mt),
            Some(DeltaV::Del) => StepHit::Del,
            None => StepHit::Tree,
        };
        Some((self.deaths.pin(), hit, root))
    }

    /// The guard away.
    pub(crate) fn drop_step(&mut self, b: usize) {
        self.deaths.unpin(b);
    }

    // ## The tables' verbs (the named-set surface)

    /// A live id's slot — `None` for an id outside the set or a
    /// dropped slot.
    pub(crate) fn table_index(&self, id: &[u8]) -> Option<usize> {
        self.tables.idx(id)
    }

    /// A slot's id (empty — a dropped slot's tombstone) and the slot
    /// count — the invariants' walk over the live set.
    pub(crate) fn table_name(&self, t: usize) -> &[u8] {
        self.tables.name(t)
    }

    pub(crate) fn roots_len(&self) -> usize {
        self.tables.roots.len()
    }

    /// A slot's standing tree root (0 — empty).
    pub(crate) fn table_root(&self, t: usize) -> PageId {
        self.tables.roots[t]
    }

    /// A slot's schema — how its records name dependent pages.
    pub(crate) fn table_schema(&self, t: usize) -> TableSchema {
        self.tables.schema(t)
    }

    /// A slot's delta records' inline bytes — see
    /// [`Tables::delta_inline`].
    pub(crate) fn table_delta_inline(&self, t: usize) -> Vec<Vec<u8>> {
        self.tables.delta_inline(t)
    }

    /// A bare EBR pin (a scan's multi-page walk) — the guard's bucket.
    pub(crate) fn pin_step(&mut self) -> usize {
        self.deaths.pin()
    }

    /// A slot's delta entries in a key range — a scan's overlay half.
    pub(crate) fn table_delta_range(
        &self,
        t: usize,
        lo: &[u8],
        hi: Option<&[u8]>,
    ) -> Vec<(Vec<u8>, DeltaV)> {
        self.tables.delta_range(t, lo, hi)
    }

    /// The live ids — enumeration for the caller's startup GC.
    pub(crate) fn table_names(&self) -> Vec<Vec<u8>> {
        self.tables.names()
    }

    /// The live `(id, root)` pairs over a roots vector — the SB's
    /// set at a flush's engraving.
    pub(crate) fn live_tables(&self, roots: &[PageId]) -> Vec<(Vec<u8>, PageId)> {
        self.tables.live_with(roots)
    }

    /// Register page deaths outside a seal (a table drop's tree and
    /// value pages).
    pub(crate) fn reg_deaths(&mut self, ps: &[PageId]) {
        self.deaths.reg(ps);
    }

    /// A slot's death — the tombstone, see [`Tables::drop_slot`]. The
    /// pages themselves die by the caller's [`Core::reg_deaths`].
    pub(crate) fn drop_table_slot(&mut self, t: usize) {
        self.tables.drop_slot(t);
    }

    // ## The drafts' verbs (the steps around the io)

    /// A draft's locus: its page and whether it is fresh — never yet
    /// written.
    pub(crate) fn draft(&self, k: &DraftId) -> Option<(PageId, bool)> {
        self.drafts.get(k)
    }

    /// The first write went through — the draft is no longer fresh.
    pub(crate) fn defresh(&mut self, k: &DraftId) {
        self.drafts.defresh(k);
    }

    /// The world's page-id bound — every issued id is below it (a
    /// round's own flight counter included by the caller while it
    /// runs).
    pub(crate) fn bound(&self) -> u64 {
        self.frees.bound()
    }

    /// Is a flush round in flight — a pool's tail is still `Taken`
    /// (from the round's seal to its commit)?
    pub(crate) fn round_live(&self) -> bool {
        self.frees.taken(0) || self.frees.taken(1)
    }

    // ## The desk's doors (the io through the cache below)

    /// One wish against a page: served on the spot, or seated in its
    /// queue (the desk wraps the answer).
    pub(crate) fn request<R>(
        &mut self,
        id: PageId,
        load: bool,
        f: impl FnOnce(&mut AlignedPage) -> (bool, R) + 'static,
    ) -> Submit<R>
    where
        R: 'static,
    {
        self.bc.request(id, load, f)
    }

    /// The page whole on disk (`ensure_saved`).
    pub(crate) fn persist(&mut self, id: PageId, tx: Reply) {
        self.bc.persist(id, tx);
    }

    /// The checkpoint barrier (`write_back_all`).
    pub(crate) fn checkpoint(&mut self, tx: Reply) {
        self.bc.checkpoint(tx);
    }

    /// The driver's tick: poll the wings, land the ready flights —
    /// how many landed.
    pub(crate) fn reap(&mut self, cx: &mut Context<'_>) -> usize {
        self.bc.reap(cx)
    }

    /// The file handle for the flush's syncs (positional io, `Clone`).
    pub(crate) fn file(&self) -> D::File {
        self.bc.file()
    }

    // ## Inspection at rest (the invariants, the debug surface)

    /// The lock's whole RAM state as owned copies — see [`Ram`].
    pub(crate) fn ram(&self) -> Ram {
        Ram {
            heads: [self.frees.head(0).to_vec(), self.frees.head(1).to_vec()],
            nexts: [self.frees.next_of(0), self.frees.next_of(1)],
            taken: [self.frees.taken(0), self.frees.taken(1)],
            tails: [self.frees.tail_of(0), self.frees.tail_of(1)],
            drafts: self.drafts.pages().collect(),
            held: self.deaths.held(),
            spent: self.frees.spent().to_vec(),
            delta: self.tables.len(),
            dying: self.deaths.len(),
        }
    }
}
