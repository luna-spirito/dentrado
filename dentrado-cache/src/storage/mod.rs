//! The store's async surface: the reads ([`records`]) — the live
//! world's point reads and scans, and the session's coherent snapshot
//! reads — and the write session ([`session`]), the one door to the
//! store's pages. A record's value is the caller's inline bytes;
//! anything bigger the
//! caller pages itself (the session's drafts are its pages, the schema
//! its declaration of what a record names), and reads of those pages
//! go by reference — bytes never leave the arena. Under it:
//! [`crate::core`] — the flush-agnostic machines the sections drive,
//! [`crate::persist`] — the persistence overlay, the flush round,
//! recovery.
//!
//! The state is the [`PersistentCore`] under a single `RefCell` (the
//! store is single-threaded by construction — compio thread-per-core,
//! `!Sync`); an atomic section is a short `borrow_mut` with no await
//! inside (the analogue of the model's `Op.state`). The disk is the
//! [`Storage`] trait; the file handle (`Clone`, positional io) lives
//! in the cache's disk arena ([`crate::blockcache`]). Handles
//! outliving an operation hold their world through an `Rc` — the read
//! guard ([`Guard`], internal) the store, a flight's lease the arena's
//! pages; the pool's slow paths are the same sections plus the flight
//! driver and the round agent — tasks that hold the store and die by
//! `Store`'s own drop.
//!
//! Loop bounds (fuel) are estimates derived in the model; running out
//! is a visible [`StoreError::Steps`] error (image corruption).

mod records;
mod session;
mod flush;
mod image;

pub use session::{DraftPageId, Session};

use std::cell::RefCell;
use std::rc::Rc;

use compio::runtime::JoinHandle;

use crate::disk::IsFilesystem;
use crate::page::PageId;
use crate::persist::PersistentCore;

// ## The read guard

/// The read guard — an EBR pin (the subscription bucket) held across a
/// multi-page walk. Death is RAII: dropping it unsubscribes and
/// advances the emptied bucket.
pub(crate) struct Guard<D: IsFilesystem> {
    pub(crate) inner: Rc<RefCell<PersistentCore<D>>>,
    pub(crate) bucket: usize,
}

impl<D: IsFilesystem> Drop for Guard<D> {
    fn drop(&mut self) {
        self.inner.borrow_mut().core.drop_step(self.bucket);
    }
}

// ## Store

/// An atomic cache store over the disk `D`. The driver's and the round
/// agent's handles are the store's own death switch: dropping the
/// store drops them, which cancels the tasks (compio's `JoinHandle`
/// cancels on drop, it does not detach) — and their `Rc`s are the
/// store's last references, so the store dies with them, not before
/// them.
pub struct Storage<D: IsFilesystem> {
    pub(crate) inner: Rc<RefCell<PersistentCore<D>>>,
    pub(crate) _driver: JoinHandle<!>,
    pub(crate) _flusher: JoinHandle<!>,
}

/// State inspection at rest (tests, debugging).
#[derive(Debug, Clone)]
pub struct DebugState {
    pub delta_len: usize,
    pub heads: [Vec<PageId>; 2],
    pub free_nexts: [PageId; 2],
    pub r#gen: u64,
    pub flush_busy: bool,
    pub dying_len: usize,
}

/// A page-partition violation — [`Store::partition_exact`]'s report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// An in-world page no role covers.
    Missing(PageId),
    /// An illegal combination of roles (a duplicate listing included)
    /// on one page: `tree`, `value`, `chunk`, `front`-cell, `free`-cell,
    /// `head`, `draft`, `held`, `spent`, `delta`.
    Roles(PageId, Vec<&'static str>),
    /// The image itself does not walk: a value chain broke past a valid
    /// SB, or a runtime bookmark sits off its pool's committed chain.
    Broken(&'static str),
}

impl<D: IsFilesystem> Storage<D> {
    pub fn debug_state(&self) -> DebugState {
        let (r#gen, flush_busy, ram) = {
            let inner = self.inner.borrow();
            (
                inner.r#gen,
                !inner.mail.is_empty() || inner.core.round_live(),
                inner.core.ram(),
            )
        };
        DebugState {
            delta_len: ram.delta,
            heads: ram.heads,
            free_nexts: ram.nexts,
            r#gen,
            flush_busy,
            dying_len: ram.dying,
        }
    }
}
