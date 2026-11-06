//! The flush round's sections: the seal's take (everything a round
//! starts from, out in one section), the failed round's rollback, the
//! commit that lands the world back. The round agent's choreography —
//! [`crate::persist::flush`] — drives these; the sections stay pure
//! transitions.

use super::Core;
use super::tables::Delta;
use crate::core::freelist::ChainEntry;
use crate::disk::IsFilesystem;
use crate::page::PageId;

/// The flush seal's take: everything a round starts from, out in one
/// section — the flush pool taken whole (head, tail entry, counter)
/// with the retiring containers (the task-eaten ones and both pools'
/// fronts, folded into `spent` by `Frees::seal`), the task pool
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

impl<D: IsFilesystem> Core<D> {
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

    /// A failed round's rollback: the seal-time world comes back — the
    /// taken pool reinstated (the husk's mid-round retires kept), the
    /// retiring containers and the deaths pile back on top of what the
    /// round's lifetime accumulated. The epoch never flipped, so the
    /// round never was. (The flight's seal-time copies; `head`, `tail`,
    /// `next` — the taken pool, `spent` — the seal's merged bag,
    /// `dying` — the sealed pile.)
    pub(crate) fn unseal(
        &mut self,
        head: Vec<PageId>,
        tail: ChainEntry,
        next: PageId,
        spent: Vec<PageId>,
        dying: Vec<PageId>,
    ) {
        self.frees.unseal(head, tail, next, spent);
        self.deaths.give_back(dying);
    }

    /// The task pool's bookmark past a round-eaten remainder (a
    /// recovered off≠0 entry the round swallowed whole) — see
    /// `Frees::retask`. Called in the round's commit section, before
    /// the commit flips the epoch.
    pub(crate) fn retask(&mut self, old: ChainEntry, new: ChainEntry) {
        self.frees.retask(old, new)
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
}
