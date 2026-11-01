//! The page dispenser: two pools — even pages and odd — each a RAM
//! head, a tail-chain of chunk pages on disk, and its own never-issued
//! counter (steps of 2 in its parity — parity names the issuing pool
//! forever, so a retired page always knows its way home). The epoch
//! says which pool serves the tasks; the opposite one is the flush's,
//! taken whole — head, tail, counter and front — by the flush's seal
//! and rebuilt by its commit.
//!
//! A pool's chain hangs in two regions. The front — between the
//! image's entry and the tail's bookmark — holds cells that are
//! RAM-side (the head materializes them) or re-copied every round
//! (pendings, drafts); the pool's next writing retires it wholesale.
//! The free run — at and below the bookmark — holds cells written once
//! and only ever eaten: consumption starts there, so the round's fresh
//! free pages serve first and the deep stock is never rewritten. The
//! chunk containers the tasks eat ride `spent` until the next seal
//! retires them into its cells.
//!
//! The chain's page format is the freelist's own private business —
//! it lives beside the machine, in [`chunk`].

use synchrony::unsync::event::{Event, EventListener};

use crate::error::StoreError;
use crate::page::PageId;

mod chunk;

pub use self::chunk::{CHUNK_SLOTS, ChainEntry, chunk_cells, chunk_next, write_chunk};

/// A freelist's tail: free (the entry), being read by a task (the
/// parked listen on the event — everyone woken re-runs its step), or
/// taken by the flush.
enum Tail {
    Free(ChainEntry),
    Busy(ChainEntry, Event),
    Taken,
}

/// One pool's freelist: the head in RAM, the tail on disk, and the
/// pool's own never-issued bound. The head is a bag of pages of the
/// pool's parity; the issue order carries no meaning.
pub(super) struct Freelist {
    head: Vec<PageId>,
    tail: Tail,
    /// The front chunks' container pages — between the image's entry
    /// and the tail's bookmark, their cells RAM-side (the head
    /// materializes them) or re-copied every round (pendings, drafts).
    /// The pool's next writing retires them into its cells.
    front: Vec<PageId>,
    next: PageId,
}

impl Freelist {
    fn empty(next: PageId) -> Self {
        Self {
            head: Vec::new(),
            tail: Tail::Free(ChainEntry::default()),
            front: Vec::new(),
            next,
        }
    }

    /// The tail's entry — where the uneaten chain begins; a taken tail
    /// has none (the flush owns the chain).
    fn tail_entry(&self) -> ChainEntry {
        match &self.tail {
            Tail::Free(e) | Tail::Busy(e, _) => *e,
            Tail::Taken => ChainEntry::default(),
        }
    }
}

/// One draft-allocation step's outcome — the pools' half; the draft
/// bookkeeping around the got page is `Drafts`' own. `Park` carries
/// the listener — the decision and the registration share one section,
/// so a splice can never slip past a parking task.
pub(super) enum FreeAct {
    Got(PageId),
    Park(EventListener),
    Read { ep: usize, e: ChainEntry },
}

/// The two-pool freelist — the world's page dispenser. Each pool grows
/// only its own counter (+2, its parity): a task's grow hands the page
/// straight out, a flush round's grow is flight-local — neither side
/// ever touches the other's pool.
pub(super) struct Frees {
    /// Which pool serves the tasks (pool 0 — even pages, 1 — odd).
    epoch: bool,
    pools: [Freelist; 2],
    /// Chunk containers the tasks ate since the last seal — retired
    /// into the next round's cells (until that commit the image's
    /// chain still walks through them, so they may not serve yet).
    spent: Vec<PageId>,
}

impl Frees {
    pub(super) fn fresh() -> Self {
        // The counters start past the SB slots (pages 0 and 1).
        Self {
            epoch: false,
            pools: [Freelist::empty(2), Freelist::empty(3)],
            spent: Vec::new(),
        }
    }

    /// Restart from the image's chains and counters.
    pub(super) fn of_sb(e0: ChainEntry, n0: PageId, e1: ChainEntry, n1: PageId) -> Self {
        Self {
            epoch: false,
            pools: [
                Freelist {
                    head: Vec::new(),
                    tail: Tail::Free(e0),
                    front: Vec::new(),
                    next: n0,
                },
                Freelist {
                    head: Vec::new(),
                    tail: Tail::Free(e1),
                    front: Vec::new(),
                    next: n1,
                },
            ],
            spent: Vec::new(),
        }
    }

    /// The task pool's index (0 — even, 1 — odd).
    pub(super) fn ep(&self) -> usize {
        self.epoch as usize
    }

    /// The world's page-id bound — every issued id is below it (the
    /// flight's own counter included by the caller while a round runs).
    pub(super) fn bound(&self) -> u64 {
        self.pools[0].next.max(self.pools[1].next) + 1
    }

    /// A pool's never-issued bound (a round's flight holds the sealed
    /// pool's live one — the husk keeps the seal-time copy).
    pub(super) fn next_of(&self, p: usize) -> PageId {
        self.pools[p].next
    }

    /// A pool's RAM head.
    pub(super) fn head(&self, p: usize) -> &[PageId] {
        &self.pools[p].head
    }

    /// The task pool's tail entry — where its uneaten chain begins.
    pub(super) fn task_tail(&self) -> ChainEntry {
        self.tail_of(self.ep())
    }

    /// A pool's tail entry — where its uneaten chain begins (an empty
    /// run: `page = 0` — the chain is eaten to the end).
    pub(super) fn tail_of(&self, p: usize) -> ChainEntry {
        self.pools[p].tail_entry()
    }

    /// Is the pool's tail taken by a flush?
    pub(super) fn taken(&self, p: usize) -> bool {
        matches!(self.pools[p].tail, Tail::Taken)
    }

    /// The containers the tasks ate since the last seal — held until
    /// the next round lists them as its cells.
    pub(super) fn spent(&self) -> &[PageId] {
        &self.spent
    }

    /// One allocation step: own pool's head → the tail chain (a single
    /// reader, the rest park) → growing the world (the pool's own
    /// counter, +2 — out).
    pub(super) fn alloc(&mut self) -> Result<FreeAct, StoreError> {
        let ep = self.ep();
        if let Some(p) = self.pools[ep].head.pop() {
            return Ok(FreeAct::Got(p));
        }
        match self.pools[ep].tail {
            Tail::Taken => Err(StoreError::PoolTaken),
            Tail::Busy(_, ref ev) => Ok(FreeAct::Park(ev.listen())),
            Tail::Free(e) if e.page == 0 => {
                let n = self.pools[ep].next;
                self.pools[ep].next = n + 2;
                Ok(FreeAct::Got(n))
            }
            Tail::Free(e) => {
                self.pools[ep].tail = Tail::Busy(e, Event::new());
                Ok(FreeAct::Read { ep, e })
            }
        }
    }

    /// Splice in the read chunk: the cells — into the head, the tail —
    /// onward, the container — into `spent` (the image's chain still
    /// walks through it, so it retires only with the next round's
    /// cells); the parked steps re-run — the wake is at most spurious,
    /// never lost. A tail already not `busy e` (the flush took the next
    /// seal after the epoch flip) — a no-op.
    pub(super) fn splice(&mut self, ep: usize, e: ChainEntry, cells: Vec<PageId>, nx: PageId) {
        let old = std::mem::replace(&mut self.pools[ep].tail, Tail::Taken);
        match old {
            Tail::Busy(e2, ev) if e2 == e => {
                self.pools[ep].head.splice(0..0, cells);
                self.pools[ep].tail = Tail::Free(ChainEntry::full(nx));
                self.spent.push(e.page);
                ev.notify_all();
            }
            other => {
                self.pools[ep].tail = other;
            }
        }
    }

    /// A retired task-parity page — back into its own pool's head.
    pub(super) fn retire(&mut self, p: PageId) {
        self.pools[(p % 2) as usize].head.push(p);
    }

    /// The flush seal: take the flush pool whole — its head, the entry
    /// into its uneaten tail, its counter — plus the retiring
    /// containers: the task-eaten ones and both pools' fronts (the
    /// copied pool's front retires with this round's copy chunks too);
    /// a husk remains (empty head, `taken` tail, the seal-time counter
    /// — only a retire may still drop arrivals into the head).
    pub(super) fn seal(&mut self) -> (Vec<PageId>, ChainEntry, PageId, Vec<PageId>) {
        let nep = 1 - self.ep();
        let n = self.pools[nep].next;
        let old = std::mem::replace(&mut self.pools[nep], Freelist::empty(n));
        self.pools[nep].tail = Tail::Taken;
        let tail = old.tail_entry();
        let mut spent = std::mem::take(&mut self.spent);
        spent.extend(old.front);
        spent.extend(std::mem::take(&mut self.pools[1 - nep].front));
        (old.head, tail, old.next, spent)
    }

    /// The flush commit (the SB is synced): the flight's pool comes
    /// back — the bookmark at the free run's head (the fresh free
    /// cells serve first), the front — the limbo chunks, the head —
    /// the husk plus the stock's surplus (popping it out of the head
    /// shrank the cells, so the need sank under the taken; the
    /// flush-parity deaths ride the chain); the task pool keeps its
    /// head and bookmark, gaining this round's deaths of its own
    /// parity into the head and the copy chunks as its front; the
    /// epoch flips.
    pub(super) fn land(
        &mut self,
        deaths: impl Iterator<Item = PageId>,
        entry: ChainEntry,
        fnext: PageId,
        surplus: &[PageId],
        fronts: [&[PageId]; 2],
    ) {
        let ep = self.ep();
        let nep = 1 - ep;
        let mut husk = std::mem::take(&mut self.pools[nep].head);
        husk.extend_from_slice(surplus);
        self.pools[nep] = Freelist {
            head: husk,
            tail: Tail::Free(entry),
            front: fronts[nep].to_vec(),
            next: fnext,
        };
        for p in deaths {
            if (p % 2) as usize == ep {
                self.pools[ep].head.push(p);
            }
        }
        self.pools[ep].front = fronts[ep].to_vec();
        self.epoch = !self.epoch;
    }
}
