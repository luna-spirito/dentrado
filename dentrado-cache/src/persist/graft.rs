//! The incremental half of a tree flush — grafting a round's delta
//! onto the standing tree copy-on-write. Only the dirty root-to-leaf
//! paths take new pages; the clean subtrees keep theirs (their pages
//! live on in both images until the path above them changes), and a
//! rewrite that changes nothing — a no-op delete, a re-put of the very
//! same record — keeps the old page too, the skip cascading up through
//! the reports. The retired pages ride the round like any death: limbo
//! first, EBR at the commit. Deletion is real: a `Del` removes its
//! record, an emptied leaf drops its route, an emptied root empties
//! the table. Never a sibling merge — an underfull node is correct
//! (the repack of a dirty node is already greedy); a wholesale
//! repacking of a hollowed tree is a future op.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::PersistentCore;
use crate::blockcache;
use crate::core::tree::{self, Item};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;

/// One batch element: a record going in, or a key going out.
#[derive(Clone)]
pub(super) enum Op {
    Put(Item),
    Del(Vec<u8>),
}

impl Op {
    pub(super) fn key(&self) -> &[u8] {
        match self {
            Op::Put((k, _, _)) => k,
            Op::Del(k) => k,
        }
    }
}

/// The graft's outcome: the new drafts — children before parents, the
/// new root last, `tree::build`'s order (empty — nothing changed, or
/// the tree emptied), the pages the dirty paths retired (an unchanged
/// frame keeps its page), and whether the batch emptied the tree (the
/// table's root becomes 0).
pub(super) struct Grafted {
    pub(super) drafts: Vec<tree::Node>,
    pub(super) replaced: Vec<PageId>,
    pub(super) emptied: bool,
}

/// A visited node on a dirty path: its page (a death this round, if
/// the rewrite changes it), its parse, and its share of the round's
/// sorted batch.
struct Frame {
    p: PageId,
    node: tree::Node,
    batch: Vec<Op>,
}

/// The report up from one rewritten node: the draft replacing the old
/// page (the old page itself, for a rewrite that changed nothing; 0 —
/// the node emptied and its route dies), the subtree's lo key (the
/// parent's route), and the right siblings a repack was born.
struct Up {
    id: PageId,
    lo: Vec<u8>,
    cuts: Vec<(PageId, Vec<u8>)>,
}

/// Graft `batch` (sorted by key) onto the tree at `root`
/// (never 0 — an empty table is built whole): see [`Grafted`]. `fuel0`
/// bounds the reads: more nodes than the file holds can only be
/// corruption.
pub(super) async fn run<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    root: PageId,
    batch: Vec<Op>,
    fuel0: u64,
) -> Result<Grafted, StoreError> {
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
                let i = tree::route_at(&f.node, f.batch[at].key()).unwrap_or(0);
                let mut end = at + 1;
                while end < f.batch.len()
                    && tree::route_at(&f.node, f.batch[end].key()).unwrap_or(0) == i
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
    let mut replaced: Vec<PageId> = Vec::new();
    for layer in layers.iter().rev() {
        for f in layer {
            let (up, retired) = rewrite(f, &ups, &mut drafts)?;
            if retired {
                replaced.push(f.p);
            }
            ups.insert(f.p, up);
        }
    }
    let mut up = ups.remove(&root).expect("the root is a frame");
    if up.id == 0 {
        return Ok(Grafted {
            drafts,
            replaced,
            emptied: true,
        });
    }
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
    Ok(Grafted {
        drafts,
        replaced,
        emptied: false,
    })
}

/// One frame's rewrite: a leaf merges its share in, an interior
/// re-points its dirty children; either way an overflowing node repacks
/// into as many siblings as fit — the same greedy packing as the
/// wholesale build. The report pairs with whether the rewrite retired
/// the old page: a rewrite that lands on the very same records keeps
/// it (the skip cascades — the parent re-points nothing), an emptied
/// node drops its route (`id` 0).
fn rewrite(
    f: &Frame,
    ups: &HashMap<PageId, Up>,
    drafts: &mut Vec<tree::Node>,
) -> Result<(Up, bool), StoreError> {
    if f.node.level == 0 {
        let merged = merge(&f.node, &f.batch);
        if same_records(&f.node, &merged) {
            return Ok((keep(f), false));
        }
        if merged.is_empty() {
            return Ok((dropped(), true));
        }
        Ok((
            repack(0, merged, tree::LEAF_MAX, tree::leaf_e, drafts)?,
            true,
        ))
    } else {
        let ts = repoint(&f.node, ups);
        if same_routes(&f.node, &ts) {
            return Ok((keep(f), false));
        }
        if ts.is_empty() {
            return Ok((dropped(), true));
        }
        Ok((
            repack(f.node.level, ts, tree::NODE_MAX, tree::route_e, drafts)?,
            true,
        ))
    }
}

/// The unchanged frame's report: the old page stays, no siblings.
fn keep(f: &Frame) -> Up {
    Up {
        id: f.p,
        lo: f.node.es[0].0.clone(),
        cuts: Vec::new(),
    }
}

/// The emptied node's report: id 0 — the parent drops this route.
fn dropped() -> Up {
    Up {
        id: 0,
        lo: Vec::new(),
        cuts: Vec::new(),
    }
}

/// Did a merged leaf land on exactly the node's own records?
fn same_records(node: &tree::Node, ts: &[Item]) -> bool {
    node.es.len() == ts.len()
        && node
            .es
            .iter()
            .zip(&node.ids)
            .zip(ts)
            .all(|(((k, mt), &vr), (tk, tmt, tvr))| k == tk && mt == tmt && vr == *tvr)
}

/// Did a re-pointed interior land on exactly the node's own routes?
fn same_routes(node: &tree::Node, ts: &[Item]) -> bool {
    node.es.len() == ts.len()
        && node
            .es
            .iter()
            .zip(&node.ids)
            .zip(ts)
            .all(|(((lo, _), &id), (tlo, _, tid))| lo == tlo && id == *tid)
}

/// The packed records as drafts — one node, or a run of siblings born
/// of the repack; the report goes up. Never called on an empty `ts` —
/// the rewrite reports the emptied node before this.
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

/// The leaf's records with its share merged in — both sides sorted; an
/// equal key's old record steps aside (the delta is the newer half),
/// and a `Del` takes its record away without leaving one behind.
fn merge(node: &tree::Node, batch: &[Op]) -> Vec<Item> {
    let (es, mut out) = (&node.es, Vec::with_capacity(node.es.len() + batch.len()));
    let (mut i, mut j) = (0usize, 0usize);
    while i < es.len() || j < batch.len() {
        let old = match (es.get(i), batch.get(j)) {
            (Some((k, _)), Some(op)) => k.as_slice() < op.key(),
            (Some(_), None) => true,
            _ => false,
        };
        if old {
            out.push((es[i].0.clone(), es[i].1.clone(), node.ids[i]));
            i += 1;
        } else {
            match &batch[j] {
                Op::Put(it) => out.push(it.clone()),
                Op::Del(_) => {}
            }
            let k = batch[j].key();
            j += 1;
            if let Some((ok, _)) = es.get(i) {
                if ok.as_slice() == k {
                    i += 1; // the replaced or deleted record
                }
            }
        }
    }
    out
}

/// The interior's routes with its dirty children re-pointed: each
/// replaced child's report from below — its draft, its subtree's lo,
/// the right siblings born under it; an emptied child (id 0) drops its
/// route.
fn repoint(node: &tree::Node, ups: &HashMap<PageId, Up>) -> Vec<Item> {
    let mut out = Vec::with_capacity(node.ids.len());
    for ((lo, _), &id) in node.es.iter().zip(&node.ids) {
        if let Some(up) = ups.get(&id) {
            if up.id == 0 {
                continue; // the dropped route
            }
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

async fn read<D: IsFilesystem>(
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
