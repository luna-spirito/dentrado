//! The freelist's chunk page: `[next u64][slots u64 × CHUNK_SLOTS]` —
//! a bare page id; zero slots are padding, the reader skips them. The
//! offset of the first live slot belongs to the entry alone
//! ([`ChainEntry`] — the SB's e0/e1): every page behind the entry is
//! full. (A departure from the model: its header is 16 bytes —
//! `[next][off][pad]` — with `off` always persisted 0; the port drops
//! it, 511 slots, all derived.)

use std::mem::size_of;

use crate::page::{Cur, PAGE_SIZE, Page, PageId};

/// The chunk page's header: the next page.
const HDR: usize = 8;

// REVIEW 1: shouldn't be magic constant, should be derived from others.
// ANSWER: derived — the page minus the 8-byte header, cut into `PageId`
// slots.
/// Cell slots in a freelist chain chunk page.
pub const CHUNK_SLOTS: usize = (PAGE_SIZE - HDR) / size_of::<PageId>();

/// An entry into a tail chain: the chunk page and the offset of its
/// first live slot — the only place an offset can appear; `page = 0` —
/// no chain.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ChainEntry {
    pub page: PageId,
    pub off: u32, // REVIEW 2: all pages in the chain besides the first one are expected to be full, why shouldn't they?
                  // So this sounds more like superblock-specific field
                  // It's best to generalize freelist datastructure instead of having orphan write_chunk & others.
                  // ANSWER: agreed and done — the offset lives in the
                  // entry alone (here and in the SB); the pages behind
                  // an entry are full and carry none.
}

impl ChainEntry {
    /// An entry into a whole page — eating is page-granular, so the
    /// page after an eaten one begins full.
    pub fn full(page: PageId) -> Self {
        Self { page, off: 0 }
    }

    /// An entry as parsed off a superblock: a dead page (`0` — no
    /// chain) carries no offset, so junk normalizes to the empty entry.
    pub fn at(page: PageId, off: u32) -> Self {
        if page == 0 {
            Self::default()
        } else {
            Self { page, off }
        }
    }
}

/// Write a chunk page in place.
pub fn write_chunk(p: &mut Page, nx: PageId, cells: &[PageId]) {
    let mut c = Cur::at(p);
    c.u64(nx);
    for k in 0..CHUNK_SLOTS {
        c.u64(cells.get(k).copied().unwrap_or(0));
    }
    c.fin();
}

/// The live cells of a chunk page from an offset.
pub fn chunk_cells(b: &Page, off: usize) -> Vec<PageId> {
    (off.min(CHUNK_SLOTS)..CHUNK_SLOTS)
        .map(|i| b.u64_at(HDR + 8 * i))
        .filter(|&x| x > 0)
        .collect()
}

/// The next page of a chain chunk.
pub fn chunk_next(b: &Page) -> PageId {
    b.u64_at(0)
}
