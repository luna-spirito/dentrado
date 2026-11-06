//! The draft arena — the pages between allocation and publication. A
//! page enters as a fresh draft (never yet written, its slot still
//! holding pool noise) and leaves by publication (the `put_many` seal)
//! or death ([`Core::dealloc_step`]).

use std::collections::HashMap;

use crate::page::PageId;

// TODO: I assume we don't really need this structure. This indirection could probably just killed.
// Use `DraftId(PageId)`, or maybe even do something like `DraftId { p: PageId, fresh: bool }`.

/// A draft's key in the store's arena.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct DraftId(u64);

impl DraftId {
    /// A key from thin air — the misuse tests' phantom.
    #[cfg(test)]
    pub(crate) fn phantom(u: u64) -> Self {
        Self(u)
    }
}

/// The draft arena: keys → (page, fresh — never yet written).
pub(crate) struct Drafts {
    next: u64,
    map: HashMap<DraftId, (PageId, bool)>,
}

impl Drafts {
    pub(crate) fn fresh() -> Self {
        Self {
            next: 0,
            map: HashMap::new(),
        }
    }

    /// A page enters the arena as a fresh draft — the key out.
    pub(crate) fn take(&mut self, p: PageId) -> DraftId {
        let k = DraftId(self.next);
        self.next += 1;
        self.map.insert(k, (p, true));
        k
    }

    pub(crate) fn get(&self, k: &DraftId) -> Option<(PageId, bool)> {
        self.map.get(k).copied()
    }

    /// The first write went through — no longer fresh (an overwrite of
    /// the slot's garbage).
    pub(crate) fn defresh(&mut self, k: &DraftId) {
        if let Some(e) = self.map.get_mut(k) {
            e.1 = false;
        }
    }

    /// Does the arena still hold this page?
    pub(crate) fn has_page(&self, p: PageId) -> bool {
        self.map.values().any(|&(q, _)| q == p)
    }

    /// The consumed drafts' pages — `None` if any has vanished.
    pub(crate) fn pages_of(&self, cs: &[DraftId]) -> Option<Vec<PageId>> {
        cs.iter()
            .map(|c| self.map.get(c).map(|&(p, _)| p))
            .collect()
    }

    pub(crate) fn remove(&mut self, k: &DraftId) -> Option<(PageId, bool)> {
        self.map.remove(k)
    }

    /// The live drafts' pages (the invariant's inspection).
    pub(crate) fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.map.values().map(|&(p, _)| p)
    }
}
