//! The arena: slots of page bytes with a clock hand — which slot to
//! reuse next. It knows pages exist; it does not know they map to a
//! disk (the mirrors and the file live one layer up, in `disk_arena`).
//! The slot state machine lives here too: every transition is an
//! [`Arena`] verb, so the bytes and their in-flight leases
//! ([`LoadPage`]/[`SavePage`]) cannot disagree.

use std::cell::UnsafeCell;
use std::mem::MaybeUninit;
use std::rc::Rc;

use compio::buf::{IoBuf, IoBufMut, SetLen};

use super::SlotId;
use crate::page::{AlignedPage, PAGE_SIZE};

// TODO: В будущем стоило бы посмотреть на bitmap (маловероятно) или хотя бы сжать до байта гарантирванно, но это потом.
/// Slot state: free / under a loading flight (the kernel writes the
/// bytes — exclusive) / under a saving flight (the kernel reads — the
/// sole reader: everyone else queues behind the flight, keeping the
/// issue order; dirty, the dirt returns on failure) / resident —
/// ready for synchronous service, with touched and dirty bits.
#[derive(Clone, Copy)]
pub(super) enum SlotSt {
    Free,
    Loading,
    Saving { used: bool },
    Resident { used: bool, dirty: bool },
}

/// A slot the clock hand deemed reusable.
pub(super) enum Victim {
    Free(SlotId),
    Clean(SlotId),
    Dirty(SlotId),
}

/// The arena: slots of page bytes, never grown, and the clock hand
/// over them.
///
/// The pages sit in one shared slice: a flight's lease holds the slice
/// (an `Rc`) and a reference into exactly its page — long after any
/// store section, deep inside the compio driver. The slice is born at
/// the store's construction ([`crate::persist::PersistentCore::fresh`])
/// — the sharing
/// is the store's architecture, not an arena detail. The slot state is
/// the exclusivity flag: every access to the bytes asserts the state
/// it needs — a discriminant read, the flags the states already carry,
/// not a byte of padding beside each 4096-aligned page.
pub(super) struct Arena {
    pages: Rc<[UnsafeCell<AlignedPage>]>,
    slots: Vec<SlotSt>,
    clock: usize,
}

impl Arena {
    /// The arena takes its pages as given — born at the store's top,
    /// shared with flights (see the struct docs). The slot count is the
    /// page count.
    pub(super) fn new(pages: Rc<[UnsafeCell<AlignedPage>]>) -> Self {
        Self {
            slots: (0..pages.len()).map(|_| SlotSt::Free).collect(),
            pages,
            clock: 0,
        }
    }

    /// The slot's state, by value — a `Copy` read, so a match on it
    /// leaves the caller free to mutate right in the arms.
    pub(super) fn state(&self, s: SlotId) -> SlotSt {
        self.slots[s]
    }

    /// The slot count (never grows).
    pub(super) fn len(&self) -> usize {
        self.slots.len()
    }

    /// A slot's page, exclusive — resident pages only: saving bytes are
    /// being read by the kernel, loading ones written. The `&mut`
    /// borrows the whole arena, so it cannot live past the next arena
    /// call — there is no holding it across a transition.
    #[allow(unsafe_code)]
    pub(super) fn page_mut(&mut self, s: SlotId) -> &mut AlignedPage {
        assert!(
            matches!(self.slots[s], SlotSt::Resident { .. }),
            "write of a non-resident slot"
        );
        // SAFETY: the asserted state vouches for exclusivity, and the
        // borrow of the whole arena carries it.
        unsafe { &mut *self.pages[s].get() }
    }

    // ## The clock hand

    // TODO: Охренеть как медленно. Желательно в будущем реконструировать весь механизм, чтобы оно быстро так пробегало
    // по нескольким bitmap'ам, и работало "окей" с SIMD, а не вот это вот всё.
    // Условно, можно было бы свести всё к трём bitmap: `free`, `busy`, `demoteable`. Но это работа на будущее.
    // Отделить хранение статуса ячейки от хранения данных под clock.
    /// One step of the hand: it stops at a victim, or only demotes, or
    /// passes by. An empty arena always passes (the wish will wait).
    fn clock_step(&mut self) -> (Option<Victim>, bool) {
        let n = self.slots.len();
        if n == 0 {
            return (None, false);
        }
        let s = self.clock % n;
        self.clock = (self.clock + 1) % n;
        let (used, dirty) = match &self.slots[s] {
            SlotSt::Free => return (Some(Victim::Free(s)), false),
            SlotSt::Resident { used, dirty } => (*used, *dirty),
            _ => return (None, false),
        };
        if !used {
            (
                Some(if dirty {
                    Victim::Dirty(s)
                } else {
                    Victim::Clean(s)
                }),
                false,
            )
        } else {
            self.slots[s] = SlotSt::Resident { used: false, dirty };
            (None, true)
        }
    }

    /// A full turn of the hand: at most `steps` steps, early exit at a
    /// victim.
    fn sweep(&mut self, steps: usize) -> (Option<Victim>, bool) {
        let mut demoted = false;
        for _ in 0..steps {
            let (v, dem) = self.clock_step();
            demoted |= dem;
            if let Some(v) = v {
                return (Some(v), demoted);
            }
        }
        (None, demoted)
    }

    /// Victim search: turns repeat while anything was demoted — hot pages
    /// survive a full turn. `None` ⇔ every slot is in flight or the arena
    /// is empty.
    pub(super) fn find_victim(&mut self) -> Option<Victim> {
        for _ in 0..self.slots.len() + 1 {
            let (v, dem) = self.sweep(self.slots.len());
            if let Some(v) = v {
                return Some(v);
            }
            if !dem {
                return None;
            }
        }
        None
    }

    // ## Slot transitions
    //
    // The only writers of `slots`, and a closed state machine: every
    // verb asserts the edge it implements (its FROM-state), so any call
    // sequence either walks real edges or panics — there is no blind
    // write, release builds included. The edges also carry the lease
    // accounting: `start_load`/`start_save` are the only minters of a
    // lease, the landings (`load_reclaim`/`load_fail`,
    // `save_settle`/`save_fail`) its only burners — each consumes it,
    // and no edge leaves
    // Loading/Saving without burning one. Hence, by induction over
    // calls: a slot in flight has exactly one live lease of its kind, a
    // live lease's slot is in flight, and nothing but its landing can
    // fire on it — the lease's reference is unique by construction,
    // not by trust.
    //
    // The mirrors (id↔slot) are tied one layer up, around these verbs.

    /// Claim a free slot for a load: reserved (loading — not a victim),
    /// the write lease leaves with the flight.
    pub(super) fn start_load(&mut self, s: SlotId) -> LoadPage {
        assert!(
            matches!(self.slots[s], SlotSt::Free),
            "load into an unfree slot"
        );
        self.slots[s] = SlotSt::Loading;
        LoadPage::take(&self.pages, s)
    }

    /// Send a resident slot into a save: the read lease leaves with the
    /// flight, the dirt travels with the bytes.
    pub(super) fn start_save(&mut self, s: SlotId) -> SavePage {
        if let SlotSt::Resident { used, .. } = self.slots[s] {
            self.slots[s] = SlotSt::Saving { used };
        } else {
            panic!("Transferred non-Resident slot to Saving");
        }
        SavePage::take(&self.pages, s)
    }

    /// Load failure: the slot is free again, the lease ends with it.
    pub(super) fn load_fail(&mut self, s: SlotId, p: LoadPage) {
        debug_assert_eq!(p.s, s);
        assert!(
            matches!(self.slots[s], SlotSt::Loading),
            "load failed on an unloading slot"
        );
        self.slots[s] = SlotSt::Free;
    }

    /// Settle a landed load: resident-clean, touched — and the write
    /// lease is back (consumed here; only a landing can return it).
    pub(super) fn load_reclaim(&mut self, s: SlotId, p: LoadPage) {
        debug_assert_eq!(p.s, s);
        assert!(
            matches!(self.slots[s], SlotSt::Loading),
            "load landed on an unloading slot"
        );
        self.slots[s] = SlotSt::Resident {
            used: true,
            dirty: false,
        };
    }

    /// Settle a landed save: resident-clean, keeping the demoted `used`
    /// bit (a kicked victim settles ready for the sweep).
    pub(super) fn save_settle(&mut self, s: SlotId, p: SavePage) {
        debug_assert_eq!(p.s, s);
        if let SlotSt::Saving { used } = self.slots[s] {
            self.slots[s] = SlotSt::Resident { used, dirty: false };
        } else {
            panic!("A save settled on a non-saving slot");
        }
    }

    /// Save failure: the dirt returned (the bytes are intact — losing
    /// them is not an option).
    pub(super) fn save_fail(&mut self, s: SlotId, p: SavePage) {
        debug_assert_eq!(p.s, s);
        if let SlotSt::Saving { used } = self.slots[s] {
            self.slots[s] = SlotSt::Resident { used, dirty: true };
        } else {
            panic!("Slot not saving");
        }
    }

    /// Settle an initialized page into a claimed slot: touched-and-dirty
    /// — a free slot taken by the room search, or the page's own
    /// resident slot overwritten in place. Never a slot in flight: its
    /// bytes belong to a live lease.
    pub(super) fn bind(&mut self, s: SlotId) {
        assert!(
            matches!(self.slots[s], SlotSt::Free | SlotSt::Resident { .. }),
            "bind onto a slot in flight"
        );
        self.slots[s] = SlotSt::Resident {
            used: true,
            dirty: true,
        };
    }

    /// A write went through in place: touched-and-dirty.
    pub(super) fn dirtied(&mut self, s: SlotId) {
        assert!(
            matches!(self.slots[s], SlotSt::Resident { .. }),
            "dirt of a non-resident slot"
        );
        self.slots[s] = SlotSt::Resident {
            used: true,
            dirty: true,
        };
    }

    /// A read went through: the `used` bit rises, the rest as it was.
    pub(super) fn touch(&mut self, s: SlotId) {
        if let SlotSt::Resident { dirty, .. } = self.slots[s] {
            self.slots[s] = SlotSt::Resident { used: true, dirty };
        } else {
            panic!("touch of a non-resident slot");
        }
    }

    /// A clean victim leaves: the slot is free. Only a resident slot
    /// can leave — one in flight is not anyone's to free.
    pub(super) fn free(&mut self, s: SlotId) {
        assert!(
            matches!(self.slots[s], SlotSt::Resident { .. }),
            "freed a slot in flight"
        );
        self.slots[s] = SlotSt::Free;
    }
}

// ## A slot as an io buffer

/// The unsafe core of the pool, used twice: a lease over a page's
/// bytes with its lifetime erased — `&'static mut` for a load (the
/// kernel writes), `&'static` for a save (it reads). Sound on three
/// counts, each cheap: the handle owns `_keep`, an `Rc` to the very
/// cells the reference points into, so they outlive it; uniqueness is
/// vouched for by the slot state — the lease is born as the slot turns
/// Loading/Saving, dies in the landing verb that consumes it, and the
/// transition surface is a closed machine (see the transitions
/// section): while the lease lives, no verb but its own landing can
/// even fire on the slot; and the bytes never move (fixed slice,
/// `UnsafeCell` in place). Exactly the compio buffer contract — a
/// buffer valid and unaliased for the whole operation it belongs to.

/// A load's lease of a slot: the kernel writes the bytes — exclusive,
/// which the Loading state vouches for while the lease lives.
pub(super) struct LoadPage {
    s: SlotId,
    page: &'static mut AlignedPage,
    _keep: Rc<[UnsafeCell<AlignedPage>]>,
}

/// A save's lease of a slot: the kernel reads the bytes — the sole
/// window on them while the lease lives (the Saving state keeps every
/// other reader and all writers out; user reads queue behind the
/// flight, so read-write causality holds through a save).
pub(super) struct SavePage {
    s: SlotId,
    page: &'static AlignedPage,
    _keep: Rc<[UnsafeCell<AlignedPage>]>,
}

impl LoadPage {
    /// Take the write lease of a slot just turned Loading.
    #[allow(unsafe_code)]
    fn take(cells: &Rc<[UnsafeCell<AlignedPage>]>, s: SlotId) -> Self {
        Self {
            s,
            // SAFETY: unique for the whole flight — the slot is Loading
            // and stays so until the landing consumes this lease; the
            // cells outlive the handle through `_keep`.
            page: unsafe { &mut *cells[s].get() },
            _keep: Rc::clone(cells),
        }
    }
}

impl SavePage {
    /// Take the read lease of a slot just turned Saving.
    #[allow(unsafe_code)]
    fn take(cells: &Rc<[UnsafeCell<AlignedPage>]>, s: SlotId) -> Self {
        Self {
            s,
            // SAFETY: the Saving state keeps every writer out while the
            // lease lives; the cells outlive the handle through `_keep`.
            page: unsafe { &*cells[s].get() },
            _keep: Rc::clone(cells),
        }
    }
}

/// The save's window: the page whole — the payload of the flight.
impl IoBuf for SavePage {
    fn as_init(&self) -> &[u8] {
        &self.page.0
    }
}

/// The load's window: the page whole (`IoBufMut` demands `IoBuf`).
impl IoBuf for LoadPage {
    fn as_init(&self) -> &[u8] {
        &self.page.0
    }
}

/// The length is fixed — the slot is given to the page whole.
#[allow(unsafe_code)] // an unsafe fn without unsafe operations inside
impl SetLen for LoadPage {
    unsafe fn set_len(&mut self, len: usize) {
        debug_assert!(len <= PAGE_SIZE);
    }
}

#[allow(unsafe_code)]
impl IoBufMut for LoadPage {
    fn as_uninit(&mut self) -> &mut [MaybeUninit<u8>] {
        self.page.0.as_uninit()
    }
}
