//! The disk arena: pages ↔ slots ↔ the file — the mirrors (`map`,
//! `backward`) around the arena's slot state machine, and the file
//! handle for flights. The fields answer to no one: the machine above
//! speaks in verbs only.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::rc::Rc;

use super::SlotId;
use super::arena::{Arena, LoadPage, SavePage, SlotSt, Victim};
use crate::disk::IsFilesystem;
use crate::page::{OptionPageId, Page, PageId};

/// The disk side of the cache: an [`Arena`] of slots, the mirrors tying
/// pages to slots (a resident/loading/saving slot is always tied — the
/// transitions below are the only writers, and they keep both mirrors
/// agreed), and the file handle for flights. The slot states
/// themselves are the arena's; every verb here is an arena transition
/// plus the mirror half of the same move, or a plain question passed
/// down — never a field held out.
pub(super) struct DiskArena<D: IsFilesystem> {
    arena: Arena,
    map: HashMap<PageId, SlotId>,
    backward: Vec<OptionPageId>,
    file: D::File,
}

impl<D: IsFilesystem> DiskArena<D> {
    pub(super) fn new(file: D::File, pages: Rc<[UnsafeCell<Page>]>) -> Self {
        let arena = Arena::new(pages);
        Self {
            backward: vec![OptionPageId::NONE; arena.len()],
            arena,
            map: HashMap::new(),
            file,
        }
    }

    /// The page a slot holds (a victim has one).
    pub(super) fn page_of(&self, s: SlotId) -> Option<PageId> {
        self.backward[s].get()
    }

    /// The slot a page sits in — resident, loading or saving alike.
    pub(super) fn slot_of(&self, id: PageId) -> Option<SlotId> {
        self.map.get(&id).copied()
    }

    /// Does the page need a save to be clean on disk: a dirty resident
    /// or an in-flight save (the rest is already there).
    pub(super) fn needs_save(&self, id: PageId) -> bool {
        self.map.get(&id).is_some_and(|&s| {
            matches!(
                self.arena.state(s),
                SlotSt::Resident { dirty: true, .. } | SlotSt::Saving { .. }
            )
        })
    }

    /// Pages on slots whose state matches — the checkpoint's sweep
    /// (dirty residents, flights in the air).
    pub(super) fn pages_where(&self, f: impl Fn(SlotSt) -> bool) -> Vec<PageId> {
        (0..self.backward.len())
            .filter(|&s| f(self.arena.state(s)))
            .filter_map(|s| self.backward[s].get())
            .collect()
    }

    // ## Arena service — the machine's only window in

    /// A slot's state, a pure question.
    pub(super) fn state(&self, s: SlotId) -> SlotSt {
        self.arena.state(s)
    }

    /// The clock's touch — a served page is young again.
    pub(super) fn touch(&mut self, s: SlotId) {
        self.arena.touch(s)
    }

    /// A resident page's bytes, for writing.
    pub(super) fn page_mut(&mut self, s: SlotId) -> &mut Page {
        self.arena.page_mut(s)
    }

    /// A resident page was written to — dirty now.
    pub(super) fn dirtied(&mut self, s: SlotId) {
        self.arena.dirtied(s)
    }

    /// The clock hand's next verdict.
    pub(super) fn find_victim(&mut self) -> Option<Victim> {
        self.arena.find_victim()
    }

    /// The file handle — for the wings' takeoff and the store's own
    /// syncs; the disk itself stays here.
    pub(super) fn file(&self) -> D::File {
        self.file.clone()
    }

    // ## Mirrors

    /// Tie a slot to a page — both mirrors agree.
    fn tie(&mut self, s: SlotId, id: PageId) {
        self.map.insert(id, s);
        self.backward[s] = OptionPageId::some(id);
    }

    /// Untie a slot from its page — both mirrors agree.
    fn untie(&mut self, s: SlotId, id: PageId) {
        self.map.remove(&id);
        self.backward[s] = OptionPageId::NONE;
    }

    // ## Slot transitions (the state half is the arena's verb)

    /// Untie a clean victim: mirrors agreed, slot free. Only ever called
    /// on a tied slot.
    // TODO: Внезапно начинает хотеться Typestates.
    // ANSWER: the runtime half is in place — every arena verb is now a
    // checked edge, so the mirrors sit on a closed machine (wrong order
    // panics, never corrupts). The compile-time half is exactly the
    // leases: Loading/Saving states already move as LoadPage/SavePage
    // tokens; the remaining states (Free/Resident) must stay observable
    // to the clock hand and the scans, which is what a linear typestate
    // cannot be.
    pub(super) fn unmap(&mut self, s: SlotId) {
        if let Some(id) = self.backward[s].take() {
            self.map.remove(&id);
            self.arena.free(s);
        } else {
            panic!("Slot not mapped");
        }
    }

    /// Claim a slot for a load: reserved (loading — not a victim),
    /// mirrors tied, the write lease leaves with the flight.
    pub(super) fn load_start(&mut self, s: SlotId, id: PageId) -> LoadPage {
        let p = self.arena.start_load(s);
        self.tie(s, id);
        p
    }

    /// Load failure: the slot untied — mirrors agreed, slot free; the
    /// lease ends.
    pub(super) fn load_fail(&mut self, s: SlotId, id: PageId, p: LoadPage) {
        self.arena.load_fail(s, p);
        self.untie(s, id);
    }

    /// Settle a landed load: tied to `id`, resident-clean, touched.
    pub(super) fn reclaim(&mut self, s: SlotId, id: PageId, p: LoadPage) {
        self.arena.load_reclaim(s, p);
        self.tie(s, id);
    }

    /// Settle an initialized page into a claimed slot: resident-dirty
    /// (the init wrote it), touched, mirrors tied.
    pub(super) fn bind(&mut self, s: SlotId, id: PageId) {
        self.arena.bind(s);
        self.tie(s, id);
    }

    /// Condemn a dirty slot to a save: the state flips Saving, the read
    /// lease leaves with the flight (the mirrors already agree).
    pub(super) fn start_save(&mut self, s: SlotId) -> SavePage {
        self.arena.start_save(s)
    }

    /// A settled save: resident-clean, the demoted `used` kept for the
    /// next victim search.
    pub(super) fn save_settle(&mut self, s: SlotId, p: SavePage) {
        self.arena.save_settle(s, p)
    }

    /// A failed save: the dirt is back — resident-dirty again.
    pub(super) fn save_fail(&mut self, s: SlotId, p: SavePage) {
        self.arena.save_fail(s, p)
    }
}
