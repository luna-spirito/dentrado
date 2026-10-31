//! The incremental half of a tree flush — grafting a round's delta
//! onto the standing tree copy-on-write. Only the dirty root-to-leaf
//! paths take new pages; the clean subtrees keep theirs (their pages
//! live on in both images until the path above them changes). The
//! retired pages ride the round like any death: limbo first, EBR at
//! the commit. Never a merge — the store only ever adds keys.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::PersistentCore;
use crate::blockcache;
use crate::core::tree::{self, Item};
use crate::disk::Storage;
use crate::error::StoreError;
use crate::page::PageId;

/// A visited node on a dirty path: its page (a death this round), its
/// parse, and its share of the round's sorted batch.
struct Frame {
    p: PageId,
    node: tree::Node,
    batch: Vec<Item>,
}

/// The report up from one rewritten node: the draft replacing the old
/// page, the subtree's lo key (the parent's route), and the right
/// siblings a repack was born.
struct Up {
    id: PageId,
    lo: Vec<u8>,
    cuts: Vec<(PageId, Vec<u8>)>,
}

/// Graft `batch` (any order — the graft sorts) onto the tree at `root`
/// (never 0 — an empty table is built whole): the new drafts —
/// children before parents, the new root last, `tree::build`'s order —
/// and the old pages the dirty paths retired. `fuel0` bounds the
/// reads: more nodes than the file holds can only be corruption.
pub(super) async fn run<D: Storage>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    mut batch: Vec<Item>,
    fuel0: u64,
) -> Result<(Vec<tree::Node>, Vec<PageId>), StoreError> {
    batch.sort_by(|a, b| a.0.cmp(&b.0));
    let mut fuel = fuel0;
    // Top-down: each frontier interior splits its share among the
    // children — the sorted keys give each child one run — and the
    // children form the next frontier. The frames stay: the rewrite
    // walks them back.
    let mut layers: Vec<Vec<Frame>> = Vec::new();
    let mut frontier = vec![Frame {
        p: root,
        node: read(inner, root, &mut fuel).await?,
        batch,
    }];
    while !frontier.is_empty() {
        let mut below: Vec<Frame> = Vec::new();
        for f in &mut frontier {
            if f.node.level == 0 {
                continue;
            }
            let mut at = 0;
            while at < f.batch.len() {
                let i = tree::route_at(&f.node, &f.batch[at].0).unwrap_or(0);
                let mut end = at + 1;
                while end < f.batch.len()
                    && tree::route_at(&f.node, &f.batch[end].0).unwrap_or(0) == i
                {
                    end += 1;
                }
                let cp = f.node.ids[i];
                below.push(Frame {
                    p: cp,
                    node: read(inner, cp, &mut fuel).await?,
                    batch: f.batch[at..end].to_vec(),
                });
                at = end;
            }
        }
        layers.push(std::mem::take(&mut frontier));
        frontier = below;
    }
    // Bottom-up: each frame's rewrite over its children's reports. A
    // split at the top crowns a new root — and a crown that itself
    // overflows repacks again (one round can dirty the whole tree).
    let mut ups: HashMap<PageId, Up> = HashMap::new();
    let mut drafts: Vec<tree::Node> = Vec::new();
    for layer in layers.iter().rev() {
        for f in layer {
            let up = rewrite(f, &ups, &mut drafts)?;
            ups.insert(f.p, up);
        }
    }
    let mut up = ups.remove(&root).expect("the root is a frame");
    let mut level = layers[0][0].node.level;
    while !up.cuts.is_empty() {
        level += 1;
        if level > 64 {
            return Err(StoreError::Steps("tree: crown"));
        }
        let mut ts = vec![(up.lo.clone(), Vec::new(), up.id)];
        ts.extend(up.cuts.drain(..).map(|(id, lo)| (lo, Vec::new(), id)));
        up = repack(level, ts, tree::NODE_MAX, tree::route_e, &mut drafts)?;
    }
    let replaced: Vec<PageId> = layers.iter().flat_map(|l| l.iter().map(|f| f.p)).collect();
    Ok((drafts, replaced))
}

/// One frame's rewrite: a leaf merges its share in, an interior
/// re-points its dirty children; either way an overflowing node repacks
/// into as many siblings as fit — the same greedy packing as the
/// wholesale build.
fn rewrite(
    f: &Frame,
    ups: &HashMap<PageId, Up>,
    drafts: &mut Vec<tree::Node>,
) -> Result<Up, StoreError> {
    if f.node.level == 0 {
        repack(
            0,
            merge(&f.node, &f.batch),
            tree::LEAF_MAX,
            tree::leaf_e,
            drafts,
        )
    } else {
        repack(
            f.node.level,
            repoint(&f.node, ups),
            tree::NODE_MAX,
            tree::route_e,
            drafts,
        )
    }
}

/// The packed records as drafts — one node, or a run of siblings born
/// of the repack; the report goes up.
fn repack(
    level: u32,
    ts: Vec<Item>,
    cap: usize,
    cost: impl Fn(&Item) -> usize,
    drafts: &mut Vec<tree::Node>,
) -> Result<Up, StoreError> {
    let mut runs = tree::pack(&ts, cap, cost)?.into_iter();
    let (at, len) = runs.next().expect("a visited node holds a record");
    let mut up = Up {
        id: tree::virt(drafts.len()),
        lo: ts[at].0.clone(),
        cuts: Vec::new(),
    };
    emit(drafts, level, &ts[at..at + len]);
    for (at, len) in runs {
        up.cuts.push((tree::virt(drafts.len()), ts[at].0.clone()));
        emit(drafts, level, &ts[at..at + len]);
    }
    Ok(up)
}

fn emit(drafts: &mut Vec<tree::Node>, level: u32, ts: &[Item]) {
    drafts.push(tree::Node {
        level,
        es: ts
            .iter()
            .map(|(k, mt, _)| (k.clone(), mt.clone()))
            .collect(),
        ids: ts.iter().map(|t| t.2).collect(),
    });
}

/// The leaf's records with its share merged in — both sides sorted, an
/// equal key's old record steps aside (the delta is the newer half).
fn merge(node: &tree::Node, batch: &[Item]) -> Vec<Item> {
    let (es, mut out) = (&node.es, Vec::with_capacity(node.es.len() + batch.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < es.len() || j < batch.len() {
        let old = match (es.get(i), batch.get(j)) {
            (Some((k, _)), Some(b)) => k.as_slice() < b.0.as_slice(),
            (Some(_), None) => true,
            _ => false,
        };
        if old {
            out.push((es[i].0.clone(), es[i].1.clone(), node.ids[i]));
            i += 1;
        } else {
            out.push(batch[j].clone());
            j += 1;
            if let Some((k, _)) = es.get(i) {
                if k.as_slice() == out.last().unwrap().0.as_slice() {
                    i += 1; // the replaced record
                }
            }
        }
    }
    out
}

/// The interior's routes with its dirty children re-pointed: each
/// replaced child's report from below — its draft, its subtree's lo,
/// the right siblings born under it.
fn repoint(node: &tree::Node, ups: &HashMap<PageId, Up>) -> Vec<Item> {
    let mut out = Vec::with_capacity(node.ids.len());
    for ((lo, _), &id) in node.es.iter().zip(&node.ids) {
        if let Some(up) = ups.get(&id) {
            out.push((up.lo.clone(), Vec::new(), up.id));
            for (cid, clo) in &up.cuts {
                out.push((clo.clone(), Vec::new(), *cid));
            }
        } else {
            out.push((lo.clone(), Vec::new(), id));
        }
    }
    out
}

async fn read<D: Storage>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    p: PageId,
    fuel: &mut u64,
) -> Result<tree::Node, StoreError> {
    if *fuel == 0 {
        return Err(StoreError::Steps("tree: steps"));
    }
    *fuel -= 1;
    blockcache::read::<D, _>(inner, p, tree::parse_node).await
}
