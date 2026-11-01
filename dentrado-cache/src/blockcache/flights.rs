//! The wings: the flights, the registry, and the driver task that polls
//! it. A slot travels as its own io buffer — its lease, `arena::LoadPage`
//! or `arena::SavePage`, is the flight's sole passenger.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use compio::runtime::JoinHandle;
use futures::future::poll_fn;

use super::arena::{LoadPage, SavePage};
use super::{BlockCache, SlotId};
use crate::disk::IsFilesystem;
use crate::error::StoreError;
use crate::page::PageId;
use crate::persist::PersistentCore;

// ## Flights

/// Which way a flight flew, with its lease back: a load (the kernel
/// wrote the slot bytes) or a save (it read them). What becomes of the
/// slot afterwards is the landing's business — loads and saves both
/// settle the page resident-clean; eviction is not a kind: a kicked
/// victim simply carries a demoted `used` bit, and the next victim
/// search takes it.
pub(super) enum Wings {
    Load(LoadPage),
    Save(SavePage),
}

/// A flight's outcome: over whom, how it ended, the lease back — the
/// slot's fate and the task's posts are decided by the landing (whose
/// verbs consume the lease; a dropped flight's lease dies with it).
pub(super) struct FlDone {
    pub(super) s: SlotId,
    pub(super) id: PageId,
    pub(super) ok: Result<(), StoreError>,
    pub(super) wings: Wings,
}

/// A flight is positional io over a slot's bytes: a load (the kernel
/// writes) or a save (it reads). It owns its data: the file handle is a
/// clone, the buffer is the slot's lease (the sole passenger, keeping
/// the arena's pages alive for the duration of the flight). No fate of
/// its own — the outcome goes to the landing.
///
/// The inner op future is pinned in its box (`!Unpin` — the kernel holds
/// a pointer to the buffer from submission, it cannot move), which makes
/// the flight itself `Unpin` — the registry grows and moves flights
/// freely, with no `dyn`.
enum Flight<D: IsFilesystem> {
    Load {
        s: SlotId,
        id: PageId,
        fut: Pin<Box<D::Read<LoadPage>>>,
    },
    Save {
        s: SlotId,
        id: PageId,
        fut: Pin<Box<D::Write<SavePage>>>,
    },
}

impl<D: IsFilesystem> Future for Flight<D> {
    type Output = FlDone;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<FlDone> {
        match self.get_mut() {
            Flight::Load { s, id, fut } => fut.as_mut().poll(cx).map(|(ok, page)| FlDone {
                s: *s,
                id: *id,
                ok: ok.map_err(StoreError::from_io),
                wings: Wings::Load(page),
            }),
            Flight::Save { s, id, fut } => fut.as_mut().poll(cx).map(|(ok, page)| FlDone {
                s: *s,
                id: *id,
                ok: ok.map_err(StoreError::from_io),
                wings: Wings::Save(page),
            }),
        }
    }
}

// ## The flight registry

/// The flight registry: flights + the driver's waker. Pushed to from any
/// store section (write and wake), polled by the driver in short
/// sections — a borrow never lives across an await, so single-threaded
/// no wakeup is ever lost: a push is either seen by a poll or wakes the
/// sleeper.
pub(super) struct Flights<D: IsFilesystem> {
    futs: Vec<Flight<D>>,
    waker: Option<Waker>,
}

impl<D: IsFilesystem> Flights<D> {
    pub(super) fn new() -> Self {
        Self {
            futs: Vec::new(),
            waker: None,
        }
    }

    /// Wake the sleeper (a flight arrived).
    fn wake(&self) {
        if let Some(w) = &self.waker {
            w.wake_by_ref();
        }
    }

    /// Take a flight in and wake the driver.
    fn push(&mut self, f: Flight<D>) {
        self.futs.push(f);
        self.wake();
    }

    /// Poll everything alive: the ready are taken away; nothing ready —
    /// the sleeper is recorded, a push wakes it.
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Vec<FlDone> {
        let mut done = Vec::new();
        let mut i = 0;
        while i < self.futs.len() {
            // Flight: Unpin — nothing to pin
            match Pin::new(&mut self.futs[i]).poll(cx) {
                Poll::Ready(d) => {
                    self.futs.swap_remove(i); // the flight has flown out
                    done.push(d);
                }
                Poll::Pending => i += 1,
            }
        }
        if done.is_empty() {
            self.waker = Some(cx.waker().clone());
        }
        done
    }
}

impl<D: IsFilesystem> BlockCache<D> {
    /// Start a flight over a slot: the lease goes to the registry (any
    /// section can push), the driver wakes and polls.
    pub(super) fn launch(&mut self, s: SlotId, page: PageId, wings: Wings) {
        let f = match wings {
            Wings::Load(buf) => Flight::Load {
                s,
                id: page,
                fut: Box::pin(D::read_page(self.seq.file(), page, buf)),
            },
            Wings::Save(buf) => Flight::Save {
                s,
                id: page,
                fut: Box::pin(D::write_page(self.seq.file(), page, buf)),
            },
        };
        self.fl.push(f);
    }
}

// ## The driver

/// The flight driver — a task owning the store whole: the registry is a
/// field of it, every section pushes through its own borrow, the
/// driver reaps in short ones (a borrow never lives across an await,
/// so single-threaded no wakeup is ever lost). It never exits on its
/// own — the store's death is its death: `Store` holds the task's
/// [`JoinHandle`], and dropping the handle cancels the task, dropping
/// the future with its `Rc` — the store's last reference. The
/// in-flight ops die cancelled, and each of their buffers stays
/// compio's until its op is reaped, so no page is ever torn from under
/// the kernel.
async fn drive<D: IsFilesystem>(inner: Rc<RefCell<PersistentCore<D>>>) -> ! {
    loop {
        poll_fn(|cx| match inner.borrow_mut().core.reap(cx) {
            0 => Poll::Pending,
            _ => Poll::Ready(()),
        })
        .await;
    }
}

/// Spawn the driver task at store birth: the handle goes to `Store` —
/// its drop is the driver's death sentence. Only the spawn needs a
/// runtime context, hence this lives here and not in the constructor.
pub(crate) fn spawn_driver<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
) -> JoinHandle<!> {
    compio::runtime::spawn(drive(Rc::clone(inner)))
}
