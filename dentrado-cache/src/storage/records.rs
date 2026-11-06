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

use super::{Guard, Storage};
use crate::blockcache;
use crate::core::StepHit;
use crate::core::tree::{leaf_find, parse_node, route};
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
    /// Read one record's value whole: the delta first, the standing
    /// tree second. The bytes are the record's inline half — pages a
    /// record names are read through [`Storage::read_page`] by the
    /// owner's own codec. The EBR pin lives inside the walk; the
    /// answer is owned bytes, no page references out.
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

    /// Scan a table: the records under `prefix`, strictly past `after`,
    /// in key order — the standing tree pruned to the range, the delta
    /// overlaid on it (a delta put replaces, a delta deletion hides).
    /// The EBR pin lives inside the walk; the answer is owned bytes.
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
        let mut merged: BTreeMap<Vec<u8>, Option<Vec<u8>>> = BTreeMap::new();
        for (k, mt) in walk_range::<D>(&self.inner, root, lo, end.as_deref(), bound).await? {
            merged.insert(k, Some(mt));
        }
        for (k, v) in overlay {
            merged.insert(
                k,
                match v {
                    crate::core::DeltaV::Put(mt) => Some(mt),
                    crate::core::DeltaV::Del => None,
                },
            );
        }
        let mut out = Vec::new();
        for (k, mt) in merged {
            if k.as_slice() <= after {
                continue;
            }
            if let Some(mt) = mt {
                out.push((k, mt));
            }
        }
        drop(guard);
        Ok(out)
    }
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
