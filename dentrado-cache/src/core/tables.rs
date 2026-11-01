//! The tables — the store's fixed set of key-addressed tables, each a
//! B-tree on disk plus the delta before it. The delta is the latest
//! writes not yet moved into the tree: it is searched first, and the
//! flush commit purges what the round sealed. The table count is
//! fixed at `open` — the caller's global enum; the trees' page codec
//! and geometry live in [`crate::core::tree`].

use super::CacheValue;
use crate::page::PageId;

/// The delta map — a table's not-yet-flushed half.
pub(crate) type Delta = imbl::HashMap<Vec<u8>, CacheValue>;

/// The tables: each table's B-tree root plus its own delta.
pub(crate) struct Tables {
    /// Each table's tree root; 0 — empty.
    pub(crate) roots: Vec<PageId>,
    deltas: Vec<Delta>,
}

impl Tables {
    pub(crate) fn fresh(n: usize) -> Self {
        Self {
            roots: vec![0; n],
            deltas: (0..n).map(|_| Delta::new()).collect(),
        }
    }

    pub(crate) fn of_image(roots: &[PageId]) -> Self {
        Self {
            roots: roots.to_vec(),
            ..Self::fresh(roots.len())
        }
    }

    pub(crate) fn get(&self, t: usize, k: &[u8]) -> Option<CacheValue> {
        self.deltas[t].get(k).cloned()
    }

    pub(crate) fn put(&mut self, t: usize, k: &[u8], v: CacheValue) {
        self.deltas[t].insert(k.to_vec(), v);
    }

    /// The delta snapshots for a flush round's seal.
    pub(crate) fn snap(&self) -> Vec<Delta> {
        self.deltas.iter().cloned().collect()
    }

    /// The commit: purge what the round sealed — a key the delta still
    /// agrees with; later writes keep their entries.
    pub(crate) fn commit(&mut self, snaps: &[Delta]) {
        for (d, snap) in self.deltas.iter_mut().zip(snaps) {
            for (k, v) in snap.iter() {
                if d.get(k) == Some(v) {
                    d.remove(k);
                }
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.deltas.iter().map(|d| d.len()).sum()
    }

    /// The deltas' value pages — the uncommitted half of the tables.
    pub(crate) fn pages(&self) -> impl Iterator<Item = PageId> + '_ {
        self.deltas
            .iter()
            .flat_map(|d| d.values().flat_map(|v| v.pages.iter().copied()))
    }
}
