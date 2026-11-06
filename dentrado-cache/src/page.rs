//! The page and its byte primitives — the substrate of every codec.
//! All integers are little-endian; a page is always exactly
//! [`PAGE_SIZE`] bytes. Reads past the page edge give zeros (like the
//! model's `byteAt`): lengths from disk are clamped by callers, noise
//! does not inflate parsing. The store's own page kinds' codecs live
//! with their machines — the tree node in `core::tree`, the
//! superblock in `core::sb`, the freelist's chunk in
//! `core::freelist::chunk`; a record's pages are the owner's bytes —
//! the store parses them only through its schema.

/// The file page size.
pub const PAGE_SIZE: usize = 4096;
/// Torn-write byte: an underwritten page is entirely noise made of it.
pub const TORN_BYTE: u8 = 0b10101010;

pub type PageId = u64;

/// A `PageId` with a sentinel instead of `Option`: `u64` has no niche,
/// `Option<PageId>` would cost 16 bytes per mirror slot;
/// [`OptionPageId::NONE`] (`u64::MAX`) plays "no page" — 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct OptionPageId(PageId);

impl OptionPageId {
    pub const NONE: Self = Self(u64::MAX);

    /// Wrap an honest id (`u64::MAX` cannot be an id — the page world is
    /// far smaller).
    pub fn some(id: PageId) -> Self {
        debug_assert!(id != u64::MAX);
        Self(id)
    }

    pub fn is_some(self) -> bool {
        self != Self::NONE
    }

    /// Unwrap into an `Option` — for matches and API bounds.
    pub fn get(self) -> Option<PageId> {
        if self.is_some() { Some(self.0) } else { None }
    }

    /// Take and empty, like `Option::take`.
    pub fn take(&mut self) -> Option<PageId> {
        std::mem::replace(self, Self::NONE).get()
    }
}

/// A page is exactly [`PAGE_SIZE`] bytes, page-aligned: a wrong size is
/// unrepresentable by the type, and the alignment makes an arena slot a
/// ready buffer for positional io (DMA, O_DIRECT). At runtime pages live
/// only in the pool arena ([`crate::blockcache`]); codecs write into the slot
/// in place, `zeroed` is the only constructor (arena, tests).
#[repr(align(4096))]
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlignedPage(pub [u8; PAGE_SIZE]);

impl AlignedPage {
    // TODO: Page должны существовать исключительно внутри Pool. Факт присутствования этого конструктора здесь сильно пугает.
    pub fn zeroed() -> Self {
        Self([0; PAGE_SIZE])
    }

    /// The byte at an index; past the page edge — zero.
    pub fn byte_at(&self, i: usize) -> u8 {
        self.0.get(i).copied().unwrap_or(0)
    }

    /// Write a little-endian u32 at an index; past the edge — nothing
    /// (the mirror of [`Page::byte_at`]).
    pub fn put_u32_at(&mut self, i: usize, x: u32) {
        for (k, b) in x.to_le_bytes().into_iter().enumerate() {
            if let Some(v) = self.0.get_mut(i + k) {
                *v = b;
            }
        }
    }

    /// Write a little-endian u64 at an index; past the edge — nothing.
    pub fn put_u64_at(&mut self, i: usize, x: u64) {
        self.put_u32_at(i, x as u32);
        self.put_u32_at(i + 4, (x >> 32) as u32);
    }

    pub fn u32_at(&self, i: usize) -> u32 {
        let mut x = 0u32;
        for k in 0..4 {
            x |= (self.byte_at(i + k) as u32) << (8 * k);
        }
        x
    }

    pub fn u64_at(&self, i: usize) -> u64 {
        let mut x = 0u64;
        for k in 0..8 {
            x |= (self.byte_at(i + k) as u64) << (8 * k);
        }
        x
    }

    /// Bytes `[i, i+n)` clamped to the page — the model's `sliceAt`:
    /// there is nothing to read past the page (empty).
    pub fn slice_at(&self, i: usize, n: usize) -> &[u8] {
        let i = i.min(PAGE_SIZE);
        &self.0[i..(i + n).min(PAGE_SIZE)]
    }
}

/// An append cursor into a page: the codecs write straight into the
/// bytes — no staging buffer; past the edge nothing sticks (like
/// [`Page::put_u32_at`]), and the unwritten tail zeroes at [`Cur::fin`].
pub(crate) struct Cur<'a> {
    p: &'a mut AlignedPage,
    i: usize,
}

impl<'a> Cur<'a> {
    /// Start at the page's beginning.
    pub(crate) fn at(p: &'a mut AlignedPage) -> Self {
        Self { p, i: 0 }
    }

    pub(crate) fn u32(&mut self, x: u32) {
        self.p.put_u32_at(self.i, x);
        self.i = (self.i + 4).min(PAGE_SIZE);
    }

    pub(crate) fn u64(&mut self, x: u64) {
        self.p.put_u64_at(self.i, x);
        self.i = (self.i + 8).min(PAGE_SIZE);
    }

    /// Raw bytes, cut at the page's edge.
    pub(crate) fn raw(&mut self, b: &[u8]) {
        let n = (PAGE_SIZE - self.i).min(b.len());
        self.p.0[self.i..self.i + n].copy_from_slice(&b[..n]);
        self.i = (self.i + b.len()).min(PAGE_SIZE);
    }

    /// Done: the unwritten tail zeroes.
    pub(crate) fn fin(self) {
        self.p.0[self.i..].fill(0);
    }
}

const FNV_INIT: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001B3;

/// An FNV-1a-like sum over chain cells (as in the model: `h*prime + x`).
pub fn fnv_id(ids: &[PageId]) -> u64 {
    ids.iter()
        .fold(FNV_INIT, |h, &x| h.wrapping_mul(FNV_PRIME).wrapping_add(x))
}
