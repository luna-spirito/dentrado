//! The tables' B-tree: the page codec and the pure geometry — the
//! greedy packing the wholesale build and the incremental graft share.
//! The tree's only writer is the flush: an empty table is built whole
//! from the sorted records — the interior levels stack bottom-up,
//! every page written once; a standing tree takes the graft — only its
//! dirty paths change, the clean subtrees keep their pages. A read is
//! a descent: interior entries route by their subtree's smallest key,
//! the leaf holds the records.

use crate::error::StoreError;
use crate::page::{Cur, PAGE_SIZE, Page, PageId};

/// A record: the key, the inline bytes, the value chain's first page
/// (0 — inline only).
pub type Item = (Vec<u8>, Vec<u8>, PageId);

/// A node's payload: the page minus the 8-byte header.
const PAYLOAD: usize = PAGE_SIZE - 8;
/// The parse's leaf bound — and the pack's: a record costs at least
/// its root slot and two lengths — 16 bytes.
pub const LEAF_MAX: usize = (PAGE_SIZE - 8) / 16;
/// The parse's interior bound — and the pack's: a route costs at least
/// its child slot and two lengths — 16 bytes.
pub const NODE_MAX: usize = (PAGE_SIZE - 8) / 16;

/// A parsed node: `level` 0 — a leaf, its entries `(key, inline)`
/// zipped with the value roots in `ids`; a level above — interior, its
/// entries the routing keys (each subtree's smallest), `ids` the
/// children.
pub struct Node {
    pub level: u32,
    pub es: Vec<(Vec<u8>, Vec<u8>)>,
    pub ids: Vec<PageId>,
}

/// Write a node: `es` in key order (the caller's — packing produces
/// it), `ids` beside the entries (the value roots / the children).
pub fn write_node(p: &mut Page, level: u32, es: &[(Vec<u8>, Vec<u8>)], ids: &[PageId]) {
    let mut c = Cur::at(p);
    c.u32(es.len() as u32);
    c.u32(level);
    for &id in ids {
        c.u64(id);
    }
    for (k, mt) in es {
        c.u32(k.len().min(PAYLOAD) as u32);
        c.raw(&k[..k.len().min(PAYLOAD)]);
        c.u32(mt.len().min(PAYLOAD) as u32);
        c.raw(&mt[..mt.len().min(PAYLOAD)]);
    }
    c.fin();
}

/// Parse a node; counts and lengths clamp to the page — noise does not
/// inflate the parse.
pub fn parse_node(b: &Page) -> Node {
    let level = b.u32_at(4);
    let n = (b.u32_at(0) as usize).min(if level == 0 { LEAF_MAX } else { NODE_MAX });
    let ids: Vec<PageId> = (0..n).map(|i| b.u64_at(8 + 8 * i)).collect();
    let mut off = 8 + 8 * n;
    let mut es = Vec::with_capacity(n);
    for _ in 0..n {
        let kl = (b.u32_at(off) as usize).min(PAYLOAD);
        let k = b.slice_at(off + 4, kl).to_vec();
        off += 4 + kl;
        let ml = (b.u32_at(off) as usize).min(PAYLOAD);
        let mt = b.slice_at(off + 4, ml).to_vec();
        off += 4 + ml;
        es.push((k, mt));
    }
    Node { level, es, ids }
}

/// The leaf's exact record — its inline bytes and chain head.
pub fn leaf_find<'a>(n: &'a Node, k: &[u8]) -> Option<(&'a [u8], PageId)> {
    n.es.iter()
        .zip(&n.ids)
        .find(|((lk, _), _)| lk.as_slice() == k)
        .map(|((_, mt), &vr)| (mt.as_slice(), vr))
}

/// The interior's child index for `k` — the last routing key at or
/// below it (`None` — `k` sits below the whole tree).
pub fn route_at(n: &Node, k: &[u8]) -> Option<usize> {
    n.es.iter()
        .rev()
        .position(|(lk, _)| lk.as_slice() <= k)
        .map(|i| n.es.len() - 1 - i)
}

/// The interior's child page for `k`.
pub fn route(n: &Node, k: &[u8]) -> Option<PageId> {
    route_at(n, k).map(|i| n.ids[i])
}

// ## The geometry

/// A leaf record's cost in the page: the value-root slot, two lengths,
/// the bytes.
pub fn leaf_e((k, mt, _): &Item) -> usize {
    16 + k.len() + mt.len()
}

/// Could any leaf hold this record — the record surface's `BadKey`
/// line (a bigger one no page could hold, and the flush would refuse
/// the whole round).
pub fn leaf_holds(k: &[u8], mt: &[u8]) -> bool {
    16 + k.len() + mt.len() <= PAYLOAD
}

/// A route's cost: the child slot, two lengths, the key — the length
/// word of the (always empty) inline bytes is still written.
pub fn route_e((k, _, _): &Item) -> usize {
    16 + k.len()
}

/// A node's span over the level below: the items `[at, at+len)`, `lo`
/// — the span's smallest item (the routing key's source).
struct Span {
    at: usize,
    len: usize,
    lo: usize,
}

/// The greedy runs of `es` — each maximal while `cost` fits the payload
/// and the count holds, so every run but the last is full. A single
/// entry past the payload is refused loudly — no page could hold it.
pub fn pack<T>(
    es: &[T],
    cap: usize,
    cost: impl Fn(&T) -> usize,
) -> Result<Vec<(usize, usize)>, StoreError> {
    let mut runs = Vec::new();
    let (mut at, mut len, mut used) = (0usize, 0usize, 0usize);
    for (i, e) in es.iter().enumerate() {
        let c = cost(e);
        if c > PAYLOAD {
            return Err(StoreError::Steps("tree: record over page"));
        }
        if len > 0 && (used + c > PAYLOAD || len == cap) {
            runs.push((at, len));
            at = i;
            len = 0;
            used = 0;
        }
        len += 1;
        used += c;
    }
    if len > 0 {
        runs.push((at, len));
    }
    Ok(runs)
}

/// The greedy levels over sorted records: the leaves packed by bytes
/// and count, each interior level by routes and count, until one node
/// crowns. A record no leaf could hold is refused loudly — here,
/// before any page moves.
fn spans(sorted: &[Item]) -> Result<Vec<Vec<Span>>, StoreError> {
    if sorted.is_empty() {
        return Ok(Vec::new());
    }
    let mut levels: Vec<Vec<Span>> = vec![
        pack(sorted, LEAF_MAX, leaf_e)?
            .into_iter()
            .map(|(at, len)| Span { at, len, lo: at })
            .collect(),
    ];
    while levels.last().map_or(false, |l| l.len() > 1) {
        let below = levels.last().unwrap();
        let lv: Vec<Span> = pack(below, NODE_MAX, |s| route_e(&sorted[s.lo]))?
            .into_iter()
            .map(|(at, len)| Span {
                at,
                len,
                lo: below[at].lo,
            })
            .collect();
        levels.push(lv);
    }
    Ok(levels)
}

/// The wholesale build's page needs per level (level 0 — the leaves);
/// an empty table needs none.
pub fn plan(sorted: &[Item]) -> Result<Vec<usize>, StoreError> {
    Ok(spans(sorted)?.iter().map(|l| l.len()).collect())
}

/// Build the whole tree bottom-up over the stock `ids` — a level's
/// pages in a row, the leaves first — returning the pages in the same
/// order, the root last. The stock's length must be the plan's sum.
pub fn build(sorted: &[Item], ids: &[PageId]) -> Result<Vec<Page>, StoreError> {
    let levels = spans(sorted)?;
    debug_assert_eq!(
        ids.len(),
        levels.iter().map(|l| l.len()).sum::<usize>(),
        "the stock must match the plan"
    );
    let bases: Vec<usize> = levels
        .iter()
        .scan(0, |a, l| {
            let b = *a;
            *a += l.len();
            Some(b)
        })
        .collect();
    let mut out: Vec<Page> = Vec::with_capacity(ids.len());
    for (h, lv) in levels.iter().enumerate() {
        for sp in lv.iter() {
            let mut p = Page::zeroed();
            if h == 0 {
                let es: Vec<(Vec<u8>, Vec<u8>)> = sorted[sp.at..sp.at + sp.len]
                    .iter()
                    .map(|(k, mt, _)| (k.clone(), mt.clone()))
                    .collect();
                let vrs: Vec<PageId> = sorted[sp.at..sp.at + sp.len]
                    .iter()
                    .map(|(_, _, vr)| *vr)
                    .collect();
                write_node(&mut p, 0, &es, &vrs);
            } else {
                let cb = bases[h - 1];
                let es: Vec<(Vec<u8>, Vec<u8>)> = below_keys(levels[h - 1].as_slice(), sp, sorted);
                let ch: Vec<PageId> = (sp.at..sp.at + sp.len).map(|i| ids[cb + i]).collect();
                write_node(&mut p, h as u32, &es, &ch);
            }
            out.push(p);
        }
    }
    Ok(out)
}

/// An interior node's routing entries: each child span's smallest key.
fn below_keys(below: &[Span], sp: &Span, sorted: &[Item]) -> Vec<(Vec<u8>, Vec<u8>)> {
    below[sp.at..sp.at + sp.len]
        .iter()
        .map(|s| (sorted[s.lo].0.clone(), Vec::new()))
        .collect()
}

// ## The graft's virtual ids

/// A draft node's id before the stock lands: the high bit tags it, the
/// low bits index the graft's drafts — real ids are file offsets, far
/// below.
const VIRT: PageId = 1 << 63;

/// Draft `i`'s virtual id.
pub fn virt(i: usize) -> PageId {
    VIRT | i as PageId
}

/// A graft child against its stock: a virtual id takes the stock's
/// slot, a real one — a clean subtree's page — stays.
pub fn real(p: PageId, stock: &[PageId]) -> PageId {
    if p & VIRT == 0 {
        p
    } else {
        stock[(p & !VIRT) as usize]
    }
}
