//! Per-core, single-threaded storage — **typed over `R`**.
//!
//! [`CacheStorage`] describes, with full type safety, the operations dentrado
//! performs against persistent (restart-surviving) state. Each operation is a
//! named, typed dentrado interaction: no caller ever names a byte key or
//! serializes a value at this level.
//!
//! ## One implementation
//!
//! There is one backend: page-cache code materializing localization, the
//! event shards, content data, and gear views. Disk mode pairs it with an
//! append-only journal (the source of truth, so a restart replays only the
//! tail); RAM mode runs the same code over a memory disk and survives nothing.
//! Keeping it one implementation lets replay reuse the live applies instead
//! of maintaining a second, subtly different fold.
//!
//! ## Durability of the gear view
//!
//! A gear's working state is not interpreted by dentrado: the gear gets a
//! [`GearView`] — a flat key-value overlay scoped to its stable id — and
//! stores whatever it wants there. A disk backend persists the view through
//! `flush`; a RAM backend loses it with everything else on process death. The
//! view is recomputable from the event log, so a lost view is a cold start,
//! never a correctness break.
//!
//! ## Commit ordering
//!
//! An operation appends its journal record first. Localization rows are
//! visible immediately through the RAM delta; events stay pending until
//! [`CacheStorage::commit_log`], which syncs the journal and then writes the
//! batch's rows, the shard tips, and the cache's applied cursor in one atomic
//! session. A gear view is flushed after the events it derives from, so its
//! watermark can never survive past them.
//!
//! Everything here is restart-surviving state. The RAM disk loses it all on
//! process death — but that is a property of the disk (equally true of its
//! events and localization), not of any one layer.
//!
//! ## Concurrency
//!
//! All methods take `&self` and return an `async` future: the backend is
//! interior-mutable (an async gate plus the store's own handles), so no
//! `RefCell` borrow is held across an `.await` on the caller side. `!Send` by
//! construction, used monomorphically.
//!
//! The `Drop`-driven paths stay synchronous by design: gear views are never
//! touched by `evict_gear`, and a stopped subscription carries an opaque
//! session token instead of reading localization.

pub mod journal;
pub mod store;
pub mod view;
pub mod cache;

pub use cache::CacheStorage;
pub use view::GearView;

use std::marker::PhantomData;

use crate::{
    core::{api::RuntimeTypes, loc_ctx::StoredEvent},
    types::{DataId, GroupEventId, LocDataId, LocGroupId, LocSenderId, LocUserId, SenderPk},
};

/// Changes to a group since a watermark: ids of events added and ids of events
/// superseded/removed, plus the new tip watermark to persist. Bodies are
/// fetched on demand via [`CacheStorage::fetch_event`] — mirrors the
/// `query_events` + `get_stored_event` two-step and avoids loading bodies a
/// gear never reads.
#[derive(Debug, Clone)]
pub struct GroupDiff {
    pub added: Vec<GroupEventId>,
    pub removed: Vec<GroupEventId>,
    pub watermark: Watermark,
}

/// Event-log watermark: how far a consumer has walked a group's changelog —
/// the number of accepted slots and of `removed` entries it has seen.
/// `Watermark::default()` means "from the beginning".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Watermark {
    pub added: u64,
    pub removed: u64,
}

/// A group-bound async read view over [`CacheStorage`]. It binds the group
/// once (at the gear-run boundary) so CPU algorithms below stay group-agnostic:
/// they see only `&GroupStore` plus a slot, never a `LocGroupId`.
///
/// The methods are `async` and delegate unboxed to the backend (no extra
/// allocations beyond what the backend does itself).
pub struct GroupStore<'a, R: RuntimeTypes> {
    storage: &'a CacheStorage<R>,
    group: LocGroupId,
    _r: PhantomData<fn() -> R>,
}

impl<'a, R: RuntimeTypes> GroupStore<'a, R> {
    #[must_use]
    pub fn new(storage: &'a CacheStorage<R>, group: LocGroupId) -> Self {
        Self {
            storage,
            group,
            _r: PhantomData,
        }
    }

    /// The bound group (one gear = one group).
    #[must_use]
    pub fn group(&self) -> LocGroupId {
        self.group
    }

    /// The event body at `slot` in this group.
    pub async fn stored_event(&self, slot: GroupEventId) -> Option<StoredEvent<R::Body>> {
        self.storage.fetch_event(self.group, slot).await
    }

    /// The sender's local user.
    pub async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.storage.sender_user(sid).await
    }

    /// The sender's public key.
    pub async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.storage.sender_pk(sid).await
    }

    /// The data record `(DataId, content)` behind a local id.
    pub async fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.storage.fetch_data(did).await
    }
}
