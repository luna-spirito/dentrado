//! The store's core — the flush-agnostic half of the single lock's
//! content. The five machines it composes are its own: nobody outside
//! reaches a field — the layers (the store's surface
//! [`crate::store`], the persistence overlay [`crate::persist`], the
//! blockcache's desk) drive the core through its verbs, each an
//! atomic section: a pure transition, one decision and one mutation
//! in a piece. The small machines live each in its own module
//! ([`freelist`], [`ebr`], [`tables`]); the draft arena is defined
//! here; the page codecs beside them ([`val`], [`tree`],
//! [`sb`], `freelist::chunk`). The lock's other half — the
//! persistence overlay — in [`crate::persist`].

mod ebr;
pub mod freelist;
pub mod sb;
pub mod tables;
pub mod tree;
pub mod val;

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::task::Context;

use synchrony::unsync::event::EventListener;

use self::ebr::Deaths;
use self::freelist::{ChainEntry, FreeAct, Frees};
use self::sb::Sb;
pub(crate) use self::tables::Delta;
use self::tables::Tables;
use crate::blockcache::{BlockCache, Reply, Submit};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{Page, PageId};

// ## Types

/// A draft's key in the store's arena.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DraftId(u64);

/// A cache entry: inline bytes and the value's pages (chain order).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CacheValue {
    pub mt: Vec<u8>,
    pub pages: Vec<PageId>,
}

/// One draft-allocation step: the outcome the async surface acts on.
/// `Park` carries the listener (the parking decision and the
/// registration share the section).
pub(crate) enum AllocAct {
    Got(DraftId),
    Park(EventListener),
    Read { ep: usize, e: ChainEntry },
}

// REVIEW 8: Слишком много полей у структуры. Необходимо разбить на несколько логических куско, если это возможно.
// ANSWER: split realized — the five machines by concern (`bc` —
// buffers, `frees` — page dispensing, `deaths` — deaths, `drafts` —
// drafts, `cache` — tree+delta) are the core; the two globals
// (the round agent's mailbox, `gen`) left for the persistence overlay
// ([`crate::persist::PersistentCore`]). The machines are closed
// behind the core's verbs below.
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

/// The flush seal's take: everything a round starts from, out in one
/// section — the flush pool taken whole (head, tail entry, counter)
/// with the retiring containers (the task-eaten ones and both pools'
/// fronts, folded into `spent` by [`Frees::seal`]), the task pool
/// copied (head, bookmark, counter), the deaths pile, the pendings
/// copy, the drafts' pages copy, the delta snapshots, the table roots.
pub(crate) struct Sealed {
    pub(crate) ep: usize,
    /// A — the flush pool, taken whole.
    pub(crate) ahead: Vec<PageId>,
    pub(crate) atail: ChainEntry,
    pub(crate) anext: PageId,
    /// The containers retiring into this round's cells.
    pub(crate) spent: Vec<PageId>,
    /// B — the task pool, copied.
    pub(crate) bhead: Vec<PageId>,
    pub(crate) btail: ChainEntry,
    pub(crate) bnext: PageId,
    pub(crate) dying: Vec<PageId>,
    pub(crate) pend: Vec<PageId>,
    pub(crate) drafts: Vec<PageId>,
    pub(crate) snaps: Vec<Delta>,
    pub(crate) roots: Vec<PageId>,
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
    /// The deltas' value pages — the uncommitted half of the tables.
    pub(crate) delta_pages: Vec<PageId>,
    pub(crate) delta: usize,
    pub(crate) dying: usize,
}

/// The draft arena: keys → (page, fresh — never yet written). A page
/// enters as a fresh draft and leaves by publication or death.
struct Drafts {
    next: u64,
    map: HashMap<DraftId, (PageId, bool)>,
}

impl Drafts {
    fn fresh() -> Self {
        Self {
            next: 0,
            map: HashMap::new(),
        }
    }

    /// A page enters the arena as a fresh draft — the key out.
    fn take(&mut self, p: PageId) -> DraftId {
        let k = DraftId(self.next);
        self.next += 1;
        self.map.insert(k, (p, true));
        k
    }

    fn get(&self, k: &DraftId) -> Option<(PageId, bool)> {
        self.map.get(k).copied()
    }

    /// The first write went through — no longer fresh (an overwrite of
    /// the slot's garbage).
    fn defresh(&mut self, k: &DraftId) {
        if let Some(e) = self.map.get_mut(k) {
            e.1 = false;
        }
    }

    fn owns_all(&self, cs: &[DraftId]) -> bool {
        cs.iter().all(|c| self.map.contains_key(c))
    }

    /// Does the arena still hold this page?
    fn has_page(&self, p: PageId) -> bool {
        self.map.values().any(|&(q, _)| q == p)
    }

    /// The consumed drafts' pages — `None` if any has vanished.
    fn pages_of(&self, cs: &[DraftId]) -> Option<Vec<PageId>> {
        cs.iter()
            .map(|c| self.map.get(c).map(|&(p, _)| p))
            .collect()
    }

    fn remove(&mut self, k: &DraftId) -> Option<(PageId, bool)> {
        self.map.remove(k)
    }

    /// The live drafts' pages (the invariant's inspection).
    fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.map.values().map(|&(p, _)| p)
    }
}

impl<D: IsFilesystem> Core<D> {
    pub(crate) fn fresh(file: D::File, cap: usize, n_tables: usize) -> Self {
        // The arena's pages, born here at the top and shared downward: a
        // flight's lease is an `Rc` to these very pages, so the bytes
        // stay put for as long as the kernel holds them — through any
        // store section, and past the store's own death: a cancelled
        // op's buffer stays compio's until the op is reaped.
        let pages: Rc<[UnsafeCell<Page>]> =
            (0..cap).map(|_| UnsafeCell::new(Page::zeroed())).collect();
        Self {
            bc: BlockCache::new(file, pages),
            frees: Frees::fresh(),
            drafts: Drafts::fresh(),
            deaths: Deaths::fresh(),
            tables: Tables::fresh(n_tables),
        }
    }

    /// Recovery over a live store: the image's fields go on top; the
    /// block cache is not touched (the validation chains are read
    /// directly).
    pub(crate) fn of_sb(&mut self, sb: &Sb) {
        self.frees = Frees::of_sb(sb.e0, sb.next0, sb.e1, sb.next1);
        self.drafts = Drafts::fresh();
        self.deaths = Deaths::fresh();
        self.tables = Tables::of_image(&sb.roots);
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

    /// The `put_cache` seal: the delta + death registration — one
    /// section.
    pub(crate) fn seal_step(
        &mut self,
        t: usize,
        k: &[u8],
        mt: &[u8],
        pages: &[PageId],
        consumed: &[DraftId],
        freed: &[PageId],
    ) -> bool {
        if !self.drafts.owns_all(consumed) {
            return false;
        }
        // The double-registration family: a page both consumed and
        // freed is simultaneously sealed into the tree and registered
        // as a death — freelist corruption.
        debug_assert!(
            pages.iter().all(|p| !freed.contains(p)),
            "a page both consumed and freed"
        );
        debug_assert!(
            freed.iter().all(|p| !self.drafts.has_page(*p)),
            "a freed page still held as a draft"
        );
        self.deaths.reg(freed);
        self.tables.put(
            t,
            k,
            CacheValue {
                mt: mt.to_vec(),
                pages: pages.to_vec(),
            },
        );
        for c in consumed {
            self.drafts.remove(c);
        }
        true
    }

    /// The pin + the `get_cache` decision: the guard — the subscription
    /// bucket; the delta before the tree.
    pub(crate) fn get_step(&mut self, t: usize, k: &[u8]) -> (usize, Option<CacheValue>, PageId) {
        (
            self.deaths.pin(),
            self.tables.get(t, k),
            self.tables.roots[t],
        )
    }

    /// The guard away.
    pub(crate) fn drop_step(&mut self, b: usize) {
        self.deaths.unpin(b);
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

    /// The consumed drafts' pages — `None` if any has vanished.
    pub(crate) fn pages_of(&self, cs: &[DraftId]) -> Option<Vec<PageId>> {
        self.drafts.pages_of(cs)
    }

    // ## The flush round's sections

    /// The flush seal: the round's whole starting world out in one
    /// section — see [`Sealed`].
    pub(crate) fn seal(&mut self) -> Sealed {
        let ep = self.frees.ep();
        let (ahead, atail, anext, spent) = self.frees.seal();
        Sealed {
            ep,
            ahead,
            atail,
            anext,
            spent,
            bhead: self.frees.head(ep).to_vec(),
            btail: self.frees.task_tail(),
            bnext: self.frees.next_of(ep),
            dying: self.deaths.take(),
            pend: self.deaths.pendings(),
            drafts: self.drafts.pages().collect(),
            snaps: self.tables.snap(),
            roots: self.tables.roots.clone(),
        }
    }

    /// The taken pool's husk — the retires that arrived during the
    /// round. Their listings retire with this round's fronts, so the
    /// flight re-lists them at its snapshot.
    pub(crate) fn husk(&self) -> Vec<PageId> {
        self.frees.head(1 - self.frees.ep()).to_vec()
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

    /// The flush commit (the SB is synced): the flight's pool lands
    /// back — the bookmark at the free run's head, the front — the
    /// limbo chunks, the head — the husk plus the stock surplus; the
    /// copied pool keeps its head and bookmark, gaining this round's
    /// deaths of its own parity into the head and the copy chunks as
    /// its front; the rebuilt tables' old pages register as deaths (the
    /// EBR shepherds them past the readers mid-descent); the deltas
    /// purge the sealed, the tables move to the new roots.
    pub(crate) fn commit(
        &mut self,
        deaths: impl Iterator<Item = PageId>,
        old_tree: &[PageId],
        entry: ChainEntry,
        fnext: PageId,
        surplus: &[PageId],
        fronts: [&[PageId]; 2],
        snaps: &[Delta],
        roots: &[PageId],
    ) {
        self.deaths.reg(old_tree);
        self.frees.land(deaths, entry, fnext, surplus, fronts);
        self.tables.commit(snaps);
        self.tables.roots.copy_from_slice(roots);
    }

    // ## The desk's doors (the io through the cache below)

    /// One wish against a page: served on the spot, or seated in its
    /// queue (the desk wraps the answer).
    pub(crate) fn request<R>(
        &mut self,
        id: PageId,
        load: bool,
        f: impl FnOnce(&mut Page) -> (bool, R) + 'static,
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
            delta_pages: self.tables.pages().collect(),
            delta: self.tables.len(),
            dying: self.deaths.len(),
        }
    }
}
