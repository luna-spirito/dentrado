//! The tables — the store's named set of key-addressed tables, each a
//! B-tree on disk plus the delta before it. The delta is the latest
//! writes not yet moved into the tree: it is searched first, and the
//! flush commit purges what the round sealed. The set is identified by
//! names (see [`crate::core::sb`]); the trees' page codec and geometry
//! live in [`crate::core::tree`].

use std::collections::HashMap;

use super::CacheValue;
use crate::page::PageId;

/// The delta map — a table's not-yet-flushed half.
pub(crate) type Delta = imbl::HashMap<Vec<u8>, CacheValue>;

/// The tables: each slot's name, B-tree root and own delta. A dropped
/// slot is a tombstone — an empty name — not a removal: slots never
/// shift while the store lives (a flush round's sealed snapshots index
/// by slot), and the tombstone leaves the SB and every lookup, dying
/// whole at the next process (which opens by name, not position).
pub(crate) struct Tables {
    /// Each slot's name; empty — a dropped slot's tombstone.
    names: Vec<String>,
    /// Live names → slot (a tombstone leaves it).
    index: HashMap<String, usize>,
    /// Each table's tree root; 0 — empty.
    pub(crate) roots: Vec<PageId>,
    deltas: Vec<Delta>,
}

impl Tables {
    pub(crate) fn fresh(names: &[&str]) -> Self {
        let names: Vec<String> = names.iter().map(|n| n.to_string()).collect();
        Self {
            index: names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), i))
                .collect(),
            roots: vec![0; names.len()],
            deltas: (0..names.len()).map(|_| Delta::new()).collect(),
            names,
        }
    }

    /// The union: the opener's names first (in opener order), then the
    /// image's extras (in image order) — the opener's new tables start
    /// empty, the image's orphans stay enumerable and droppable.
    pub(crate) fn of_image(image: &[(String, PageId)], opener: &[&str]) -> Self {
        let mut names = opener.to_vec();
        for (n, _) in image {
            if !names.iter().any(|o| o == n) {
                names.push(n.as_str());
            }
        }
        let mut t = Self::fresh(&names);
        for (n, r) in image {
            if let Some(&i) = t.index.get(n) {
                t.roots[i] = *r;
            }
        }
        t
    }

    /// A live name's slot.
    pub(crate) fn idx(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
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

    /// The live names (slot order, tombstones out).
    pub(crate) fn names(&self) -> Vec<String> {
        self.names
            .iter()
            .filter(|n| !n.is_empty())
            .cloned()
            .collect()
    }

    /// The live `(name, root)` pairs over a roots vector (the flush's
    /// new roots) — the SB's table set.
    pub(crate) fn live_with(&self, roots: &[PageId]) -> Vec<(String, PageId)> {
        self.names
            .iter()
            .zip(roots)
            .filter(|(n, _)| !n.is_empty())
            .map(|(n, &r)| (n.clone(), r))
            .collect()
    }

    /// A slot's delta value pages — the uncommitted half of one table
    /// (a drop must free them with the tree).
    pub(crate) fn delta_pages(&self, t: usize) -> Vec<PageId> {
        self.deltas[t]
            .values()
            .flat_map(|v| v.pages.iter().copied())
            .collect()
    }

    /// A slot's death: the name and delta go, the root zeroes — the
    /// pages themselves die with the caller's EBR registration.
    pub(crate) fn drop_slot(&mut self, t: usize) {
        let name = self.names[t].clone();
        if name.is_empty() {
            return; // already a tombstone
        }
        self.names[t].clear();
        self.index.remove(&name);
        self.roots[t] = 0;
        self.deltas[t] = Delta::new();
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
