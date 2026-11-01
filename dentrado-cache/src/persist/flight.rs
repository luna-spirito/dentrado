//! A flush round's flight — the future's locals: everything the round
//! carries across its await points. Nobody but the executor reads it,
//! a crash swallows it without a trace, and its only touches of
//! foreign state (growing the world, the husk snapshot, the commit)
//! remain atomic sections. The round's body ([`super::flush`]) drives
//! it; the stock fixpoint lives there too.

use crate::core::Delta;
use crate::core::freelist::ChainEntry;
use crate::page::PageId;

/// A flush round's flight.
pub(super) struct Flight {
    pub(super) ep: usize,
    /// A, taken whole — eaten and grown through the round: the stock
    /// head, the tail-eating position, the counter, the drains.
    pub(super) head: Vec<PageId>,
    pub(super) tail: ChainEntry,
    pub(super) next: PageId,
    pub(super) drains: Vec<PageId>,
    /// The sealed world: the retiring containers, the deaths pile, the
    /// pendings and drafts copies (the limbo), the task pool's copy.
    pub(super) spent: Vec<PageId>,
    pub(super) dying: Vec<PageId>,
    pub(super) pend: Vec<PageId>,
    pub(super) drafts: Vec<PageId>,
    pub(super) bhead: Vec<PageId>,
    pub(super) btail: ChainEntry,
    pub(super) bnext: PageId,
    /// The delta snapshots from the start, the table roots beside them.
    pub(super) snaps: Vec<Delta>,
    pub(super) roots: Vec<PageId>,
    /// The rebuilt tables' old pages — limbo here, deaths at the commit.
    pub(super) old_tree: Vec<PageId>,
    pub(super) old_gen: u64,
}

impl Flight {
    /// Splice in a tail chunk: the cells into the head remainder, the
    /// emptied chunk page into the drains, the position onward.
    pub(super) fn eat(&mut self, cells: Vec<PageId>, nx: PageId, chunk_p: PageId) {
        self.head.extend(cells);
        self.drains.push(chunk_p);
        self.tail = ChainEntry::full(nx);
    }

    /// The round's cells for pool `p`, split in two: the limbo —
    /// pendings, drafts and the retired trees, re-copied every round
    /// while their pages stay un-free (guarded, in the arena, or
    /// undrained deaths) — and the free — everything the round frees
    /// for good. The bag: the stock head and the husk (the taken
    /// pool), the head copy (the other one).
    pub(super) fn cells(&self, live: &[PageId], p: usize) -> (Vec<PageId>, Vec<PageId>) {
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

/// A flip snapshot: everything going into the new image — the stock
/// and, per pool, the limbo and the free cells.
pub(super) struct FlipData {
    /// The tree pages + the chunk pages.
    pub(super) stock: Vec<PageId>,
    pub(super) limbo: [Vec<PageId>; 2],
    pub(super) free: [Vec<PageId>; 2],
    pub(super) ep: usize,
    pub(super) old_gen: u64,
}
