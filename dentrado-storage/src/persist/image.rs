//! The image: recovery (the newest valid SB wins) and the invariant
//! against the file's bytes. All reads go through the pool — pages live
//! only there.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

use super::PersistentCore;
use crate::blockcache;
use crate::core::freelist::{ChainEntry, chunk_cells, chunk_next};
use crate::core::sb::{Sb, parse_sb};
use crate::core::tree::{Item, parse_node};
use crate::disk::Storage;
use crate::error::StoreError;
use crate::page::{PageId, fnv_id};
use crate::store::{Store, Violation};

impl<D: Storage> Store<D> {
    /// Recovery: the newest valid SB (both chains walk through, counts
    /// and sums agree) — its table set must equal the opener's; both
    /// eof — a fresh store; content without a valid image — death.
    /// Reads go through the pool: pages live only there.
    pub async fn open(
        disk: &D,
        path: &str,
        cap: usize,
        n_tables: usize,
    ) -> Result<Store<D>, StoreError>
    where
        D: 'static,
    {
        let file = D::open(disk, path)
            .await
            .map_err(|e| StoreError::Io(e.to_string()))?;
        let inner = Rc::new(RefCell::new(PersistentCore::fresh(file, cap, n_tables)));
        // An error from here on drops the handles — and with them the
        // driver and the round agent, whose `Rc`s would otherwise keep
        // the store alive forever.
        let driver = blockcache::spawn_driver(&inner);
        let flusher = super::flush::spawn_agent(&inner);
        let (ra, sa) = sb_slot::<D>(&inner, 0).await;
        let (rb, sb1) = sb_slot::<D>(&inner, 1).await;
        let mut pick: Option<Sb> = None;
        for sb in [sa, sb1].into_iter().flatten() {
            if sb_valid::<D>(&inner, &sb).await {
                pick = Some(match pick {
                    Some(p) if p.r#gen >= sb.r#gen => p,
                    _ => sb,
                });
            }
        }
        match pick {
            Some(sb) if sb.roots.len() != n_tables => return Err(StoreError::TableMismatch),
            Some(sb) => inner.borrow_mut().of_sb(&sb),
            None if ra || rb => return Err(StoreError::CorruptImage),
            None => {}
        }
        Ok(Store {
            inner,
            _driver: driver,
            _flusher: flusher,
        })
    }

    /// The store's invariant against the file's bytes (checked at rest
    /// — a flush round has finished): the heads' parities, the tails
    /// taken by no one, the dispenser (the heads) not crossing the
    /// closure of the newest valid image. All reads go through the
    /// pool.
    pub async fn ok_store(&self) -> bool {
        let ram_ok = {
            let inner = self.inner.borrow();
            let ram = inner.core.ram();
            let (h0, h1) = (&ram.heads[0], &ram.heads[1]);
            h0.iter().all(|p| p % 2 == 0)
                && h1.iter().all(|p| p % 2 == 1)
                && ram.nexts[0] % 2 == 0
                && ram.nexts[1] % 2 == 1
                && !ram.taken[0]
                && !ram.taken[1]
                && !h0.iter().any(|p| h1.contains(p))
                && !ram.drafts.iter().any(|p| h0.contains(p) || h1.contains(p))
        };
        if !ram_ok {
            return false;
        }
        let (_, sa) = sb_slot::<D>(&self.inner, 0).await;
        let (_, sb1) = sb_slot::<D>(&self.inner, 1).await;
        let mut cands: Vec<(Sb, Vec<PageId>, Vec<PageId>)> = Vec::new();
        for sb in [sa, sb1].into_iter().flatten() {
            if sb_valid::<D>(&self.inner, &sb).await {
                if let Some(r) = self.disk_ref(&sb).await {
                    cands.push((sb, r.0, r.1));
                }
            }
        }
        let Some((_, tree, vals)) = cands.into_iter().max_by_key(|(s, _, _)| s.r#gen) else {
            return true; // no valid image — the dispenser has no one to conflict with
        };
        !{
            let inner = self.inner.borrow();
            let ram = inner.core.ram();
            tree.iter()
                .chain(vals.iter())
                .any(|p| ram.heads.iter().any(|h| h.contains(p)))
        }
    }

    /// The page partition at rest: every page below the newest image's
    /// counters is the root's, a chain cell, or held in RAM (the heads,
    /// the drafts, the undrained deaths). The SB slots (pages 0 and 1)
    /// sit outside — permanently occupied.
    pub async fn partition_ok(&self) -> bool {
        let (_, sa) = sb_slot::<D>(&self.inner, 0).await;
        let (_, sb1) = sb_slot::<D>(&self.inner, 1).await;
        let mut cands: Vec<Sb> = Vec::new();
        for sb in [sa, sb1].into_iter().flatten() {
            if sb_valid::<D>(&self.inner, &sb).await {
                cands.push(sb);
            }
        }
        let Some(sb) = cands.into_iter().max_by_key(|s| s.r#gen) else {
            return true; // no valid image — nothing to partition against
        };
        let fuel = sb.next0.max(sb.next1) + 1;
        let Some(refs) = self.disk_ref(&sb).await else {
            return false;
        };
        let parts = match (
            walk_parts::<D>(&self.inner, sb.e0, fuel).await,
            walk_parts::<D>(&self.inner, sb.e1, fuel).await,
        ) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return false,
        };
        let covered: Vec<PageId> = refs
            .0
            .iter()
            .chain(refs.1.iter())
            .chain(
                parts
                    .0
                    .iter()
                    .chain(parts.1.iter())
                    .flat_map(|(chunk, cells)| [chunk].into_iter().chain(cells.iter())),
            )
            .copied()
            .collect();
        let held = {
            let inner = self.inner.borrow();
            let ram = inner.core.ram();
            ram.heads
                .iter()
                .flatten()
                .chain(ram.drafts.iter())
                .chain(ram.held.iter())
                .copied()
                .collect::<Vec<_>>()
        };
        let in_world = |p: u64| {
            if p % 2 == 0 {
                p < sb.next0
            } else {
                p < sb.next1
            }
        };
        let miss: Vec<u64> = (2..fuel)
            .filter(|&p| in_world(p))
            .filter(|&p| !covered.contains(&p) && !held.contains(&p))
            .collect();
        miss.is_empty()
    }

    /// The page partition, exact: every page below the newest image's
    /// counters carries a legal combination of roles and only legal
    /// ones. The roles: the tables' `tree` pages and the `value` chains'
    /// pages; the chains' `chunk` containers and their `cell`s — a
    /// `front` cell above the pool's live bookmark (RAM-side: the head
    /// materializes it, or it is the limbo — a live draft, an undrained
    /// death, a retired tree) and a `free` cell at-and-below it
    /// (write-once, only ever eaten); the RAM `head`s, `draft`s, `held`
    /// deaths, task-eaten containers in `spent` (still links until the
    /// next round lists them) and the deltas' uncommitted pages. The
    /// design's pairings: a tree page and a free cell stand strictly
    /// alone; a value at most once, possibly still held by a pending
    /// death; a chunk alone or riding `spent`; a front cell at most
    /// once, with at most one RAM role beside it. Every other
    /// combination — and a duplicate listing above all — is the
    /// double-issue family: the same page listed twice serves twice.
    pub async fn partition_exact(&self) -> Result<(), Vec<Violation>> {
        let (_, sa) = sb_slot::<D>(&self.inner, 0).await;
        let (_, sb1) = sb_slot::<D>(&self.inner, 1).await;
        let mut cands: Vec<Sb> = Vec::new();
        for sb in [sa, sb1].into_iter().flatten() {
            if sb_valid::<D>(&self.inner, &sb).await {
                cands.push(sb);
            }
        }
        let Some(sb) = cands.into_iter().max_by_key(|s| s.r#gen) else {
            return Ok(()); // no valid image — nothing to partition against
        };
        let fuel = sb.next0.max(sb.next1) + 1;
        let mut roles: BTreeMap<PageId, Vec<&'static str>> = BTreeMap::new();
        let mut bad: Vec<Violation> = Vec::new();
        fn add(roles: &mut BTreeMap<PageId, Vec<&'static str>>, p: PageId, r: &'static str) {
            roles.entry(p).or_default().push(r);
        }
        match self.disk_ref(&sb).await {
            Some((tree, vals)) => {
                for &p in &tree {
                    add(&mut roles, p, "tree");
                }
                for &p in &vals {
                    add(&mut roles, p, "value");
                }
            }
            None => bad.push(Violation::Broken("a value chain broke past a valid SB")),
        }
        let ram = self.inner.borrow().core.ram();
        for (p, e) in [(0usize, sb.e0), (1usize, sb.e1)] {
            let parts = match walk_parts::<D>(&self.inner, e, fuel).await {
                Ok(x) => x,
                Err(_) => {
                    bad.push(Violation::Broken("a pool chain broke past a valid SB"));
                    continue;
                }
            };
            // The region boundary: cells of the bookmark's chunk and of
            // everything below it are the free run; above — the front.
            // An exhausted bookmark (`page = 0`) leaves the whole walk
            // front; a bookmark off its chain is itself corruption.
            let mut region = "front";
            let mut on_chain = ram.tails[p].page == 0;
            for (chunk, cells) in parts {
                add(&mut roles, chunk, "chunk");
                if chunk == ram.tails[p].page {
                    on_chain = true;
                    region = "free";
                }
                for &c in &cells {
                    add(&mut roles, c, region);
                }
            }
            if !on_chain {
                bad.push(Violation::Broken("the runtime bookmark is off its chain"));
            }
        }
        for h in &ram.heads {
            for &x in h {
                add(&mut roles, x, "head");
            }
        }
        for &x in &ram.drafts {
            add(&mut roles, x, "draft");
        }
        for &x in &ram.held {
            add(&mut roles, x, "held");
        }
        for &x in &ram.spent {
            add(&mut roles, x, "spent");
        }
        for &x in &ram.delta_pages {
            add(&mut roles, x, "delta");
        }
        for (&p, rs) in &roles {
            if !legal_roles(rs) {
                bad.push(Violation::Roles(p, rs.clone()));
            }
        }
        let in_world = |p: u64| {
            if p % 2 == 0 {
                p < sb.next0
            } else {
                p < sb.next1
            }
        };
        for p in (2..fuel)
            .filter(|&p| in_world(p))
            .filter(|p| !roles.contains_key(p))
        {
            bad.push(Violation::Missing(p));
        }
        if bad.is_empty() { Ok(()) } else { Err(bad) }
    }

    /// The image's closure: every table's tree pages + all the value
    /// chains (through the pool).
    async fn disk_ref(&self, sb: &Sb) -> Option<(Vec<PageId>, Vec<PageId>)> {
        let fuel = sb.next0.max(sb.next1) + 1;
        let mut tree = Vec::new();
        let mut vals = Vec::new();
        for &r in &sb.roots {
            let (es, ps) = walk_tree::<D>(&self.inner, r, fuel).await.ok()?;
            tree.extend(ps);
            for (_, _, vr) in es {
                if vr != 0 {
                    vals.extend(self.walk_value(vr, fuel).await.ok()?);
                }
            }
        }
        Some((tree, vals))
    }
}

/// Walk a table's tree breadth-first: its records in key order (the
/// leaves come out left to right) and all its pages.
pub(super) async fn walk_tree<D: Storage>(
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

/// Walk a tail chain, collecting each chunk page with its live cells —
/// the chain's occupancy in walk order, from the entry down.
async fn walk_parts<D: Storage>(
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
pub(super) async fn walk_chain_raw<D: Storage>(
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

/// The design's legal role combinations (see [`Store::partition_exact`]):
/// a tree page and a free-run cell stand strictly alone; a value at most
/// once, possibly still held by a pending death; a chunk container
/// alone — or still linked while its page rides `spent` (the task-eaten
/// containers stay links of the committed image until the next round
/// lists them); a front cell at most once, with at most one RAM role
/// beside it; a RAM-only page carries exactly one role.
fn legal_roles(rs: &[&'static str]) -> bool {
    let n = |r: &'static str| rs.iter().filter(|&&x| x == r).count();
    let ram = n("head") + n("draft") + n("held") + n("spent") + n("delta");
    match (n("tree"), n("value"), n("chunk"), n("front"), n("free")) {
        (1, 0, 0, 0, 0) => rs.len() == 1,
        (0, 1, 0, 0, 0) => rs.len() == 1 || (rs.len() == 2 && n("held") == 1),
        (0, 0, 1, 0, 0) => rs.len() == 1 || (rs.len() == 2 && n("spent") == 1),
        (0, 0, 0, 1, 0) => rs.len() == 1 || (rs.len() == 2 && ram == 1),
        (0, 0, 0, 0, 1) => rs.len() == 1,
        (0, 0, 0, 0, 0) => rs.len() == 1 && ram == 1,
        _ => false,
    }
}

/// An SB slot: readability (eof/failure — none) and the parse.
async fn sb_slot<D: Storage>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    id: PageId,
) -> (bool, Option<Sb>) {
    match blockcache::read::<D, _>(inner, id, parse_sb).await {
        Ok(sb) => (true, sb),
        Err(_) => (false, None),
    }
}

/// A slot is valid if the counters carry their parities, both chains
/// walk through, and the counts and sums agree.
async fn sb_valid<D: Storage>(inner: &Rc<RefCell<PersistentCore<D>>>, sb: &Sb) -> bool {
    let fuel = sb.next0.max(sb.next1) + 1;
    let Ok(c0) = walk_chain_raw::<D>(inner, sb.e0, fuel).await else {
        return false;
    };
    let Ok(c1) = walk_chain_raw::<D>(inner, sb.e1, fuel).await else {
        return false;
    };
    sb.next0 % 2 == 0
        && sb.next1 % 2 == 1
        && c0.len() as u64 == sb.c0
        && c1.len() as u64 == sb.c1
        && fnv_id(&c0) == sb.s0
        && fnv_id(&c1) == sb.s1
}
