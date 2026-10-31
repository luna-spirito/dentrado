//! The value-page codec: `[n u32][pad][next u64][payload]` — a single
//! header for the children, `n ≤ 1` (`0` — the last page of the
//! chain). Draft pages are value pages not yet published.

use crate::page::{PAGE_SIZE, Page, PageId};

/// Value-page payload: the page minus the 16-byte header.
pub const VAL_PAYLOAD: usize = PAGE_SIZE - 16;

// REVIEW 5: Ещё один метод на копирование. Он допустим не более предыдущего.
// ANSWER: the price of the frozen surface `f: Fn(&[u8]) -> (Vec<u8>, R)`
// — the payload is born as the caller's Vec and lands in the page, one
// copy on the way; the disk path itself is zero-copy (the codec writes
// into the pool's slot). A `Fn(&mut [u8]) -> R` surface would remove
// it — parked until the test surface may move.
/// Write a value page in place: the header plus the payload (a copy
/// with the tail zeroed, an overlong input cut to the payload).
pub fn write_val(p: &mut Page, next: PageId, last: bool, payload: &[u8]) {
    link_val(p, next, last);
    p.put_u32_at(4, 0); // the pad — a recycled slot may hold foreign bytes
    let n = payload.len().min(VAL_PAYLOAD);
    p.0[16..16 + n].copy_from_slice(&payload[..n]);
    p.0[16 + n..].fill(0);
}

// REVIEW 6: Этот метод делает очень странные предположения о структуре страницы.
// ANSWER: the assumptions are this module's own — the very layout
// `write_val` above writes; the header is one `[n][pad][next]` piece,
// resewn here as a whole. The general half of the wish (a `ValPage`
// view type instead of scattered offsets) is the open TODO in `page`'s
// head note.
/// Resew a value page's header (draft stitching in `put_cache`) — the
/// payload stays in place, only `next`/lastness change.
pub fn link_val(p: &mut Page, next: PageId, last: bool) {
    p.put_u32_at(0, if last { 0 } else { 1 });
    p.put_u64_at(8, next);
}

// REVIE 7: Этот метод делает очень странные предположения о структуре страницы. Все эти методы относятся, судя по всему, к Freelist, так что они делают вообще в этом модуле?
// this one is the value chain's reader, it stays.
// ANSWER: none of these touch the freelist — that page kind is
// `chunk`'s; these are the value chain's codec, and every offset they
// assume is written by `write_val` in this very module.
/// A value page's children: `next` (0 — last) and the payload.
pub fn val_split(b: &Page) -> (PageId, &[u8]) {
    let next = if b.u32_at(0) == 0 { 0 } else { b.u64_at(8) };
    (next, b.slice_at(16, VAL_PAYLOAD))
}
