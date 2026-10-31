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
use crate::disk::Storage;

/// The single lock's full content: the core plus the round agent's
/// mailbox and the commit generation. Everything else a round carries
/// is the flight's own locals — these are all the overlay owns
/// between rounds.
pub(crate) struct PersistentCore<D: Storage> {
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
}

impl<D: Storage> PersistentCore<D> {
    pub(super) fn fresh(file: D::File, cap: usize, n_tables: usize) -> Self {
        Self {
            core: Core::fresh(file, cap, n_tables),
            mail: Vec::new(),
            mail_ev: Event::new(),
            r#gen: 0,
        }
    }

    /// Recovery over a live store: the image's fields go on top; the
    /// block cache is not touched (the validation chains are read
    /// directly).
    pub(super) fn of_sb(&mut self, sb: &Sb) {
        self.core.of_sb(sb);
        self.r#gen = sb.r#gen;
    }
}
