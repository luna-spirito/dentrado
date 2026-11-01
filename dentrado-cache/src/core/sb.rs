//! The superblock: magic, version, generation, the pools'
//! `free_next` counters, the tables' roots, the even/odd chain entries,
//! cell counts and sums. Two SB slots (file pages 0 and 1), alternating
//! per generation; recovery picks the newest valid one.

use super::freelist::ChainEntry;
use crate::page::{Cur, PAGE_SIZE, Page, PageId};

/// Superblock magic (`"DNTR"` little-endian).
pub const SB_MAGIC: u32 = 0x52544E44;
/// Image format version.
pub const SB_VERSION: u32 = 3;

/// The parse's root-count bound: the fixed tail (two chain entries,
/// four counters — 56 bytes) starts right after the roots — junk
/// cannot inflate the parse.
pub const MAX_ROOTS: usize = (PAGE_SIZE - 40 - 56) / 8;

/// A parsed SB: `next0`/`next1` — the even/odd pools' never-issued
/// counters (even/odd by parity), `roots` — the tables' tree roots (0 —
/// an empty table), `e0`/`e1` — the pools' chain entries, `c0`/`c1` —
/// the chains' cell counts, `s0`/`s1` — the chains' cell sums.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sb {
    pub r#gen: u64,
    pub next0: PageId,
    pub next1: PageId,
    pub roots: Vec<PageId>,
    pub e0: ChainEntry,
    pub e1: ChainEntry,
    pub c0: u64,
    pub c1: u64,
    pub s0: u64,
    pub s1: u64,
}

/// Write an SB page: `roots` — the tables' tree roots, `next0`/`next1`
/// — the pools' never-issued counters, `even`/`odd` — the pools' chain
/// entries, `even_cells`/`odd_cells` — the chains' cell counts,
/// `even_sum`/`odd_sum` — the chains' cell sums. More roots than the
/// page holds is the caller's fixed table set outgrowing the format —
/// refused loudly, never truncated.
pub fn write_sb(
    p: &mut Page,
    r#gen: u64,
    next0: PageId,
    next1: PageId,
    roots: &[PageId],
    even: ChainEntry,
    odd: ChainEntry,
    even_cells: u64,
    odd_cells: u64,
    even_sum: u64,
    odd_sum: u64,
) {
    assert!(
        roots.len() <= MAX_ROOTS,
        "sb: the table set overgrew the page"
    );
    let mut c = Cur::at(p);
    c.u32(SB_MAGIC);
    c.u32(SB_VERSION);
    c.u64(r#gen);
    c.u64(next0);
    c.u64(next1);
    c.u32(roots.len() as u32);
    c.u32(0);
    for &r in roots {
        c.u64(r);
    }
    c.u64(even.page);
    c.u32(even.off);
    c.u64(odd.page);
    c.u32(odd.off);
    c.u64(even_cells);
    c.u64(odd_cells);
    c.u64(even_sum);
    c.u64(odd_sum);
    c.fin();
}

/// Parse an SB page; `None` — not an SB (garbage, torn noise).
pub fn parse_sb(b: &Page) -> Option<Sb> {
    (b.u32_at(0) == SB_MAGIC && b.u32_at(4) == SB_VERSION).then(|| {
        let n = (b.u32_at(32) as usize).min(MAX_ROOTS);
        Sb {
            r#gen: b.u64_at(8),
            next0: b.u64_at(16),
            next1: b.u64_at(24),
            roots: (0..n).map(|i| b.u64_at(40 + 8 * i)).collect(),
            e0: ChainEntry {
                page: b.u64_at(40 + 8 * n),
                off: b.u32_at(48 + 8 * n),
            },
            e1: ChainEntry {
                page: b.u64_at(52 + 8 * n),
                off: b.u32_at(60 + 8 * n),
            },
            c0: b.u64_at(64 + 8 * n),
            c1: b.u64_at(72 + 8 * n),
            s0: b.u64_at(80 + 8 * n),
            s1: b.u64_at(88 + 8 * n),
        }
    })
}
