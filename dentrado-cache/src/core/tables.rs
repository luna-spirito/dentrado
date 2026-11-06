//! The tables — the store's set of key-addressed tables, each a
//! B-tree on disk plus the delta before it. The delta is the latest
//! writes not yet moved into the tree: it is searched first, and the
//! flush commit purges what the round sealed. The set is identified by
//! opaque id bytes (see [`crate::core::sb`]); the trees' page codec
//! and geometry live in [`crate::core::tree`].

use std::collections::HashMap;

use crate::page::PageId;
use crate::schema::{TableSchema, TableSpec};

/// One delta entry: a record's inline bytes on their way in, a key on
/// its way out. The record's pages the delta does not know — the
/// schema (and the caller's own bookkeeping) says what they are.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum DeltaV {
    Put(Vec<u8>),
    Del,
}

/// The delta map — a table's not-yet-flushed half.
pub(crate) type Delta = imbl::HashMap<Vec<u8>, DeltaV>;

/// The tables: each slot's id, schema, B-tree root and own delta. A
/// dropped slot is a tombstone — an empty id — not a removal: slots
/// never shift while the store lives (a flush round's sealed snapshots
/// index by slot), and the tombstone leaves the SB and every lookup,
/// dying whole at the next process (which opens by id, not position).
pub(crate) struct Tables {
    /// Each slot's id bytes; empty — a dropped slot's tombstone.
    names: Vec<Vec<u8>>,
    /// Live ids → slot (a tombstone leaves it).
    index: HashMap<Vec<u8>, usize>,
    /// Each slot's schema, parallel to the ids. An image orphan the
    /// opener didn't name gets [`crate::schema::NO_DEPS`]: its bytes
    /// cannot be trusted to follow any schema the opener knows, so a
    /// dropped orphan frees only its own pages.
    schemas: Vec<TableSchema>,
    /// Each table's tree root; 0 — empty.
    pub(crate) roots: Vec<PageId>, // TODO: pub(crate) should be avoided on fields.
    deltas: Vec<Delta>,
}

impl Tables {
    pub(crate) fn fresh(specs: &[TableSpec]) -> Self {
        let names: Vec<Vec<u8>> = specs.iter().map(|s| s.id.clone()).collect();
        Self {
            index: names
                .iter()
                .enumerate()
                .map(|(i, n)| (n.clone(), i))
                .collect(),
            schemas: specs.iter().map(|s| s.schema).collect(),
            roots: vec![0; names.len()],
            deltas: (0..names.len()).map(|_| Delta::new()).collect(),
            names,
        }
    }

    /// The union: the opener's tables first (in opener order), then the
    /// image's extras (in image order) — the opener's new tables start
    /// empty, the image's orphans stay enumerable and droppable.
    pub(crate) fn of_image(image: &[(Vec<u8>, PageId)], opener: &[TableSpec]) -> Self {
        let mut specs = opener.to_vec();
        for (id, _) in image {
            if !specs.iter().any(|s| &s.id == id) {
                specs.push(TableSpec::flat(id.clone()));
            }
        }
        let mut t = Self::fresh(&specs);
        for (id, r) in image {
            if let Some(&i) = t.index.get(id) {
                t.roots[i] = *r;
            }
        }
        t
    }

    /// A live id's slot.
    pub(crate) fn idx(&self, id: &[u8]) -> Option<usize> {
        self.index.get(id).copied()
    }

    /// A slot's id (empty — a dropped slot's tombstone).
    pub(crate) fn name(&self, t: usize) -> &[u8] {
        &self.names[t]
    }

    pub(crate) fn schema(&self, t: usize) -> TableSchema {
        self.schemas[t]
    }

    pub(crate) fn get(&self, t: usize, k: &[u8]) -> Option<DeltaV> {
        self.deltas[t].get(k).cloned()
    }

    pub(crate) fn put(&mut self, t: usize, k: &[u8], mt: Vec<u8>) {
        self.deltas[t].insert(k.to_vec(), DeltaV::Put(mt));
    }

    pub(crate) fn del(&mut self, t: usize, k: &[u8]) {
        self.deltas[t].insert(k.to_vec(), DeltaV::Del);
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

    /// The live ids (slot order, tombstones out).
    pub(crate) fn names(&self) -> Vec<Vec<u8>> {
        self.names
            .iter()
            .filter(|n| !n.is_empty())
            .cloned()
            .collect()
    }

    /// Every slot's id, tombstones included — a snapshot's
    /// slot-parallel copy (the roots and deltas line up with it).
    pub(crate) fn names_all(&self) -> Vec<Vec<u8>> {
        self.names.clone()
    }

    /// The live `(id, root)` pairs over a roots vector (the flush's
    /// new roots) — the SB's table set.
    pub(crate) fn live_with(&self, roots: &[PageId]) -> Vec<(Vec<u8>, PageId)> {
        self.names
            .iter()
            .zip(roots)
            .filter(|(n, _)| !n.is_empty())
            .map(|(n, &r)| (n.clone(), r))
            .collect()
    }

    /// A slot's delta records' inline bytes — a drop (and the
    /// partition's exactness) walks the pages their schemas name.
    pub(crate) fn delta_inline(&self, t: usize) -> Vec<Vec<u8>> {
        self.deltas[t]
            .values()
            .filter_map(|v| match v {
                DeltaV::Put(mt) => Some(mt.clone()),
                DeltaV::Del => None,
            })
            .collect()
    }

    /// A slot's delta entries in a key range — a scan's overlay half.
    pub(crate) fn delta_range(
        &self,
        t: usize,
        lo: &[u8],
        hi: Option<&[u8]>,
    ) -> Vec<(Vec<u8>, DeltaV)> {
        delta_range(&self.deltas[t], lo, hi)
    }

    /// A slot's death: the id and delta go, the root zeroes — the
    /// pages themselves die with the caller's EBR registration.
    pub(crate) fn drop_slot(&mut self, t: usize) {
        if self.names[t].is_empty() {
            return; // already a tombstone
        }
        let name = std::mem::take(&mut self.names[t]);
        self.index.remove(&name);
        self.roots[t] = 0;
        self.deltas[t] = Delta::new();
    }

    pub(crate) fn len(&self) -> usize {
        self.deltas.iter().map(|d| d.len()).sum()
    }
}

/// A delta's entries in a key range — the scan overlay's filter. The
/// range is half-open: `k >= lo`, `k < hi` when `hi` is named.
pub(crate) fn delta_range(d: &Delta, lo: &[u8], hi: Option<&[u8]>) -> Vec<(Vec<u8>, DeltaV)> {
    d.iter()
        .filter(|(k, _)| k.as_slice() >= lo && hi.is_none_or(|h| k.as_slice() < h))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}
