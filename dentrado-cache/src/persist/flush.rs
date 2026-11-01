//! One flush round, end to end: the round agent — the perpetual task
//! that owns every round — the round's body, and its two atomic
//! sections, the seal and the commit. The flight (the round's locals)
//! lives in [`super::flight`].

use std::cell::RefCell;
use std::rc::Rc;

use compio::runtime::JoinHandle;
use futures::channel::oneshot;

use super::PersistentCore;
use super::flight::{Flight, FlipData};
use super::graft;
use super::image::walk_chain_raw;
use crate::blockcache;
use crate::core::freelist::{CHUNK_SLOTS, ChainEntry, chunk_cells, chunk_next, write_chunk};
use crate::core::sb::write_sb;
use crate::core::tree;
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{Page, PageId, fnv_id};
use crate::storage::Storage;

// ## The agent

impl<D: IsFilesystem> Storage<D> {
    /// The flush is total: the request lands in the round agent's
    /// mailbox, the reply carries the round's verdict — everyone
    /// drained into one batch shares one round. A dropped caller
    /// cancels nothing: only its reply address dies, the round runs to
    /// its end (a checkpoint is observer-independent, like the pool's
    /// io). The agent's death — the store's own drop — is the only
    /// refusal left.
    pub async fn flush(&self) -> Result<(), StoreError> {
        let (tx, rx) = oneshot::channel::<Result<(), StoreError>>();
        {
            let mut inner = self.inner.borrow_mut();
            inner.mail.push(tx);
            inner.mail_ev.notify(1);
        }
        match rx.await {
            Ok(r) => r,
            Err(_) => Err(StoreError::ChannelClosed),
        }
    }
}

/// The round agent — the flush's counterpart of the io driver: a
/// round must complete whatever its requesters do, so it lives in a
/// task the store dies with, never in a caller's future (a dropped
/// `flush` cancels only its reply address). Each batch drained from
/// the mailbox is spun as exactly one round; a body failure is data —
/// the reply — never the loop's death, for the agent must not die
/// before the store does, or the mailbox would fill with letters
/// nobody answers. The listener is taken out before the mailbox is
/// looked into, and the only await is an empty mailbox under a live
/// listener: a letter's push and ring are one section of the single
/// lock, and a letter landing anywhere else sits in the mailbox for
/// the next look — a wake can be spurious, never lost.
async fn agent<D: IsFilesystem>(inner: Rc<RefCell<PersistentCore<D>>>) -> ! {
    loop {
        let ln = inner.borrow().mail_ev.listen();
        let batch = std::mem::take(&mut inner.borrow_mut().mail);
        if batch.is_empty() {
            ln.await;
            continue;
        }
        drop(ln);
        let r = round::<D>(&inner).await;
        for tx in batch {
            let _ = tx.send(r.clone());
        }
    }
}

/// Spawn the round agent at store birth — the driver's own pattern:
/// the handle goes to `Store`, its drop is the agent's death sentence.
pub(super) fn spawn_agent<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
) -> JoinHandle<!> {
    compio::runtime::spawn(agent(Rc::clone(inner)))
}

/// Take `n` pages from the flight: the head's remainder → the tail's
/// chunks → growing the round's own counter. The flight is the
/// accumulator (the round's locals).
async fn take_flight<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    fl: &mut Flight,
    fuel0: u64,
    mut n: usize,
) -> Result<Vec<PageId>, StoreError> {
    let mut out = Vec::new();
    let mut fuel = fuel0;
    loop {
        if n == 0 {
            return Ok(out);
        }
        if fuel == 0 {
            return Err(StoreError::Steps("takeFlight: steps"));
        }
        fuel -= 1;
        if let Some(p) = fl.head.pop() {
            out.push(p);
            n -= 1;
        } else if fl.tail.page == 0 {
            let p = fl.next;
            fl.next += 2;
            fl.head.push(p);
        } else {
            let tp = fl.tail.page;
            let off = fl.tail.off;
            let (cells, nx) = blockcache::read::<D, _>(inner, tp, move |b| {
                (chunk_cells(b, off as usize), chunk_next(b))
            })
            .await?;
            fl.eat(cells, nx, tp);
        }
    }
}

/// The stock fixpoint: snapshot → cells → need → take what is
/// missing (eating the tail grows the drains — the cells grow —
/// resnapshot). The snapshot's one live read is the husk — the
/// mid-round retires into the taken pool, whose listings retire
/// with this round's fronts.
async fn flip_loop<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    mut fl: Flight,
    need_pages: usize,
) -> Result<(Flight, FlipData), StoreError> {
    let mut stock: Vec<PageId> = Vec::new();
    let mut fuel = fl.next + 4;
    // The taken pool's runs are separate (its bookmark must land at
    // a chunk boundary); the copied pool's make do with one.
    let chunks = |l: usize, f: usize, split: bool| {
        if split {
            l.div_ceil(CHUNK_SLOTS) + f.div_ceil(CHUNK_SLOTS)
        } else {
            (l + f).div_ceil(CHUNK_SLOTS)
        }
    };
    loop {
        if fuel == 0 {
            return Err(StoreError::Steps("flip: steps"));
        }
        fuel -= 1;
        let husk = {
            let in_ = inner.borrow();
            in_.core.husk()
        };
        let (l0, f0) = fl.cells(&husk, 0);
        let (l1, f1) = fl.cells(&husk, 1);
        let need = need_pages
            + chunks(l0.len(), f0.len(), fl.ep != 0)
            + chunks(l1.len(), f1.len(), fl.ep != 1);
        if stock.len() >= need {
            let (ep, old_gen) = (fl.ep, fl.old_gen);
            return Ok((
                fl,
                FlipData {
                    stock,
                    limbo: [l0, l1],
                    free: [f0, f1],
                    ep,
                    old_gen,
                },
            ));
        }
        let want = need - stock.len();
        let more = take_flight::<D>(inner, &mut fl, 2 * want as u64 + 2, want).await?;
        stock.extend(more);
    }
}

/// One round: seal → per table, graft the delta onto the standing
/// tree (an empty delta keeps it — no churn; an empty table is built
/// whole) → the stock fixpoint → the writes (trees, chunks) → write
/// back the dirty and sync the data → the full chains (counts, sums)
/// → the SB into the alternating slot → sync — the commit point → the
/// RAM commit in one borrow. The retired pages — the grafted paths'
/// old pages — ride the round's limbo cells (crash safety) and die at
/// the commit (EBR — the readers mid-descent).
async fn round<D: IsFilesystem>(inner: &Rc<RefCell<PersistentCore<D>>>) -> Result<(), StoreError> {
    // The seal: everything the round starts with leaves for the
    // flight — the round's locals; the core's flush pool stays
    // empty until the commit.
    let mut fl = {
        let mut in_ = inner.borrow_mut();
        let s = in_.core.seal();
        Flight {
            ep: s.ep,
            head: s.ahead,
            tail: s.atail,
            next: s.anext,
            drains: Vec::new(),
            spent: s.spent,
            dying: s.dying,
            pend: s.pend,
            drafts: s.drafts,
            bhead: s.bhead,
            btail: s.btail,
            bnext: s.bnext,
            snaps: s.snaps,
            roots: s.roots,
            old_tree: Vec::new(),
            old_gen: in_.r#gen,
        }
    };
    // The trees, per table: the delta as the sorted batch, then a
    // standing tree takes the graft — only its dirty paths change —
    // while an empty table is built whole. The grafts' reads run
    // before any stock moves; their bound is the seal-time
    // counters.
    let fn_next = fl.next.max(fl.bnext) + 1;
    let n = fl.snaps.len();
    let mut builds: Vec<Vec<tree::Item>> = (0..n).map(|_| Vec::new()).collect();
    let mut grafts: Vec<Vec<tree::Node>> = (0..n).map(|_| Vec::new()).collect();
    let mut counts: Vec<usize> = vec![0; n];
    let mut need_pages = 0usize;
    for t in 0..n {
        if fl.snaps[t].is_empty() {
            continue; // nothing new — the old tree stands
        }
        let mut batch: Vec<tree::Item> = fl.snaps[t]
            .iter()
            .map(|(k, cv)| {
                (
                    k.clone(),
                    cv.mt.clone(),
                    cv.pages.first().copied().unwrap_or(0),
                )
            })
            .collect();
        batch.sort_by(|a, b| a.0.cmp(&b.0));
        if fl.roots[t] == 0 {
            counts[t] = tree::plan(&batch)?.iter().sum();
            builds[t] = batch;
        } else {
            let (drafts, replaced) = graft::run::<D>(inner, fl.roots[t], batch, fn_next).await?;
            fl.old_tree.extend(replaced);
            counts[t] = drafts.len();
            grafts[t] = drafts;
        }
        need_pages += counts[t];
    }
    let (fl2, fd) = flip_loop::<D>(inner, fl, need_pages).await?;
    let FlipData {
        stock,
        limbo,
        free,
        ep,
        old_gen,
    } = fd;
    let nep = 1 - ep;
    let [l0, l1] = limbo;
    let [f0, f1] = free;
    let (ln, le) = if nep == 0 { (l0, l1) } else { (l1, l0) };
    let (nfree, fe) = if nep == 0 { (f0, f1) } else { (f1, f0) };

    // The taken pool's chain: the limbo run heads the free run —
    // the limbo (pendings, drafts, retired trees) is re-copied
    // every round and retires wholesale at the next taking, the
    // free run is written once and only ever eaten. The copied
    // pool's chunks are all front (above its live bookmark) — one
    // run.
    let nl = ln.len().div_ceil(CHUNK_SLOTS);
    let need_n = nl + nfree.len().div_ceil(CHUNK_SLOTS);
    let ecells: Vec<PageId> = le.into_iter().chain(fe).collect();
    let used = need_pages + need_n + ecells.len().div_ceil(CHUNK_SLOTS);
    // The surplus — stock beyond need (popping it out of the head
    // shrank the cells, so the need sank under the taken) — returns
    // into the pool's head at the commit; its listing retired with
    // this round's fronts, the next round packs it again.
    let surplus = &stock[used..];
    let chunk_ps = &stock[need_pages..used];
    let nlimbo_ps: Vec<PageId> = chunk_ps[..nl].to_vec();
    let nfree_ps: Vec<PageId> = chunk_ps[nl..need_n].to_vec();
    let ep_ps: Vec<PageId> = chunk_ps[need_n..].to_vec();
    // The continuations past the new chunks: what the flush left
    // uneaten (the taken pool) or the seal-time bookmark (the
    // copied pool — the chunks the tasks eat during the round stay
    // linked, their cells stale-listed). The last new chunk points
    // here — NOT at the chain's entry (that would be a ring).
    let ftail = fl2.tail;
    let btail = fl2.btail;
    // The taken pool's new bookmark: the free run's head, past the
    // limbo — consumption starts with the round's fresh free
    // cells.
    let mark = nfree_ps
        .first()
        .copied()
        .map(ChainEntry::full)
        .unwrap_or(ftail);
    let entry_n = nlimbo_ps
        .first()
        .or(nfree_ps.first())
        .copied()
        .map(ChainEntry::full)
        .unwrap_or(ftail);
    let entry_e = ep_ps
        .first()
        .copied()
        .map(ChainEntry::full)
        .unwrap_or(btail);
    // The SB's counters: the round's own — exact, tasks cannot
    // touch it — and the task pool's seal-time copy (mid-round
    // growth stays above it, unlisted and unborn).
    let (sn0, sn1) = if nep == 0 {
        (fl2.next, fl2.bnext)
    } else {
        (fl2.bnext, fl2.next)
    };
    debug_assert!(
        ln.iter()
            .chain(&nfree)
            .all(|&x| (x % 2) as usize == nep && x < fl2.next)
            && ecells
                .iter()
                .all(|&x| (x % 2) as usize == ep && x < fl2.bnext),
        "a cell outside its pool's parity or counter"
    );
    // The trees: each table over its stock slice, written page by
    // page into the stock's slots (the pool waits out flights and
    // capacity on its own). A graft's drafts resolve their virtual
    // ids against the stock — a clean child keeps its page. The
    // root is the slice's last page — the build's and the graft's
    // orders put it there alike.
    let mut new_roots = fl2.roots.clone();
    let mut at = 0usize;
    for t in 0..n {
        if counts[t] == 0 {
            continue;
        }
        let ids = &stock[at..at + counts[t]];
        let imgs = if grafts[t].is_empty() {
            tree::build(&builds[t], ids)?
        } else {
            grafts[t]
                .iter()
                .map(|d| {
                    let rid: Vec<PageId> = d.ids.iter().map(|&c| tree::real(c, ids)).collect();
                    let mut p = Page::zeroed();
                    tree::write_node(&mut p, d.level, &d.es, &rid);
                    p
                })
                .collect()
        };
        for (&id, img) in ids.iter().zip(imgs) {
            blockcache::init::<D, _>(inner, id, move |b| b.0.copy_from_slice(&img.0)).await?;
        }
        new_roots[t] = ids[counts[t] - 1];
        at += counts[t];
    }
    init_chain::<D>(inner, &nlimbo_ps, mark, &ln).await?;
    init_chain::<D>(inner, &nfree_ps, ftail, &nfree).await?;
    init_chain::<D>(inner, &ep_ps, btail, &ecells).await?;
    blockcache::write_back_all::<D>(inner).await?;
    sync::<D>(inner).await?;
    // The walks' bound is the round's own pages — the flight
    // counter and the seal-time copy: longer chains can only be
    // corruption.
    let tn = walk_chain_raw::<D>(inner, entry_n, fn_next).await?;
    let te = walk_chain_raw::<D>(inner, entry_e, fn_next).await?;
    let (even, odd) = if nep == 0 {
        (entry_n, entry_e)
    } else {
        (entry_e, entry_n)
    };
    let (teven, todd) = if nep == 0 { (&tn, &te) } else { (&te, &tn) };
    let (even_cells, odd_cells) = (teven.len() as u64, todd.len() as u64);
    let (even_sum, odd_sum) = (fnv_id(teven), fnv_id(todd));
    // The SB is a full-fledged pool page: settle it into a slot,
    // save, sync — the commit point.
    let sb_id = old_gen % 2;
    let sb_gen = old_gen + 1;
    let sb_roots = new_roots.clone();
    blockcache::init::<D, _>(inner, sb_id, move |b| {
        write_sb(
            b, sb_gen, sn0, sn1, &sb_roots, even, odd, even_cells, odd_cells, even_sum, odd_sum,
        )
    })
    .await?;
    blockcache::ensure_saved::<D>(inner, sb_id).await?;
    sync::<D>(inner).await?;
    // The RAM commit in one borrow: the pools land back (the
    // bookmark and the fronts with them), the old trees die (EBR),
    // the deltas purge the sealed, the world moves to the new
    // image.
    {
        let mut in_ = inner.borrow_mut();
        let (f0, f1) = if nep == 0 {
            (&nlimbo_ps, &ep_ps)
        } else {
            (&ep_ps, &nlimbo_ps)
        };
        in_.core.commit(
            fl2.dying
                .iter()
                .chain(fl2.drains.iter())
                .chain(fl2.spent.iter())
                .copied(),
            &fl2.old_tree,
            mark,
            fl2.next,
            surplus,
            [f0, f1],
            &fl2.snaps,
            &new_roots,
        );
        in_.r#gen += 1;
    }
    Ok(())
}

/// The writes for the chain's new head: chunk pages with next
/// pointers, the last one — onto the continuation page. Every page
/// is assembled right in the pool's slot — no value pages are in
/// flight.
async fn init_chain<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
    pages: &[PageId],
    entry: ChainEntry,
    cells: &[PageId],
) -> Result<(), StoreError> {
    for (i, g) in cells.chunks(CHUNK_SLOTS).enumerate() {
        let nx = match pages.get(i + 1) {
            Some(&q) => q,
            // A continuation sits behind a page pointer, so it must
            // be a whole page: eating is page-granular, and the one
            // place a nonzero off can appear — an untouched
            // recovered head — goes straight into the SB, never
            // behind a next.
            None => {
                assert_eq!(entry.off, 0, "a continuation behind a next is not whole");
                entry.page
            }
        };
        let g = g.to_vec();
        blockcache::init::<D, _>(inner, pages[i], move |b| write_chunk(b, nx, &g)).await?;
    }
    Ok(())
}

/// Sync the file — the handle clones out of the cache's disk arena, the
/// await stays outside every borrow.
async fn sync<D: IsFilesystem>(inner: &Rc<RefCell<PersistentCore<D>>>) -> Result<(), StoreError> {
    let file = inner.borrow().core.file();
    D::sync(file)
        .await
        .map_err(|e| StoreError::Io(e.to_string()))
}
