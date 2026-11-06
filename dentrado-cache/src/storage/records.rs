//! The record surface's read half — the store's tables as
//! key-addressed records: point reads and prefix scans. A record's
//! value is its inline bytes — the caller's, whole: the store neither
//! chunks values into pages nor parses what a record names (its
//! schema says that, and the session's `take`/`free` lists say which
//! drafts join the world and which pages die with the replaced
//! record). Writes live in [`super::session`].

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use super::{Guard, Session, Storage};
use crate::blockcache;
use crate::core::tree::{leaf_find, parse_node, route};
use crate::core::{DeltaV, StepHit};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;
use crate::persist::PersistentCore;
use crate::persist::walks::walk_range;

/// The table's slot and the world's page bound — one section.
fn table_at<D: IsFilesystem>(s: &Storage<D>, id: &[u8]) -> Result<(usize, u64), StoreError> {
    let inner = s.inner.borrow();
    if inner.poisoned {
        return Err(StoreError::Poisoned);
    }
    let Some(t) = inner.core.table_index(id) else {
        return Err(StoreError::NoTable);
    };
    Ok((t, inner.core.bound()))
}

impl<D: IsFilesystem> Storage<D> {
    /// Read one record's value whole from the live world: the delta
    /// first, the standing tree second. The bytes are the record's
    /// inline half — pages a record names are read through
    /// [`Session::read`] by the owner's own codec. The EBR pin lives
    /// inside the walk; the answer is owned bytes, no page references
    /// out.
    pub async fn get(&self, id: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        let (t, bound) = table_at(self, id)?;
        let (bucket, hit, root) = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            let Some((b, hit, root)) = inner.core.get_step(t, key) else {
                return Err(StoreError::NoTable);
            };
            (b, hit, root)
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket,
        };
        let r = match hit {
            StepHit::Put(mt) => Ok(Some(mt)),
            StepHit::Del => Ok(None),
            StepHit::Tree => match descend::<D>(&self.inner, root, key, bound).await? {
                None => Ok(None),
                Some(mt) => Ok(Some(mt)),
            },
        };
        drop(guard);
        r
    }

    /// Scan a table's live world: the records under `prefix`, strictly
    /// past `after`, in key order — the standing tree pruned to the
    /// range, the delta overlaid on it (a delta put replaces, a delta
    /// deletion hides). The EBR pin lives inside the walk; the answer
    /// is owned bytes.
    pub async fn scan(
        &self,
        id: &[u8],
        prefix: &[u8],
        after: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        let (t, bound) = table_at(self, id)?;
        let end = prefix_end(prefix);
        let lo: &[u8] = if after > prefix { after } else { prefix };
        let (bucket, root, overlay) = {
            let mut inner = self.inner.borrow_mut();
            if inner.poisoned {
                return Err(StoreError::Poisoned);
            }
            (
                inner.core.pin_step(),
                inner.core.table_root(t),
                inner.core.table_delta_range(t, prefix, end.as_deref()),
            )
        };
        let guard = Guard {
            inner: self.inner.clone(),
            bucket,
        };
        let walked = walk_range::<D>(&self.inner, root, lo, end.as_deref(), bound).await?;
        drop(guard);
        Ok(merge_range(walked, overlay, after))
    }
}

impl<D: IsFilesystem> Session<D> {
    /// Read one record from the session's snapshot: the delta first,
    /// the standing tree second. The snapshot's pin — taken at the
    /// first read — keeps the tree's pages alive; the answer is owned
    /// bytes, no page references out.
    pub async fn get(&mut self, id: &[u8], key: &[u8]) -> Result<Option<Vec<u8>>, StoreError> {
        self.ensure_snapshot()?;
        let (root, bound) = {
            let snap = self.snap.as_ref().expect("the snapshot was just ensured");
            let t = snap.table(id).ok_or(StoreError::NoTable)?;
            match snap.delta(t, key) {
                Some(DeltaV::Put(mt)) => return Ok(Some(mt.clone())),
                Some(DeltaV::Del) => return Ok(None),
                None => {}
            }
            (snap.root(t), snap.bound())
        };
        descend::<D>(&self.inner, root, key, bound).await
    }

    /// Scan the session's snapshot: the records under `prefix`, strictly
    /// past `after`, in key order — the snapshot's tree pruned to the
    /// range, its delta overlaid (a put replaces, a deletion hides).
    pub async fn scan(
        &mut self,
        id: &[u8],
        prefix: &[u8],
        after: &[u8],
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>, StoreError> {
        self.ensure_snapshot()?;
        let end = prefix_end(prefix);
        let lo: &[u8] = if after > prefix { after } else { prefix };
        let (root, bound, overlay) = {
            let snap = self.snap.as_ref().expect("the snapshot was just ensured");
            let t = snap.table(id).ok_or(StoreError::NoTable)?;
            (
                snap.root(t),
                snap.bound(),
                snap.delta_range(t, lo, end.as_deref()),
            )
        };
        let walked = walk_range::<D>(&self.inner, root, lo, end.as_deref(), bound).await?;
        Ok(merge_range(walked, overlay, after))
    }
}

/// The scan's merged answer: the tree's walk overlaid with delta entries
/// (a put replaces, a deletion hides), strictly past `after`, in key
/// order.
fn merge_range(
    walked: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>,
    overlay: impl IntoIterator<Item = (Vec<u8>, DeltaV)>,
    after: &[u8],
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
    for (k, mt) in walked {
        merged.insert(k, Some(mt));
    }
    for (k, v) in overlay {
        merged.insert(
            k,
            match v {
                DeltaV::Put(mt) => Some(mt),
                DeltaV::Del => None,
            },
        );
    }
    merged
        .into_iter()
        .filter(|(k, _)| k.as_slice() > after)
        .filter_map(|(k, mt)| mt.map(|mt| (k, mt)))
        .collect()
}

/// The standing tree's record for `key` — a root-to-leaf descent, its
/// inline bytes. The fuel is the world's pages: a deeper tree can only
/// be corruption.
async fn descend<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    key: &[u8],
    bound: u64,
) -> Result<Option<Vec<u8>>, StoreError> {
    if root == 0 {
        return Ok(None);
    }
    let mut p = root;
    let mut fuel = bound;
    let hit = loop {
        if fuel == 0 {
            return Err(StoreError::Steps("tree: steps"));
        }
        fuel -= 1;
        let node = blockcache::read::<D, _>(inner, p, parse_node).await?;
        if node.level == 0 {
            break leaf_find(&node, key).map(|mt| mt.to_vec());
        }
        match route(&node, key) {
            Some(q) => p = q,
            None => break None, // below the tree's smallest key
        }
    };
    Ok(hit)
}

/// The key just past the prefix's span: the prefix with its last
/// non-`0xFF` byte incremented, the `0xFF` tail dropped (`None` — an
/// all-`0xFF` prefix runs to the end of the key space).
fn prefix_end(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut e = prefix.to_vec();
    while matches!(e.last(), Some(&0xFF)) {
        e.pop();
    }
    match e.last_mut() {
        Some(b) => {
            *b += 1;
            Some(e)
        }
        None => None,
    }
}
