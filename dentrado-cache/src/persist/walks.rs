//! The image's walks — every multi-page traversal the store performs
//! outside a live tree descent: whole trees (the drops, the
//! invariants), ranged trees (the scans), value chains, and the
//! schema-aware dependency closures. All reads go through the pool —
//! pages live only there.

use std::cell::RefCell;
use std::collections::{HashSet, VecDeque};
use std::rc::Rc;

use super::PersistentCore;
use crate::blockcache;
use crate::core::freelist::{ChainEntry, chunk_cells, chunk_next};
use crate::core::tree::{Item, parse_node};
use crate::core::val::val_split;
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;
use crate::schema::{PageSchema, TableSchema};

/// Walk a table's tree breadth-first: its records in key order (the
/// leaves come out left to right) and all its pages.
pub(crate) async fn walk_tree<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    fuel0: u64,
) -> Result<(Vec<Item>, Vec<PageId>), StoreError> {
    if root == 0 {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut es = Vec::new();
    let mut pages = Vec::new();
    let mut q: VecDeque<PageId> = VecDeque::from([root]);
    let mut fuel = fuel0;
    while let Some(p) = q.pop_front() {
        if fuel == 0 {
            return Err(StoreError::Steps("tree: steps"));
        }
        fuel -= 1;
        let n = blockcache::read::<D, _>(inner, p, parse_node).await?;
        if n.level == 0 {
            es.extend(n.es.into_iter().zip(n.ids).map(|((k, mt), vr)| (k, mt, vr)));
        } else {
            q.extend(n.ids);
        }
        pages.push(p);
    }
    Ok((es, pages))
}

/// Walk a table's tree in key order, collecting the records in
/// `[lo, hi)` (`hi` — `None` — open-ended). The descent is pruned: a
/// child whose key range cannot overlap the ask is never read — a scan
/// touches only the leaves it answers for.
pub(crate) async fn walk_range<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    lo: &[u8],
    hi: Option<&[u8]>,
    fuel0: u64,
) -> Result<Vec<Item>, StoreError> {
    if root == 0 {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let mut fuel = fuel0;
    // In-order DFS: the frame (page, next child); a node re-enters per
    // child — a cache hit and a re-parse, bounded by height × fanout.
    let mut stack: Vec<(PageId, usize)> = vec![(root, 0)];
    while let Some((p, at)) = stack.pop() {
        if fuel == 0 {
            return Err(StoreError::Steps("tree: steps"));
        }
        fuel -= 1;
        let node = blockcache::read::<D, _>(inner, p, parse_node).await?;
        if node.level == 0 {
            for ((k, mt), &vr) in node.es.iter().zip(&node.ids) {
                if k.as_slice() >= lo && hi.is_none_or(|h| k.as_slice() < h) {
                    out.push((k.clone(), mt.clone(), vr));
                }
            }
            continue;
        }
        if let Some(&child) = node.ids.get(at) {
            // Child `at` spans [es[at], es[at+1]) — child 0 below the
            // smallest key, the last one open-ended. Descend only where
            // the span meets the ask.
            let clo = node.es.get(at).map(|(k, _)| k.as_slice());
            let chi = node.es.get(at + 1).map(|(k, _)| k.as_slice());
            let meets = hi.is_none_or(|h| clo.is_none_or(|c| c < h)) && chi.is_none_or(|c| c > lo);
            stack.push((p, at + 1));
            if meets {
                stack.push((child, 0));
            }
        }
    }
    Ok(out)
}

/// Walk a tail chain, collecting each chunk page with its live cells —
/// the chain's occupancy in walk order, from the entry down.
pub(crate) async fn walk_parts<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    e0: ChainEntry,
    fuel0: u64,
) -> Result<Vec<(PageId, Vec<PageId>)>, StoreError> {
    let mut out = Vec::new();
    let mut e = e0;
    let mut fuel = fuel0;
    loop {
        if e.page == 0 {
            return Ok(out);
        }
        if fuel == 0 {
            return Err(StoreError::Steps("walk: steps"));
        }
        fuel -= 1;
        let (cs, nx) = blockcache::read::<D, _>(inner, e.page, move |b| {
            (chunk_cells(b, e.off as usize), chunk_next(b))
        })
        .await?;
        out.push((e.page, cs));
        e = ChainEntry::full(nx);
    }
}

/// Walk a tail chain: the cells in issue order.
pub(crate) async fn walk_chain_raw<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    e0: ChainEntry,
    fuel0: u64,
) -> Result<Vec<PageId>, StoreError> {
    Ok(walk_parts::<D>(inner, e0, fuel0)
        .await?
        .into_iter()
        .flat_map(|(_, cs)| cs)
        .collect())
}

/// Walk a value's chain (pages by their `next` headers). The bound is
/// the world's pages: a longer value chain can only be corruption.
pub(crate) async fn walk_value<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    first: PageId,
    bound: u64,
) -> Result<Vec<PageId>, StoreError> {
    let mut out = Vec::new();
    let mut p = first;
    let mut fuel = bound;
    loop {
        if fuel == 0 {
            return Err(StoreError::Steps("value: steps"));
        }
        fuel -= 1;
        out.push(p);
        let next = blockcache::read::<D, _>(inner, p, |b| val_split(b).0).await?;
        if next == 0 {
            return Ok(out);
        }
        p = next;
    }
}

/// A record's full page closure beyond its own bytes: its value chain
/// (when `vr` names one) plus, recursively, every page the schema
/// names from the record's inline bytes and each dependent page's
/// payload. Shared pages are walked once; the fuel bounds the walk —
/// a schema naming more pages than the world holds is corruption, as
/// is a schema that cannot parse what it is handed.
pub(crate) async fn walk_deps<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    schema: &'static dyn TableSchema,
    mt: &[u8],
    vr: PageId,
    fuel0: u64,
) -> Result<Vec<PageId>, StoreError> {
    let mut out = if vr != 0 {
        walk_value::<D>(inner, vr, fuel0).await?
    } else {
        Vec::new()
    };
    let mut seen: HashSet<PageId> = out.iter().copied().collect();
    let mut q: VecDeque<(PageId, &'static dyn PageSchema)> = schema.val_deps(mt)?.into();
    let mut fuel = fuel0;
    while let Some((p, ps)) = q.pop_front() {
        if !seen.insert(p) {
            continue; // a shared page — already in the closure
        }
        if fuel == 0 {
            return Err(StoreError::CorruptValue(
                "a dependency walk ran out of fuel",
            ));
        }
        fuel -= 1;
        out.push(p);
        let deps = blockcache::read::<D, _>(inner, p, move |b| ps.deps(val_split(b).1)).await??;
        q.extend(deps);
    }
    Ok(out)
}
