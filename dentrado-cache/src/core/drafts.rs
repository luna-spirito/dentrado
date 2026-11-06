//! The draft arena's store-side half: the set of pages between
//! allocation and publication. A page enters at allocation (the
//! session's own ledger tracks its identity and freshness) and leaves
//! by publication (a commit's seal) or death (a discard, a dropped
//! session). The set is the store's whole interest: what it must not
//! hand out again, must pack as limbo at a flush, and must refuse on
//! a caller's `free` list.

use std::collections::HashSet;

use crate::page::PageId;

/// The held drafts — a set, not a ledger: which pages are spoken for.
pub(crate) struct Drafts {
    held: HashSet<PageId>,
}

impl Drafts {
    pub(crate) fn fresh() -> Self {
        Self {
            held: HashSet::new(),
        }
    }

    /// A page enters the arena.
    pub(crate) fn take(&mut self, p: PageId) {
        self.held.insert(p);
    }

    /// Does the arena still hold this page?
    pub(crate) fn has(&self, p: PageId) -> bool {
        self.held.contains(&p)
    }

    /// The page leaves — published or dead. Whether it was there.
    pub(crate) fn remove(&mut self, p: PageId) -> bool {
        self.held.remove(&p)
    }

    /// The live drafts' pages (the invariant's inspection, the flush's
    /// limbo).
    pub(crate) fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.held.iter().copied()
    }
}
