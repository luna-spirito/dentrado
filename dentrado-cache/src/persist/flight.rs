//! A flush round's flight — the future's locals: everything the round
//! carries across its await points. Nobody but the executor reads it,
//! a crash swallows it without a trace, and its only touches of
//! foreign state (growing the world, the husk snapshot, the commit)
//! remain atomic sections. The round's body ([`super::flush`]) drives
//! it; the stock fixpoint lives here too.

use std::cell::RefCell;
use std::collections::HashSet;
use std::rc::Rc;

use super::PersistentCore;
use crate::core::Delta;
use crate::core::freelist::{CHUNK_SLOTS, ChainEntry, chunk_cells, chunk_next};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;

/// The seal-time world of the taken pool — the round's rollback
/// restores it wholesale if the round fails; the flight's own copies
/// (the head, the tail, the counter) evolve past it through the round.
pub(crate) struct Home {
    pub(crate) head: Vec<PageId>,
    pub(crate) tail: ChainEntry,
    pub(crate) next: PageId,
}

/// A flush round's flight.
pub(crate) struct Flight {
    pub(crate) ep: usize,
    /// A, taken whole — eaten and grown through the round: the stock
    /// head, the tail-eating position, the counter, the drains.
    pub(crate) head: Vec<PageId>,
    pub(crate) tail: ChainEntry,
    pub(crate) next: PageId,
    pub(crate) drains: Vec<PageId>,
    /// The taken pool's seal-time world (the rollback's anchor).
    pub(crate) home: Home,
    /// The sealed world: the retiring containers, the deaths pile, the
    /// pendings and drafts copies (the limbo), the task pool's copy.
    pub(crate) spent: Vec<PageId>,
    pub(crate) dying: Vec<PageId>,
    pub(crate) pend: Vec<PageId>,
    pub(crate) drafts: Vec<PageId>,
    pub(crate) bhead: Vec<PageId>,
    pub(crate) btail: ChainEntry,
    /// The copied pool's seal-time bookmark (the rollback-side anchor
    /// of the round-eaten remainder move — see [`Flight::eat_remainder`]).
    pub(crate) bhome: ChainEntry,
    pub(crate) bnext: PageId,
    /// The delta snapshots from the start, the table roots beside them.
    pub(crate) snaps: Vec<Delta>,
    pub(crate) roots: Vec<PageId>,
    /// The rebuilt tables' old pages — limbo here, deaths at the commit.
    pub(crate) old_tree: Vec<PageId>,
    pub(crate) old_gen: u64,
}

impl Flight {
    /// Splice in a tail chunk: the cells into the head remainder, the
    /// emptied chunk page into the drains, the position onward.
    pub(crate) fn eat(&mut self, cells: Vec<PageId>, nx: PageId, chunk_p: PageId) {
        self.head.extend(cells);
        self.drains.push(chunk_p);
        self.tail = ChainEntry::full(nx);
    }

    /// Swallow a recovered off≠0 remainder (the one place an offset
    /// can appear besides the SB — an untouched head): a continuation
    /// behind a page pointer must be a whole page, and eating is
    /// page-granular. The live cells re-list (a cell the round already
    /// lists — a death, a drain, a spent container, a head — stays
    /// once), the container retires, the tail moves to the next whole
    /// chunk. `taken` — the flush pool's own bag (the cells join the
    /// stock head, the container the drains); else the copied pool's
    /// (both re-list as its front cells — the new chain bypasses the
    /// container, so it retires now; the runtime bookmark follows at
    /// the commit via `Frees::retask`).
    pub(crate) fn eat_remainder(&mut self, taken: bool, cells: Vec<PageId>, nx: PageId) {
        let p = if taken { 1 - self.ep } else { self.ep };
        let listed: HashSet<PageId> = self.cells(&[], p).1.into_iter().collect();
        let page = if taken {
            self.tail.page
        } else {
            self.btail.page
        };
        let (head, tail) = if taken {
            (&mut self.head, &mut self.tail)
        } else {
            (&mut self.bhead, &mut self.btail)
        };
        head.extend(cells.into_iter().filter(|c| !listed.contains(c)));
        if !listed.contains(&page) {
            if taken {
                self.drains.push(page);
            } else {
                head.push(page);
            }
        }
        *tail = ChainEntry::full(nx);
    }

    /// The round's cells for pool `p`, split in two: the limbo —
    /// pendings, drafts and the retired trees, re-copied every round
    /// while their pages stay un-free (guarded, in the arena, or
    /// undrained deaths) — and the free — everything the round frees
    /// for good. The bag: the stock head and the husk (the taken
    /// pool), the head copy (the other one).
    pub(crate) fn cells(&self, live: &[PageId], p: usize) -> (Vec<PageId>, Vec<PageId>) {
        let par = |x: &PageId| (x % 2) as usize == p;
        let mut free: Vec<PageId> = self
            .dying
            .iter()
            .chain(self.drains.iter())
            .chain(self.spent.iter())
            .filter(|x| par(x))
            .copied()
            .collect();
        if p != self.ep {
            free.extend_from_slice(&self.head);
            free.extend_from_slice(live);
        } else {
            free.extend_from_slice(&self.bhead);
        }
        let limbo: Vec<PageId> = self
            .pend
            .iter()
            .chain(self.drafts.iter())
            .chain(self.old_tree.iter())
            .filter(|x| par(x))
            .copied()
            .collect();
        (limbo, free)
    }
}

// ## The stock fixpoint

/// Take `n` pages from the flight: the head's remainder → the tail's
/// chunks → growing the round's own counter. The flight is the
/// accumulator (the round's locals).
pub(crate) async fn take_flight<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    fl: &mut Flight,
    fuel0: u64,
    mut n: usize,
) -> Result<Vec<PageId>, StoreError> {
    let mut out = Vec::new();
    let mut fuel = fuel0;
    loop {
        if n == 0 {
            return Ok(out);
        }
        if fuel == 0 {
            return Err(StoreError::Steps("takeFlight: steps"));
        }
        fuel -= 1;
        if let Some(p) = fl.head.pop() {
            out.push(p);
            n -= 1;
        } else if fl.tail.page == 0 {
            let p = fl.next;
            fl.next += 2;
            fl.head.push(p);
        } else {
            let tp = fl.tail.page;
            let off = fl.tail.off;
            let (cells, nx) = crate::persist::read::<D, _>(inner, tp, move |b| {
                (chunk_cells(b, off as usize), chunk_next(b))
            })
            .await?;
            fl.eat(cells, nx, tp);
        }
    }
}

/// The stock fixpoint: snapshot → cells → need → take what is
/// missing (eating the tail grows the drains — the cells grow —
/// resnapshot). The snapshot's one live read is the husk — the
/// mid-round retires into the taken pool, whose listings retire
/// with this round's fronts.
pub(crate) async fn flip_loop<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    fl: &mut Flight,
    need_pages: usize,
) -> Result<FlipData, StoreError> {
    let mut stock: Vec<PageId> = Vec::new();
    let mut fuel = fl.next + 4;
    // The taken pool's runs are separate (its bookmark must land at
    // a chunk boundary); the copied pool's make do with one.
    let chunks = |l: usize, f: usize, split: bool| {
        if split {
            l.div_ceil(CHUNK_SLOTS) + f.div_ceil(CHUNK_SLOTS)
        } else {
            (l + f).div_ceil(CHUNK_SLOTS)
        }
    };
    loop {
        if fuel == 0 {
            return Err(StoreError::Steps("flip: steps"));
        }
        fuel -= 1;
        let husk = {
            let in_ = inner.borrow();
            in_.core.husk()
        };
        let (l0, f0) = fl.cells(&husk, 0);
        let (l1, f1) = fl.cells(&husk, 1);
        let need = need_pages
            + chunks(l0.len(), f0.len(), fl.ep != 0)
            + chunks(l1.len(), f1.len(), fl.ep != 1);
        if stock.len() >= need {
            let (ep, old_gen) = (fl.ep, fl.old_gen);
            return Ok(FlipData {
                stock,
                limbo: [l0, l1],
                free: [f0, f1],
                ep,
                old_gen,
            });
        }
        let want = need - stock.len();
        let more = take_flight::<D>(inner, fl, 2 * want as u64 + 2, want).await?;
        stock.extend(more);
    }
}

/// A flip snapshot: everything going into the new image — the stock
/// and, per pool, the limbo and the free cells.
pub(crate) struct FlipData {
    /// The tree pages + the chunk pages.
    pub(crate) stock: Vec<PageId>,
    pub(crate) limbo: [Vec<PageId>; 2],
    pub(crate) free: [Vec<PageId>; 2],
    pub(crate) ep: usize,
    pub(crate) old_gen: u64,
}
