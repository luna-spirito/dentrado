//! The persistence overlay: the wrapper over the core that carries
//! the flushing-specific half of the single lock's content — the
//! round agent's mailbox and the commit generation. The core
//! ([`crate::core::Core`]) knows nothing of flush rounds; the agent
//! and its rounds ([`flush`]) drive this wrapper, recovery
//! ([`image`]) rebuilds it from the image. The surface
//! ([`crate::store`]) holds the wrapper under its single `RefCell`;
//! the round agent and recovery are its async verbs.

mod flight;
mod flush;
mod graft;
mod image;

use synchrony::unsync::event::Event;

use crate::blockcache::Reply;
use crate::core::Core;
use crate::core::sb::Sb;
use crate::disk::IsFilesystem;

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
    pub(super) fn fresh(file: D::File, cap: usize, tables: &[&str]) -> Self {
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
    pub(super) fn of_sb(&mut self, sb: &Sb, opener: &[&str]) {
        self.core.of_sb(sb, opener);
        self.r#gen = sb.r#gen;
    }
}
