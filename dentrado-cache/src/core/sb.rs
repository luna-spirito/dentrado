//! The superblock: magic, version, generation, the pools'
//! `free_next` counters, the tables' roots, the even/odd chain
//! entries, cell counts and sums. Two SB slots (file pages 0 and 1),
//! alternating per generation; recovery picks the newest valid one.
//!
//! A table is identified by opaque bytes (a length byte — the format
//! bounds an id to [`MAX_TABLE_ID`] bytes, so the byte is one — then
//! the raw identity) followed by the tree root. The bytes are built by
//! the caller (postcard of a name and version); the store never
//! decodes them, only compares. The set is identified by bytes, not
//! positions: `open` unions the image's set with the opener's, so a
//! changed table set never bricks the file — the image's extras stay
//! enumerable and droppable, the opener's new ones start empty.

use super::freelist::ChainEntry;
use crate::page::{Cur, PAGE_SIZE, Page, PageId};

/// Superblock magic (`"DNTR"` little-endian).
pub const SB_MAGIC: u32 = 0x52544E44;
/// Image format version.
pub const SB_VERSION: u32 = 4;
/// A table identity's byte bound — the length byte stays one byte.
pub const MAX_TABLE_ID: usize = 63;

/// The parse's table-count bound: the fixed tail (two chain entries,
/// four counters — 56 bytes) starts right after the worst-case tables —
/// junk cannot inflate the parse.
pub const MAX_TABLES: usize = (PAGE_SIZE - 40 - 56) / (1 + MAX_TABLE_ID + 8);

/// A parsed SB: `next0`/`next1` — the even/odd pools' never-issued
/// counters (even/odd by parity), `tables` — the tables' tree roots
/// (0 — an empty table), `e0`/`e1` — the pools' chain entries,
/// `c0`/`c1` — the chains' cell counts, `s0`/`s1` — the chains' cell
/// sums.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Sb {
    pub r#gen: u64,
    pub next0: PageId,
    pub next1: PageId,
    pub tables: Vec<(Vec<u8>, PageId)>,
    pub e0: ChainEntry,
    pub e1: ChainEntry,
    pub c0: u64,
    pub c1: u64,
    pub s0: u64,
    pub s1: u64,
}

/// Write an SB page: `tables` — the tables' roots, `next0`/`next1`
/// — the pools' never-issued counters, `even`/`odd` — the pools' chain
/// entries, `even_cells`/`odd_cells` — the chains' cell counts,
/// `even_sum`/`odd_sum` — the chains' cell sums. More tables than the
/// page holds, or an id past its bound, is the caller's set outgrowing
/// the format — refused loudly, never truncated.
pub fn write_sb(
    p: &mut Page,
    r#gen: u64,
    next0: PageId,
    next1: PageId,
    tables: &[(Vec<u8>, PageId)],
    even: ChainEntry,
    odd: ChainEntry,
    even_cells: u64,
    odd_cells: u64,
    even_sum: u64,
    odd_sum: u64,
) {
    assert!(
        tables.len() <= MAX_TABLES,
        "sb: the table set overgrew the page"
    );
    let mut c = Cur::at(p);
    c.u32(SB_MAGIC);
    c.u32(SB_VERSION);
    c.u64(r#gen);
    c.u64(next0);
    c.u64(next1);
    c.u32(tables.len() as u32);
    c.u32(0);
    for (id, root) in tables {
        assert!(
            !id.is_empty() && id.len() <= MAX_TABLE_ID,
            "sb: an id outside its bound"
        );
        c.raw(&[id.len() as u8]);
        c.raw(id);
        c.u64(*root);
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

/// Parse an SB page; `None` — not an SB (garbage, torn noise). A table
/// entry whose id falls outside the format (empty, past its bound —
/// torn noise) is skipped without stopping the walk: it cannot match
/// any opener's id, so it is invisible either way. A dead entry page
/// (`0` — no chain) with a stray offset is junk normalized to the
/// empty entry.
pub fn parse_sb(b: &Page) -> Option<Sb> {
    (b.u32_at(0) == SB_MAGIC && b.u32_at(4) == SB_VERSION).then(|| {
        let n = (b.u32_at(32) as usize).min(MAX_TABLES);
        let mut at = 40usize;
        let mut tables = Vec::with_capacity(n);
        for _ in 0..n {
            let len = b.byte_at(at) as usize;
            let id = b.slice_at(at + 1, len).to_vec();
            let root = b.u64_at(at + 1 + len);
            at += 1 + len + 8;
            if !id.is_empty() && id.len() <= MAX_TABLE_ID {
                tables.push((id, root));
            }
        }
        Sb {
            r#gen: b.u64_at(8),
            next0: b.u64_at(16),
            next1: b.u64_at(24),
            tables,
            e0: ChainEntry::at(b.u64_at(at), b.u32_at(at + 8)),
            e1: ChainEntry::at(b.u64_at(at + 12), b.u32_at(at + 20)),
            c0: b.u64_at(at + 24),
            c1: b.u64_at(at + 32),
            s0: b.u64_at(at + 40),
            s1: b.u64_at(at + 48),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page written and parsed comes back whole — entries, tables
    /// and counters alike.
    #[test]
    fn sb_round_trip() {
        let sb = Sb {
            r#gen: 3,
            next0: 10,
            next1: 11,
            tables: vec![(b"a".to_vec(), 7), (b"cache:Repo:v3".to_vec(), 0)],
            e0: ChainEntry { page: 4, off: 3 },
            e1: ChainEntry::full(5),
            c0: 1,
            c1: 2,
            s0: 3,
            s1: 4,
        };
        let mut p = Page::zeroed();
        write_sb(
            &mut p, sb.r#gen, sb.next0, sb.next1, &sb.tables, sb.e0, sb.e1, sb.c0, sb.c1, sb.s0,
            sb.s1,
        );
        assert_eq!(parse_sb(&p), Some(sb));
    }

    /// Torn tail noise — a length byte past any id — is skipped, not
    /// a refusal: the tables that parsed stand.
    #[test]
    fn torn_ids_are_skipped() {
        let mut p = Page::zeroed();
        write_sb(
            &mut p,
            1,
            4,
            5,
            &[(b"ok".to_vec(), 9)],
            ChainEntry::default(),
            ChainEntry::default(),
            0,
            0,
            0,
            0,
        );
        // Junk the tail past the single table's entry — the header,
        // the count and the first table stay readable.
        let junk_from = 40 + (1 + 2 + 8);
        p.0[junk_from..].fill(0xAA);
        let sb = parse_sb(&p).unwrap();
        assert_eq!(sb.tables, vec![(b"ok".to_vec(), 9)]);
    }

    /// A dead entry page (`0` — no chain) with a stray offset is junk:
    /// the parse normalizes it to the empty entry (a continuation
    /// behind a null pointer cannot exist).
    #[test]
    fn a_dead_entry_drops_its_junk_offset() {
        let mut p = Page::zeroed();
        write_sb(
            &mut p,
            1,
            4,
            5,
            &[],
            ChainEntry { page: 0, off: 9 },
            ChainEntry::default(),
            0,
            0,
            0,
            0,
        );
        assert_eq!(parse_sb(&p).unwrap().e0, ChainEntry::default());
    }

    /// Garbage is not an SB — a zeroed page, a wrong version.
    #[test]
    fn garbage_is_not_an_sb() {
        assert_eq!(parse_sb(&Page::zeroed()), None);
        let mut p = Page::zeroed();
        let mut c = Cur::at(&mut p);
        c.u32(SB_MAGIC);
        c.u32(SB_VERSION + 1);
        c.fin();
        assert_eq!(parse_sb(&p), None);
    }
}
