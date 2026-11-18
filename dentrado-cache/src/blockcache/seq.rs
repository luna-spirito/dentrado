//! The machine: the pages' parked work over the disk arena — wishes
//! waiting for a page at rest, save requests waiting for dirty bytes to
//! settle, and the synchronous service of resident pages. The machine
//! answers a wish on the spot when the page is resident; anything else
//! parks on the page's state, and the driver resumes it when the world
//! (a flight, a slot) moves.
//!
//! There is no ticket order. A resident page serves reads and writes
//! directly — the section is exclusive, and under the store's ownership
//! rules one writer XOR any readers is the caller's business; only the
//! page's state (loading, saving, absent) parks work. Save requests
//! (`persist`, a checkpoint) wait for the page to become clean, and a
//! checkpoint is a sweep of the dirty residents and the saves already
//! in the air, not a positional barrier in a queue.

use std::cell::UnsafeCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use futures::channel::oneshot;

use super::arena::{LoadPage, SavePage, SlotSt, Victim};
use super::disk_arena::DiskArena;
use super::{PostFn, Reply, ReplyRx, SlotId};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{AlignedPage, PageId};

// ## The vocabulary with the driver

/// How far one `step` of a page's parked work went: quiet — nothing
/// runnable (no work, or the page is in flight); the page has a post
/// and no slot; the page holds dirt a save request waits for — the
/// driver flies it.
pub(super) enum Step {
    Quiet,
    NeedRoom,
    Save { s: SlotId },
}

/// The room search's answer: a slot to take; a kicked dirty victim —
/// its save is already armed (the driver flies it, and there is no room
/// this time); no room at all — every slot is in flight.
pub(super) enum Room {
    Slot(SlotId),
    Kicked {
        page: PageId,
        s: SlotId,
        buf: SavePage,
    },
    None,
}

/// A handed-over slot's fate: the page bound — the work may run; a
/// load armed — the driver flies the lease, and the landing runs the
/// work.
pub(super) enum Claim {
    Bound,
    Load(LoadPage),
}

/// A wish's fate: served on the spot — nothing parked, nothing to
/// drive; or parked on the page, the answer arriving by the receiver
/// when the page is at rest — `idle`: no post was parked yet, the
/// driver must start the page moving.
pub(crate) enum Submit<R> {
    Now(R),
    Queued {
        rx: oneshot::Receiver<Result<R, StoreError>>,
        idle: bool,
    },
}

// ## The machine's state

/// A page's parked work, and the disk question settled at its birth —
/// `load` says the first post wants the disk copy read when a slot
/// arrives (`give_slot`'s only business; posts joining a live entry
/// inherit its state — the page is in the system already, or the birth
/// decided the past). Posts against a resident page never park: they
/// are served in the caller's section.
#[derive(Default)]
struct Parked {
    load: bool,
    posts: VecDeque<PostFn>,
    /// Save requests waiting for the page to become clean: `persist`'s
    /// and checkpoints'. A request is answered when the page is resident
    /// and no post on it is dirty — or with the error that stopped it.
    saves: Vec<Reply>,
}

/// The sequential machine: the disk arena (mirrors, the file) and the
/// pages' parked work in one struct — the coupled invariants are its
/// private business: a page has at most one flight, and only the state
/// machine decides which one. It knows nothing of the driving: no
/// flights, no waiting line — those live one level up, in the driver.
pub(super) struct SeqArena<D: IsFilesystem> {
    /// The disk arena below — the machine's own; the layers above reach
    /// the pages only through the machine's verbs.
    disk: DiskArena<D>,
    /// Pages with parked work — flying and waiting for a slot alike.
    parked: HashMap<PageId, Parked>,
}

/// Where a page stands: resident (its slot, with the dirty bit), in
/// flight, or out of the cache.
enum Where {
    Res(SlotId, bool),
    Fly,
    Gone,
}

impl<D: IsFilesystem> SeqArena<D> {
    pub(super) fn new(file: D::File, pages: Rc<[UnsafeCell<AlignedPage>]>) -> Self {
        Self {
            disk: DiskArena::new(file, pages),
            parked: HashMap::new(),
        }
    }

    /// The file handle — for the wings' takeoff and the store's own
    /// syncs; the disk arena itself stays the machine's own.
    pub(super) fn file(&self) -> D::File {
        self.disk.file()
    }

    // ## Wishes

    /// A wish against a page: `f` runs over its bytes — `true`, the
    /// write left it dirty. Only a resident page with no parked work is
    /// served right here; anything else — a fresh page included —
    /// parks, the answer by the receiver. The room search stays the
    /// driver's: a kick arms the victim's save, and this desk has no
    /// hands to fly it.
    pub(super) fn submit<R>(
        &mut self,
        id: PageId,
        load: bool,
        f: impl FnOnce(&mut AlignedPage) -> (bool, R) + 'static,
    ) -> Submit<R>
    where
        R: 'static,
    {
        if !self.parked.contains_key(&id)
            && let Some(s) = self.disk.slot_of(id)
            && let SlotSt::Resident { .. } = self.disk.state(s)
        {
            let (dirty, r) = f(self.disk.page_mut(s));
            if dirty {
                self.disk.dirtied(s);
            } else {
                self.disk.touch(s);
            }
            return Submit::Now(r);
        }
        // no resident page to serve it (or work already parked ahead) —
        // the wish parks, behind a flight or for a slot
        let (tx, rx) = oneshot::channel();
        let post: PostFn = Box::new(move |p| match p {
            Ok(pg) => {
                let (dirty, r) = f(pg);
                let _ = tx.send(Ok(r));
                dirty
            }
            Err(e) => {
                let _ = tx.send(Err(e));
                false
            }
        });
        let q = self.parked.entry(id).or_default();
        let idle = q.posts.is_empty();
        if idle {
            q.load = load; // the birth settles the disk question
        }
        q.posts.push_back(post);
        Submit::Queued { rx, idle }
    }

    /// Does a waiting page still want a slot: its work alive and the
    /// page out of the cache.
    pub(super) fn wants_room(&self, id: PageId) -> bool {
        self.parked.get(&id).is_some_and(|q| !q.posts.is_empty()) && self.disk.slot_of(id).is_none()
    }

    // ## The motor's core

    /// Run a page's parked work as far as a resident page allows: every
    /// post runs in this section; a save request with dirt behind it
    /// stops the run for the driver. A page in flight parks everything;
    /// a page out of the cache keeps its posts and save requests waiting
    /// for a slot — the slot brings the bytes to write. With no post
    /// left, the bytes on disk are the whole story and the requests are
    /// answered.
    pub(super) fn step(&mut self, id: PageId) -> Step {
        let Some(q) = self.parked.get(&id) else {
            return Step::Quiet;
        };
        let has_posts = !q.posts.is_empty();
        match self.where_is(id) {
            Where::Fly => Step::Quiet,
            Where::Gone if has_posts => Step::NeedRoom,
            Where::Gone => {
                let q = self.parked.remove(&id).expect("just seen");
                for tx in q.saves {
                    let _ = tx.send(Ok(()));
                }
                Step::Quiet
            }
            Where::Res(s, dirty) => {
                let posts = self
                    .parked
                    .get_mut(&id)
                    .map(|q| std::mem::take(&mut q.posts))
                    .expect("just seen");
                let mut dirty = dirty;
                let mut wrote = false;
                for p in posts {
                    wrote |= p(Ok(self.disk.page_mut(s)));
                }
                if wrote {
                    self.disk.dirtied(s);
                    dirty = true;
                }
                let q = self.parked.get_mut(&id).expect("just seen");
                if dirty && !q.saves.is_empty() {
                    return Step::Save { s };
                }
                let q = self.parked.remove(&id).expect("just seen");
                for tx in q.saves {
                    let _ = tx.send(Ok(()));
                }
                Step::Quiet
            }
        }
    }

    /// Hand a slot to a page the driver found room for. The page's
    /// `load` — settled at its birth, and the first post is the birth's
    /// own wish — decides: an armed load goes out with its lease;
    /// otherwise the page binds and the work may run.
    pub(super) fn give_slot(&mut self, id: PageId, s: SlotId) -> Claim {
        let load = self.parked.get(&id).is_some_and(|q| q.load);
        if load {
            Claim::Load(self.disk.load_start(s, id))
        } else {
            self.disk.bind(s, id);
            Claim::Bound
        }
    }

    /// Hand out a save's lease: the slot goes Saving — the driver flies
    /// the bytes.
    pub(super) fn begin_save(&mut self, s: SlotId) -> SavePage {
        self.disk.start_save(s)
    }

    // ## Room

    /// Claim a slot for a new page: a free or clean-victim slot at once
    /// (a clean victim is unmapped right here); a condemned dirty victim
    /// is kicked — its save goes out with the lease, its landing settles
    /// it clean and demoted, and the next victim search takes it. `None`
    /// — every slot is in flight or freshly kicked.
    pub(super) fn take_room(&mut self) -> Room {
        match self.disk.find_victim() {
            Some(Victim::Free(s)) => Room::Slot(s),
            Some(Victim::Clean(s)) => {
                self.disk.unmap(s);
                Room::Slot(s)
            }
            Some(Victim::Dirty(s)) => {
                let page = self.disk.page_of(s).expect("a victim has a page");
                let buf = self.disk.start_save(s);
                Room::Kicked { page, s, buf }
            }
            None => Room::None,
        }
    }

    /// A tied slot is never free (`unmap` unties first).
    fn where_is(&self, id: PageId) -> Where {
        match self.disk.slot_of(id) {
            Some(s) => match self.disk.state(s) {
                SlotSt::Resident { dirty, .. } => Where::Res(s, dirty),
                SlotSt::Loading | SlotSt::Saving { .. } => Where::Fly,
                SlotSt::Free => Where::Gone,
            },
            None => Where::Gone,
        }
    }

    // ## Persistence

    /// Persist a page — `ensure_saved`'s whole business: an idle page
    /// that needs nothing is already whole on disk (answered at once);
    /// any other state parks a save request behind the page's work. The
    /// return — whether the page had no parked work (the driver starts
    /// it moving then).
    pub(super) fn persist(&mut self, id: PageId, tx: Reply) -> bool {
        if !self.disk.needs_save(id) && !self.parked.contains_key(&id) {
            let _ = tx.send(Ok(())); // on disk, and nothing parked above it
            return false;
        }
        let q = self.parked.entry(id).or_default();
        let idle = q.posts.is_empty();
        q.saves.push(tx);
        idle
    }

    /// A checkpoint: every dirty resident takes a save request (the
    /// driver launches its save) and every save already in the air takes
    /// a request too — its bytes may reach the disk after the caller's
    /// sync otherwise. Posts parked on a page that is still loading or
    /// has no slot are not waited for — they were not part of the world
    /// this checkpoint names; a post parked behind a save on a dirty page
    /// runs when that save lands, and its dirt is saved too. The return —
    /// the dirty pages whose saves the driver must launch, and the
    /// receivers the caller awaits.
    pub(super) fn checkpoint(&mut self) -> (Vec<PageId>, Vec<ReplyRx>) {
        let dirty = self
            .disk
            .pages_where(|st| matches!(st, SlotSt::Resident { dirty: true, .. }));
        let flying = self
            .disk
            .pages_where(|st| matches!(st, SlotSt::Saving { .. }));
        let mut launch = Vec::new();
        let mut rxs = Vec::new();
        let work = dirty
            .into_iter()
            .map(|page| (page, true))
            .chain(flying.into_iter().map(|page| (page, false)));
        for (page, start) in work {
            let (tx, rx) = oneshot::channel();
            self.parked.entry(page).or_default().saves.push(tx);
            if start {
                launch.push(page);
            }
            rxs.push(rx);
        }
        (launch, rxs)
    }

    // ## Landings

    /// A load settled: resident-clean, tied, touched — the parked work
    /// may run.
    pub(super) fn load_landed(&mut self, s: SlotId, id: PageId, p: LoadPage) {
        self.disk.reclaim(s, id, p);
    }

    /// A save settled: resident-clean, keeping the demoted `used` bit
    /// for eviction — the parked work may run.
    pub(super) fn save_landed(&mut self, s: SlotId, p: SavePage) {
        self.disk.save_settle(s, p);
    }

    /// A failed load: the slot untied and the page's work comes off —
    /// the posts refuse, the save requests get the error.
    pub(super) fn load_failed(&mut self, s: SlotId, id: PageId, p: LoadPage, e: &StoreError) {
        self.disk.load_fail(s, id, p);
        self.drain(id, e);
    }

    /// A failed save: the dirt returned, the work comes off as after a
    /// load failure — the posts refuse, the save requests get the error.
    pub(super) fn save_failed(&mut self, s: SlotId, id: PageId, p: SavePage, e: &StoreError) {
        self.disk.save_fail(s, p);
        self.drain(id, e);
    }

    /// A failed flight's page: its work comes off — the posts refuse,
    /// the save requests get the error.
    fn drain(&mut self, id: PageId, e: &StoreError) {
        if let Some(q) = self.parked.remove(&id) {
            for p in q.posts {
                p(Err(e.clone()));
            }
            for tx in q.saves {
                let _ = tx.send(Err(e.clone()));
            }
        }
    }
}
