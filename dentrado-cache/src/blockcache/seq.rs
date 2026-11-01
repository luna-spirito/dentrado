//! The machine: the pages' step queues over the disk arena — wishes,
//! room, markers, landings, and the synchronous service of resident
//! pages. A strict ordering interface: it advances a page's queue until
//! the queue needs the world (a flight, a slot), and the driving itself
//! — who flies, who waits for room — stays one level up, in the driver.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::{Rc, Weak};

use futures::channel::oneshot;

use super::arena::{LoadPage, SavePage, SlotSt, Victim};
use super::disk_arena::DiskArena;
use super::{PostFn, Reply, SlotId};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{Page, PageId};

// ## The queue's kinds

/// One step of a page's queue: a post; a save — the page must become
/// durable before the queue moves on (nothing to do if the bytes are
/// already clean or the page gone); a marker — a checkpoint's seat,
/// firing when the queue drains past it.
enum Act {
    Post(PostFn),
    Save,
    Mark(Rc<RefCell<Waiter>>), // TODO: I forgor this, it shouldn't be needed in this form I believe? No?
}

/// A checkpoint's countdown: the markers own clones of the record, the
/// last to fire (or a failed save, on behalf of everyone) delivers the
/// answer.
// TODO: почему debt+barriers — не одна и та же структура?
// ANSWER: they became one — a barrier is its markers, seats in the
// queues themselves; the countdown is the only remnant, and the
// registry holds it weakly: the markers' lifetime is the barrier's.
struct Waiter {
    left: usize,
    tx: Option<Reply>,
}

/// A marker reached: one seat fewer; the last one answers. A waiter
/// already answered (a failed save aborted it) stays silent.
fn fire(m: &Rc<RefCell<Waiter>>) {
    let mut w = m.borrow_mut();
    w.left = w.left.saturating_sub(1);
    if w.left == 0 {
        if let Some(tx) = w.tx.take() {
            drop(tx.send(Ok(())));
        }
    }
}

// ## The vocabulary with the driver

/// How far one `step` of a page's queue went: quiet — the queue drained
/// (the entry left with it) or the page is in flight (its landing
/// resumes it), nothing for the driver to do; the head post needs a
/// slot; the head save holds a dirty resident — the driver flies it
/// (`begin_save`).
pub(super) enum Step {
    Quiet,
    NeedRoom,
    Save { s: SlotId },
}

/// The room search's answer: a slot to take; a kicked dirty victim —
/// its save sits at the front of its queue, the driver must drive it
/// (and there is no room this time); no room at all — every slot is in
/// flight.
pub(super) enum Room {
    Slot(SlotId),
    Kicked(PageId),
    None,
}

/// A handed-over slot's fate: the page bound — the queue may run; a
/// load armed — the driver flies the lease, and the landing runs the
/// queue.
pub(super) enum Claim {
    Bound,
    Load(LoadPage),
}

/// A wish's fate: served on the spot — nothing queued, nothing to
/// drive; or seated in a queue, the answer arriving by the receiver
/// when the queue reaches the post — `idle`: the queue was born with
/// this very wish, the driver must start it moving.
pub(crate) enum Submit<R> {
    Now(R),
    Queued {
        rx: oneshot::Receiver<Result<R, StoreError>>,
        idle: bool,
    },
}

// ## The machine's state

/// A page's queue: its steps, and the disk question settled at its
/// birth — `load` says the head post wants the disk copy read when a
/// slot arrives (`give_slot`'s only business; posts joining a live
/// queue inherit its state — the page is in the system already, or
/// the birth decided the past). Queues born otherwise — a save of
/// `persist`'s, a kick, a checkpoint — take the `false` default: their
/// page never waits for a slot.
#[derive(Default)]
struct Q {
    load: bool,
    acts: VecDeque<Act>,
}

/// The sequential machine: the disk arena (mirrors, the file) and the
/// pages' queues in one struct — the coupled invariants are its private
/// business: a queue is never empty while it exists (emptied — removed,
/// entry and all), and only its head is ever in motion (one flight per
/// page at most). It knows nothing of the driving: no flights, no
/// waiting line — those live one level up, in the driver.
pub(super) struct SeqArena<D: IsFilesystem> {
    /// The disk arena below — the machine's own; the layers above reach
    /// the pages only through the machine's verbs.
    disk: DiskArena<D>,
    /// Pages' step queues — flying and waiting for a slot alike.
    queues: HashMap<PageId, Q>,
    /// Live checkpoints, held weakly: a failed save aborts every waiter
    /// and empties the registry, and the markers still queued fire
    /// silently after.
    bars: Vec<Weak<RefCell<Waiter>>>,
}

/// Where a page stands: resident (its slot, with the dirty bit), in
/// flight, or out of the cache.
enum Where {
    Res(SlotId, bool),
    Fly,
    Gone,
}

impl<D: IsFilesystem> SeqArena<D> {
    pub(super) fn new(file: D::File, pages: Rc<[std::cell::UnsafeCell<Page>]>) -> Self {
        Self {
            disk: DiskArena::new(file, pages),
            queues: HashMap::new(),
            bars: Vec::new(),
        }
    }

    /// The file handle — for the wings' takeoff and the store's own
    /// syncs; the disk arena itself stays the machine's own.
    pub(super) fn file(&self) -> D::File {
        self.disk.file()
    }

    // ## Wishes

    /// A wish against a page: `f` runs over its bytes — `true`, the
    /// write left it dirty. A page at rest (no live queue) is served
    /// right here, and an init claims a fresh slot besides (`load`
    /// says the disk past matters — only an init's doesn't); anything
    /// else takes a seat in the queue, the answer by the receiver.
    pub(super) fn submit<R>(
        &mut self,
        id: PageId,
        load: bool,
        f: impl FnOnce(&mut Page) -> (bool, R) + 'static,
    ) -> Submit<R>
    where
        R: 'static,
    {
        if !self.queues.contains_key(&id) {
            match self.disk.slot_of(id) {
                Some(s) => {
                    if let SlotSt::Resident { .. } = self.disk.state(s) {
                        let (dirty, r) = f(self.disk.page_mut(s));
                        if dirty {
                            self.disk.dirtied(s);
                        } else {
                            self.disk.touch(s);
                        }
                        return Submit::Now(r);
                    }
                    // in flight — the bytes belong to a lease; queue behind
                }
                None if !load => match self.take_room() {
                    // a fresh page has no past: a claimed slot is enough
                    Room::Slot(s) => {
                        self.disk.bind(s, id);
                        let (_, r) = f(self.disk.page_mut(s));
                        return Submit::Now(r);
                    }
                    // no room — the slow path's drive flies the kicked
                    // victim's save, and this wish waits in line
                    _ => {}
                },
                _ => {}
            }
        }
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
        let q = self.queues.entry(id).or_default();
        let idle = q.acts.is_empty();
        if idle {
            q.load = load; // the birth settles the disk question
        }
        q.acts.push_back(Act::Post(post));
        Submit::Queued { rx, idle }
    }

    /// Does a waiting page still want a slot: its queue alive and the
    /// page out of the cache.
    pub(super) fn wants_room(&self, id: PageId) -> bool {
        self.queues.contains_key(&id) && self.disk.slot_of(id).is_none()
    }

    // ## The motor's core

    /// Run a page's queue until it needs the world: posts against a
    /// resident page, markers on the spot; a save over dirt, or a
    /// slotless page's head post, stops the run for the driver.
    pub(super) fn step(&mut self, id: PageId) -> Step {
        enum Head {
            Post,
            Save,
            Mark(Rc<RefCell<Waiter>>),
        }
        loop {
            let head = match self.queues.get_mut(&id).and_then(|q| q.acts.front()) {
                Some(Act::Post(..)) => Head::Post,
                Some(Act::Save) => Head::Save,
                Some(Act::Mark(m)) => Head::Mark(Rc::clone(m)),
                None => {
                    self.queues.remove(&id);
                    return Step::Quiet;
                }
            };
            match head {
                Head::Mark(m) => {
                    self.pop_head(id);
                    fire(&m);
                }
                Head::Post => match self.where_is(id) {
                    Where::Res(s, _) => {
                        let Act::Post(p) = self.pop_head(id) else {
                            unreachable!("the front was just seen as a post");
                        };
                        let dirty = p(Ok(self.disk.page_mut(s)));
                        if dirty {
                            self.disk.dirtied(s);
                            self.cover(id);
                        }
                    }
                    Where::Fly => return Step::Quiet,
                    Where::Gone => return Step::NeedRoom,
                },
                Head::Save => match self.where_is(id) {
                    Where::Res(s, true) => return Step::Save { s },
                    Where::Res(_, false) | Where::Gone => {
                        self.pop_head(id);
                    }
                    Where::Fly => return Step::Quiet,
                },
            }
        }
    }

    /// Drop the queue's head step — the front was just seen, so it is
    /// there.
    fn pop_head(&mut self, id: PageId) -> Act {
        self.queues
            .get_mut(&id)
            .expect("front just seen")
            .acts
            .pop_front()
            .expect("front just seen")
    }

    /// Hand a slot to a page the driver found room for. The queue's
    /// `load` — settled at its birth, and the head is the birth's own
    /// wish — decides: an armed load goes out with its lease; otherwise
    /// the page binds and the queue may run.
    pub(super) fn give_slot(&mut self, id: PageId, s: SlotId) -> Claim {
        let load = match self.queues.get(&id) {
            Some(q) => {
                debug_assert!(matches!(q.acts.front(), Some(Act::Post(_))));
                q.load
            }
            None => false,
        };
        if load {
            Claim::Load(self.disk.load_start(s, id))
        } else {
            self.disk.bind(s, id);
            Claim::Bound
        }
    }

    /// Hand out the head save's lease: the step pops it, the slot goes
    /// Saving — the driver flies the bytes.
    pub(super) fn begin_save(&mut self, id: PageId, s: SlotId) -> SavePage {
        self.pop_head(id);
        self.disk.start_save(s)
    }

    // ## Room

    /// Claim a slot for a new page: a free or clean-victim slot at once
    /// (a clean victim is unmapped right here); a condemned dirty victim
    /// is kicked — its save settles it clean and demoted, and the next
    /// victim search takes it. `None` — every slot is in flight or
    /// freshly kicked.
    // TODO: Само разделение take_room<->find_victim, но как будто бы реализовать его нужно было через Continuation-passing-style, а не через... это.
    // Потому что сейчас find_victim при находке `Victim::Dirty` выбрасывается из цикла, чтобы take_room отработал и вернул процесс в цикл.
    // Это *такое себе*.
    // ANSWER: the loop is gone — one decision per call: a dirty victim
    // is kicked (one save) and the refusal stands. The continuation
    // arrives as the save's landing, which lets the driver pump the
    // waiting pages, so the retry is an io event, not a callback here.
    pub(super) fn take_room(&mut self) -> Room {
        match self.disk.find_victim() {
            Some(Victim::Free(s)) => Room::Slot(s),
            Some(Victim::Clean(s)) => {
                self.disk.unmap(s);
                Room::Slot(s)
            }
            Some(Victim::Dirty(s)) => Room::Kicked(self.kick(s)),
            None => Room::None,
        }
    }

    /// Kick a condemned dirty victim: a save goes to the front of its
    /// queue — the clock has already demoted its `used` bit, so the
    /// landing settles it clean and demoted, and the next victim search
    /// takes it. No one waits for the slot itself.
    fn kick(&mut self, s: SlotId) -> PageId {
        let page = self.disk.page_of(s).expect("a victim has a page");
        self.queues
            .entry(page)
            .or_default()
            .acts
            .push_front(Act::Save);
        page
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

    /// Dirt left by a run post: if a marker waits behind in the queue,
    /// a save goes in front of it — unless one already sits at the
    /// front, which runs after this post all the same and carries the
    /// dirt. Uncovered dirt waits for a future checkpoint's sweep.
    fn cover(&mut self, id: PageId) {
        if let Some(q) = self.queues.get_mut(&id) {
            let later = q.acts.iter().any(|a| matches!(a, Act::Mark(_)));
            let front_save = q.acts.front().is_some_and(|a| matches!(a, Act::Save));
            if later && !front_save {
                q.acts.push_front(Act::Save);
            }
        }
    }

    // ## Persistence

    /// Persist a page — `ensure_saved`'s whole business: an idle page
    /// that needs nothing is already whole on disk (answered at once);
    /// any other state takes a save and an answer as the queue's next
    /// steps — after whatever is already queued (a queued post's dirt
    /// waits for this very save). The return — whether the queue was
    /// idle (the driver starts it moving then).
    pub(super) fn persist(&mut self, id: PageId, tx: Reply) -> bool {
        if !self.disk.needs_save(id) && !self.queues.contains_key(&id) {
            drop(tx.send(Ok(()))); // on disk, and nothing queued above it
            return false;
        }
        let q = self.queues.entry(id).or_default();
        let idle = q.acts.is_empty();
        q.acts.push_back(Act::Save);
        q.acts.push_back(Act::Post(Box::new(move |p| {
            let _ = tx.send(p.map(|_| ()));
            false
        })));
        idle
    }

    /// The checkpoint barrier: a marker behind every live queue, a
    /// save-then-marker for each dirty resident, a lone marker for each
    /// flight in the air (its landing fires it). The return — the swept
    /// dirty pages, whose saves the driver launches right away.
    pub(super) fn checkpoint(&mut self, tx: Reply) -> Vec<PageId> {
        let flying = self
            .disk
            .pages_where(|st| matches!(st, SlotSt::Loading | SlotSt::Saving { .. }));
        let dirty = self
            .disk
            .pages_where(|st| matches!(st, SlotSt::Resident { dirty: true, .. }));
        if self.queues.is_empty() && flying.is_empty() && dirty.is_empty() {
            drop(tx.send(Ok(())));
            return Vec::new();
        }
        let bar = Rc::new(RefCell::new(Waiter {
            left: 0,
            tx: Some(tx),
        }));
        self.bars.push(Rc::downgrade(&bar));
        let mut seats = 0;
        for q in self.queues.values_mut() {
            q.acts.push_back(Act::Mark(Rc::clone(&bar)));
            seats += 1;
        }
        for page in flying {
            if !self.queues.contains_key(&page) {
                self.queues
                    .entry(page)
                    .or_default()
                    .acts
                    .push_back(Act::Mark(Rc::clone(&bar)));
                seats += 1;
            }
        }
        let mut swept = Vec::new();
        for page in dirty {
            let q = self.queues.entry(page).or_default();
            q.acts.push_front(Act::Save);
            q.acts.push_back(Act::Mark(Rc::clone(&bar)));
            swept.push(page);
            seats += 1;
        }
        bar.borrow_mut().left = seats;
        swept
    }

    // ## Landings

    /// A load settled: resident-clean, tied, touched — the queue may run.
    pub(super) fn load_landed(&mut self, s: SlotId, id: PageId, p: LoadPage) {
        self.disk.reclaim(s, id, p);
    }

    /// A save settled: resident-clean, keeping the demoted `used` bit
    /// for eviction — the queue may run.
    pub(super) fn save_landed(&mut self, s: SlotId, p: SavePage) {
        self.disk.save_settle(s, p);
    }

    /// A failed load: the slot untied and the page's queue comes off —
    /// the posts refuse, the markers fire (their barriers count them;
    /// an aborted one stays silent).
    pub(super) fn load_failed(&mut self, s: SlotId, id: PageId, p: LoadPage, e: &StoreError) {
        self.disk.load_fail(s, id, p);
        self.drain(id, e);
    }

    /// A failed save: the dirt returned, the queue comes off as after a
    /// load failure — and no checkpoint survives: every waiter is
    /// answered with the error, the markers still queued fire silently
    /// after.
    pub(super) fn save_failed(&mut self, s: SlotId, id: PageId, p: SavePage, e: &StoreError) {
        self.disk.save_fail(s, p);
        self.drain(id, e);
        for w in std::mem::take(&mut self.bars) {
            if let Some(w) = w.upgrade() {
                let mut w = w.borrow_mut();
                if let Some(tx) = w.tx.take() {
                    drop(tx.send(Err(e.clone())));
                }
            }
        }
    }

    /// A failed flight's page: its queue comes off — the posts refuse,
    /// the markers fire. The save step already flew.
    fn drain(&mut self, id: PageId, e: &StoreError) {
        if let Some(q) = self.queues.remove(&id) {
            for a in q.acts {
                match a {
                    Act::Post(p) => {
                        p(Err(e.clone()));
                    }
                    Act::Save => {}
                    Act::Mark(m) => fire(&m),
                }
            }
        }
    }
}
