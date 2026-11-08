//! The persistence overlay: the wrapper over the core that carries
//! the flushing-specific half of the single lock's content — the
//! round agent's mailbox and the commit generation. The core
//! ([`crate::core::Core`]) knows nothing of flush rounds; the agent
//! and its rounds ([`flush`]) drive this wrapper, recovery
//! ([`image`]) rebuilds it from the image. The surface
//! ([`crate::store`]) holds the wrapper under its single `RefCell`;
//! the round agent and recovery are its async verbs.

mod api;
pub(crate) mod flight;
pub(crate) mod graft;
pub(crate) mod walks;

pub(crate) use api::{ensure_saved, init, read, update, write_back_all};

use std::{cell::RefCell, panic::AssertUnwindSafe, rc::Rc, task::Poll};

use compio::runtime::JoinHandle;
use futures::{FutureExt, future::poll_fn};
use synchrony::unsync::event::Event;

use crate::blockcache::Reply;
use crate::core::Core;
use crate::core::sb::Sb;
use crate::disk::IsFilesystem;
use crate::schema::TableSpec;

/// The single lock's full content: the core plus the round agent's
/// mailbox, the commit generation, and the poison flag. Everything
/// else a round carries is the flight's own locals — these are all the
/// overlay owns between rounds.
pub(crate) struct PersistentCore<D: IsFilesystem> {
    /// The flush-agnostic core — the machines by concern.
    pub(crate) core: Core<D>,
    /// The round agent's mailbox: the reply addresses of everyone
    /// waiting for a round. A dropped waiter cancels nothing — only
    /// its letter dies; the agent drains the batch into one round
    /// regardless.
    pub(super) mail: Vec<Reply>,
    /// The mailbox's bell: a letter rings it. The agent listens
    /// before it looks (the letter and the ring are one section, so
    /// an empty look with a live listener never misses a letter).
    pub(super) mail_ev: Event,
    /// The last commit's generation — the SB slot alternator.
    pub(super) r#gen: u64,
    /// The poison flag — terminal. Set by the driver or a round that
    /// panicked, or by the surface after a hard failure; from then on
    /// every semantic operation refuses ([`StoreError::Poisoned`]).
    /// The io plumbing (the driver, the flight queues) keeps running:
    /// already-parked readers still get their flights, and nothing
    /// wedges. The store is never inspected or reused as a source of
    /// truth after the flag — the caller degrades.
    pub(crate) poisoned: bool,
}

impl<D: IsFilesystem> PersistentCore<D> {
    pub(super) fn fresh(file: D::File, cap: usize, tables: &[TableSpec]) -> Self {
        Self {
            core: Core::fresh(file, cap, tables),
            mail: Vec::new(),
            mail_ev: Event::new(),
            r#gen: 0,
            poisoned: false,
        }
    }

    /// Poison — see the field's doc.
    pub(crate) fn poison(&mut self) {
        self.poisoned = true;
    }

    /// Recovery over a live store: the image's fields go on top; the
    /// block cache is not touched (the validation chains are read
    /// directly).
    pub(super) fn of_sb(&mut self, sb: &Sb, opener: &[TableSpec]) {
        self.core.of_sb(sb, opener);
        self.r#gen = sb.r#gen;
    }
}

/// The flight driver — a task owning the store whole: the registry is a
/// field of it, every section pushes through its own borrow, the driver reaps
/// in short ones (a borrow never lives across an await, so single-threaded no
/// wakeup is ever lost). It never exits on its own — the store's death is its
/// death: `Storage` holds the task's [`JoinHandle`], and dropping the handle
/// cancels the task, dropping the future with its `Rc` — the store's last
/// reference. The in-flight ops die cancelled, and each of their buffers stays
/// compio's until its op is reaped, so no page is ever torn from under the
/// kernel.
async fn drive<D: IsFilesystem>(inner: Rc<RefCell<PersistentCore<D>>>) -> ! {
    loop {
        // A panicking reap poisons the store (from then on every semantic op
        // refuses) — but the loop lives on: the io plumbing must keep
        // draining, or the readers parked before the poison would wait for
        // flights that never land. The default panic hook has already
        // reported the cause by the time we land here.
        let step = poll_fn(|cx| match inner.borrow_mut().core.reap(cx) {
            0 => Poll::Pending,
            _ => Poll::Ready(()),
        });
        if AssertUnwindSafe(step).catch_unwind().await.is_err() {
            inner.borrow_mut().poison();
        }
    }
}

/// Spawn the driver task at store birth: the handle goes to `Storage` — its
/// drop is the driver's death sentence. Only the spawn needs a runtime
/// context, hence this lives here and not in the constructor.
pub(crate) fn spawn_driver<D: IsFilesystem>(
    inner: &Rc<RefCell<PersistentCore<D>>>,
) -> JoinHandle<!> {
    compio::runtime::spawn(drive(Rc::clone(inner)))
}
