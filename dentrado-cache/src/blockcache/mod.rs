//! Page pool with clock eviction — a port of `StorageModel/BlockCache.lean`.
//!
//! All hot data lives in the [`BlockCache`]: `cap` slots, never grown; it
//! is a field of the store's core, every transition is a short section of
//! the store's single `RefCell`, no await inside.
//!
//! A page wish goes through one door: [`BlockCache::request`] seats it
//! in the machine ([`seq::SeqArena::submit`]), which serves a page at
//! rest on the spot, and turns anything else — a fresh page's init
//! included — into a post ([`PostFn`]): the caller's closure with a
//! oneshot home, run against the page (or a refusal) exactly once. The
//! post joins the page's queue — the queue's order is the issue order,
//! and only its head is ever in motion (one flight per page at most);
//! a page without a slot waits in line, and every landing passes freed
//! rooms down ([`BlockCache::pump`]).
//!
//! Below sits the sequential machine ([`seq::SeqArena`]): the disk arena
//! (mirrors, the file) and the pages' queues in one struct, a strict
//! ordering interface — it advances a page's queue until the queue needs
//! the world. The driving is the cache's own: the waiting line
//! (`pending`) and the io futures in the [`Flights`] registry — a section
//! pushes through its own borrow and wakes the sleeper, the driver polls
//! in short sections of its own — a borrow never lives across an await,
//! so no wakeup is ever lost. The driver task holds the whole store
//! through its `Rc` and never exits on its own: `Store` holds its
//! `JoinHandle`, whose drop cancels the task (compio's contract, not
//! tokio's detach) — the death order is the store's own, and a cancelled
//! op's buffer stays compio's until the op is reaped, so the kernel never
//! touches a freed page.
//!
//! Checkpoints are markers in the queues, not epoch ledgers: a barrier
//! drops a marker behind every live queue and materializes a
//! save-then-marker queue for each dirty resident (a lone marker for
//! each flight in the air); a marker fires when the queue drains past it
//! — everything before it settled, its dirt saved before it — and the
//! last marker to fire delivers the answer. Posts arriving later append
//! behind the marker and never delay it; dirt left by a post ahead of
//! one grows a save at the queue's front.
//!
//! Eviction is lazy: a condemned dirty victim is saved like any other
//! dirty page — it stays resident, clean, its `used` bit already demoted
//! by the clock — and the next victim search sweeps it out. A failed load
//! is terminal for its queue; a failed save returns the dirt — persist
//! and checkpoint answers get the error; whoever waited behind a failed
//! eviction is served from the returned page. A true deadlock (a pool
//! with no slots forever) is an eternal wait.
//!
//! The layers, bottom-up: `arena` (slots of bytes with the clock hand,
//! the slot state machine and the flight leases — it knows pages exist,
//! not that they map to a disk), `disk_arena` (the mirrors id↔slot, the
//! file — the slot transitions), `seq` (the sequential machine: queues,
//! markers, room, and the synchronous service of resident pages — the
//! ordering interface over the disk arena), `flights` (the wings: io
//! futures, the driver task), `api` (the desk: what the store calls).
//! [`BlockCache`] itself is the driver: a sequential arena below, the
//! waiting line and the wings beside it — it decides who flies.
//!
//! Deliberate departures from the model: a model step executes
//! immediately — here an action may have to travel by flights before it
//! reaches the page (the fast path takes none); a write failure surfaces
//! as an error upward — the model's disk never refuses writes; a
//! checkpoint is a positional barrier in the queues, not the model's
//! refusal with "Saving left".

mod api;
mod arena;
mod disk_arena;
mod flights;
mod seq;

pub(crate) use self::api::{ensure_saved, init, read, update, write_back_all};
pub(crate) use self::flights::spawn_driver;

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::task::Context;

use futures::channel::oneshot;

use self::flights::{FlDone, Flights, Wings};
use self::seq::{Claim, Room, SeqArena, Step};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::{AlignedPage, PageId};

pub(crate) use self::seq::Submit;

pub(crate) type SlotId = usize;

/// A post function — an action over a page when its turn comes:
/// `submit`'s own wrapping of the caller's `f` (the answer policy is
/// the wrapper's: `Ok` with the dirt flag, a refusal by the oneshot);
/// `true` — the write left the page dirty.
pub(super) type PostFn = Box<dyn FnOnce(Result<&mut AlignedPage, StoreError>) -> bool>;

/// The settle answer's channel — a persist's or a checkpoint barrier's:
/// `Ok` when the last marker fired, `Err` from a failed save.
pub(crate) type Reply = oneshot::Sender<Result<(), StoreError>>;

/// The block cache: the sequential machine below (the disk arena, the
/// pages' queues) and the driver over it — the waiting line for room
/// and the flights. Every short section over the store's `RefCell` runs
/// through here.
pub(crate) struct BlockCache<D: IsFilesystem> {
    seq: SeqArena<D>,
    /// Pages waiting for a slot, front first: each has a live queue and
    /// no page in the cache; every landing passes freed rooms down
    /// ([`BlockCache::pump`]).
    pending: VecDeque<PageId>,
    fl: Flights<D>,
}

// ## The driver

impl<D: IsFilesystem> BlockCache<D> {
    /// The pages come ready-made from the store's top — born in
    /// `PersistentCore::fresh` and shared with flights from their first
    /// breath (see the arena's docs).
    pub(crate) fn new(file: D::File, pages: Rc<[UnsafeCell<AlignedPage>]>) -> Self {
        Self {
            seq: SeqArena::new(file, pages),
            pending: VecDeque::new(),
            fl: Flights::new(),
        }
    }

    /// The file handle for the store's flush syncs (positional io,
    /// `Clone`) — the await stays outside every borrow.
    pub(crate) fn file(&self) -> D::File {
        self.seq.file()
    }

    /// Run a wish against the page: served on the spot, or seated in
    /// the queue — a queue born with it starts moving at once (an
    /// idle-born queue is never resident: the machine would have
    /// served it instead).
    pub(crate) fn request<R>(
        &mut self,
        id: PageId,
        load: bool,
        f: impl FnOnce(&mut AlignedPage) -> (bool, R) + 'static,
    ) -> Submit<R>
    where
        R: 'static,
    {
        let served = self.seq.submit(id, load, f);
        if let Submit::Queued { idle: true, .. } = &served {
            self.drive(id);
        }
        served
    }

    /// The page whole on disk — the machine seats the save and the
    /// answer; the driver only starts an idle queue.
    pub(crate) fn persist(&mut self, id: PageId, tx: Reply) {
        if self.seq.persist(id, tx) {
            self.drive(id);
        }
    }

    /// The checkpoint barrier, seated in the machine's queues; the
    /// driver launches the sweep's saves — the markers answer at their
    /// landings.
    pub(crate) fn checkpoint(&mut self, tx: Reply) {
        for page in self.seq.checkpoint(tx) {
            self.drive(page);
        }
    }

    /// The driving loop: run a page's queue as far as the machine takes
    /// it, then do what the world contact asks — fly a save, find a
    /// slot (a kicked victim is driven first; no room — the page waits
    /// in line), fly an armed load.
    fn drive(&mut self, id: PageId) {
        loop {
            match self.seq.step(id) {
                Step::Quiet => return,
                Step::Save { s } => {
                    let buf = self.seq.begin_save(id, s);
                    self.launch(s, id, Wings::Save(buf));
                    return;
                }
                Step::NeedRoom => {
                    let Some(s) = self.take_room() else {
                        self.pending.push_back(id);
                        return;
                    };
                    match self.seq.give_slot(id, s) {
                        Claim::Bound => {} // bound — the loop runs the queue on
                        Claim::Load(buf) => {
                            self.launch(s, id, Wings::Load(buf));
                            return; // the load left — its landing resumes the queue
                        }
                    }
                }
            }
        }
    }

    /// Room for a new page: a slot, or none — a kicked victim's save
    /// flies first, its landing re-opens the search.
    fn take_room(&mut self) -> Option<SlotId> {
        match self.seq.take_room() {
            Room::Slot(s) => Some(s),
            Room::Kicked(victim) => {
                self.drive(victim);
                None
            }
            Room::None => None,
        }
    }

    /// Slot passing: admit the waiting pages — the front first — while
    /// there is any room; the first shortage ends the pass.
    fn pump(&mut self) {
        while let Some(&id) = self.pending.front() {
            if !self.seq.wants_room(id) {
                self.pending.pop_front();
                continue;
            }
            let Some(s) = self.take_room() else { break };
            self.pending.pop_front();
            match self.seq.give_slot(id, s) {
                Claim::Bound => self.drive(id),
                Claim::Load(buf) => self.launch(s, id, Wings::Load(buf)),
            }
        }
    }

    /// The driver's tick: poll the registry, land the ready flights —
    /// one short section, nothing moved stays Pending's business. The
    /// count — how many landed.
    pub(crate) fn reap(&mut self, cx: &mut Context<'_>) -> usize {
        let done = self.fl.poll(cx);
        let n = done.len();
        for d in done {
            self.land(d);
        }
        n
    }

    /// Land a flight: the machine settles the slot and the queue's
    /// fate; the driver resumes the queue and passes the freed room
    /// down.
    fn land(&mut self, d: FlDone) {
        match (d.wings, d.ok) {
            (Wings::Load(page), Ok(())) => {
                self.seq.load_landed(d.s, d.id, page);
                self.drive(d.id);
            }
            (Wings::Save(page), Ok(())) => {
                self.seq.save_landed(d.s, page);
                self.drive(d.id);
            }
            (Wings::Load(page), Err(e)) => {
                self.seq.load_failed(d.s, d.id, page, &e);
            }
            (Wings::Save(page), Err(e)) => {
                self.seq.save_failed(d.s, d.id, page, &e);
            }
        }
        self.pump();
    }
}
