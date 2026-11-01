//! Epoch-based reclamation — the runtime half of page death: the path
//! for pages removed by `put_cache` (the disk half is the flush, which
//! takes the accumulated deaths with its pool).

use crate::page::PageId;

/// An EBR bucket: the epoch's subscriptions (live readers that arrived
/// while the bucket was current) and that epoch's deaths.
struct EbrSlot {
    pins: usize,
    pending: Vec<PageId>,
}

/// EBR over three buckets: readers subscribe to the current bucket,
/// deaths are registered into it too; after a registration and after a
/// guard leaves, the hand walks the empty buckets, taking the deaths of
/// the emptied ones: by the counter there are no readers of their epoch
/// left, and the readers of the other buckets all arrived after their
/// deaths. A long reader slows the hand for a turn — deaths pile up in
/// the current bucket: latency, not safety.
///
/// The placement discipline is mirrored: a guard lands with the hand at
/// its bucket — blocking the minimum — while a batch of deaths lands
/// behind the hand, already on its re-entry path.
///
/// A deliberate departure from the model: it keeps a list of live epochs
/// with unique stamps and drains by exact comparison; here — the classic
/// EBR counter buckets, no lists, no scans; a fresh batch waits for an
/// empty turn, the safety is the same.
struct Ebr {
    cur: usize,
    slots: [EbrSlot; 3],
}

impl Ebr {
    fn fresh() -> Self {
        Self {
            cur: 0,
            slots: std::array::from_fn(|_| EbrSlot {
                pins: 0,
                pending: Vec::new(),
            }),
        }
    }

    /// Advance the hand while the next bucket is empty (at most a turn);
    /// the emptied buckets' deaths go out. A full turn happens only when
    /// there are no guards at all — then the fresh batch drains too.
    fn advance(&mut self) -> Vec<PageId> {
        let mut d = Vec::new();
        for _ in 0..3 {
            let n = (self.cur + 1) % 3;
            if self.slots[n].pins != 0 {
                break;
            }
            self.cur = n;
            d.append(&mut self.slots[n].pending);
        }
        d
    }

    /// A reader's pin — the bucket is the guard's handle. No walk: the
    /// other entries leave the hand with nothing left to take, and a
    /// subscription can only add a blocker.
    fn pin(&mut self) -> usize {
        self.slots[self.cur].pins += 1;
        self.cur
    }

    /// The guard is gone — the subscription goes too; the hand walks the
    /// emptied buckets.
    fn drop_guard(&mut self, b: usize) -> Vec<PageId> {
        self.slots[b].pins -= 1;
        self.advance()
    }

    /// Register deaths: into the current bucket first, then the hand
    /// walks — the batch lands behind it, on the re-entry path; with no
    /// guards around, the full turn reaps the fresh batch here and now.
    fn reg(&mut self, ps: &[PageId]) -> Vec<PageId> {
        self.slots[self.cur].pending.extend_from_slice(ps);
        self.advance()
    }
}

/// Deaths: the EBR machine plus the pile it drains into — every
/// advance's take lands here; the flush seals the whole pile with its
/// pool.
pub(super) struct Deaths {
    ebr: Ebr,
    dying: Vec<PageId>,
}

impl Deaths {
    pub(super) fn fresh() -> Self {
        Self {
            ebr: Ebr::fresh(),
            dying: Vec::new(),
        }
    }

    /// The pile out — the flush's seal.
    pub(super) fn take(&mut self) -> Vec<PageId> {
        std::mem::take(&mut self.dying)
    }

    /// The undrained pendings' pages, copied (the flush's seal packs
    /// them as limbo cells — a crash must not lose them; the buckets
    /// keep them until the hand drains them into the pile).
    pub(super) fn pendings(&self) -> Vec<PageId> {
        self.ebr
            .slots
            .iter()
            .flat_map(|s| s.pending.iter().copied())
            .collect()
    }

    pub(super) fn len(&self) -> usize {
        self.dying.len()
    }

    /// The pages the machine still holds — the pile plus every bucket's
    /// undrained pending (the partition's RAM half).
    pub(super) fn held(&self) -> Vec<PageId> {
        let mut v = self.dying.clone();
        v.extend(
            self.ebr
                .slots
                .iter()
                .flat_map(|s| s.pending.iter().copied()),
        );
        v
    }

    /// A reader's pin — the bucket index is the guard's handle.
    pub(super) fn pin(&mut self) -> usize {
        self.ebr.pin()
    }

    /// The guard is gone.
    pub(super) fn unpin(&mut self, b: usize) {
        let d = self.ebr.drop_guard(b);
        self.dying.extend(d);
    }

    /// Register deaths.
    pub(super) fn reg(&mut self, ps: &[PageId]) {
        let d = self.ebr.reg(ps);
        self.dying.extend(d);
    }
}
