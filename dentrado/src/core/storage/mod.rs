//! Per-core, single-threaded storage contract — **typed over `R`**.
//!
//! `Storage<R>` describes, directly and with full type safety, the operations
//! dentrado performs against persistent (restart-surviving) state. Each
//! operation is a named, typed dentrado interaction: no caller ever names a
//! byte key or serializes a value at this level.
//!
//! ## One implementation
//!
//! Both shipped backends are the same page-cache code: the journaled one
//! pairs it with an append-only log, the RAM one runs it over a memory disk.
//! The typed interface keeps that code honest and lets replay reuse the live
//! applies instead of maintaining a second, subtly different fold.
//!
//! ## Durability of the gear view
//!
//! A gear's working state is not interpreted by dentrado: the gear gets a
//! [`GearView`](view::GearView) — a flat key-value overlay scoped to its
//! stable id — and stores whatever it wants there. A disk backend persists
//! the view through `flush`; a RAM backend loses it with everything else on
//! process death. The view is recomputable from the event log, so a lost
//! view is a cold start, never a correctness break.
//!
//! ## Flush ordering
//!
//! A backend `flush` must commit in this order so that, after a crash, every
//! upper layer is interpretable in terms of the layers below it:
//!
//! 1. **localization** — every durable event's `LocSenderId` resolves;
//! 2. **event log** — the dedup index + log itself commit atomically;
//! 3. **gear view** — a gear's watermark must not survive past its events.
//!
//! Everything in this trait is restart-surviving state. A *RAM-only* backend
//! like [`InMemoryStorage`](crate::core::storage::InMemoryStorage) loses it
//! all on process death — but that is a property of the backend (equally true
//! of its events and localization), not of any one layer here.
//!
//! ## Concurrency
//!
//! All methods take `&self` and return `impl Future`: backends are
//! interior-mutable (mirroring `fs::Fs`) so no `RefCell` borrow is held across
//! an `.await` on the caller side. `!Send` by construction; used via generics
//! (monomorphized, unboxed futures), not `dyn`.
//!
//! ## Where it is used
//!
//! `Core` is generic over the backend (`Core<R, S: IsStorage<R>>`), so the same
//! gear code runs against [`InMemoryStorage`] and the journaled
//! [`PagedStorage`]. The `Drop`-driven paths stay synchronous by design: gear
//! views are never touched by `evict_gear`, and a stopped subscription carries
//! an opaque session token instead of reading localization.

pub mod store;
pub mod view;
pub mod journal;
pub mod cache;
pub mod paged;

pub use cache::{CacheStorage, InMemoryStorage};
pub use paged::PagedStorage;
pub use view::{DiskView, GearView};

use std::{fmt::Debug, future::Future, io, marker::PhantomData};

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GroupEventId, GroupRouteError, LocDataId, LocGroupId,
        LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// Changes to a group since a watermark: ids of events added and ids of events
/// superseded/removed, plus the new tip watermark to persist. Bodies are
/// fetched on demand via [`IsStorage::fetch_event`] — mirrors the current
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

/// Typed physical storage for one core. `Watermark::default()` means
/// "from the beginning".
///
/// The localization allocators are idempotent: allocating an already-known
/// user/sender/group/data returns the existing `Loc*Id`. Each also maintains the
/// reverse lookup its sibling getter reads. Allocation of a fresh id uses an
/// internal monotonic counter (NOT a row count) so that future sender eviction
/// cannot alias a live id.
pub trait IsStorage<R: RuntimeTypes>: 'static {
    // ── Layer 1: localization, idempotent allocators ──────────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId>;
    /// The same for senders; with a `uid` the user is interned too.
    fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>)
    -> impl Future<Output = LocSenderId>;
    /// The same for `R::Group`.
    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId>;
    /// The same for content-addressed `R::Data`; `data_id.hash` is verified
    /// against `R::Data::global_hash`.
    fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> impl Future<Output = Result<LocDataId, DataVerifyError>>;

    // ── Localization: reverse lookups ─────────────────────────────────────

    fn user_by_local(&self, lid: LocUserId) -> impl Future<Output = Option<UserId>>;
    fn sender_user(&self, sid: LocSenderId) -> impl Future<Output = Option<LocUserId>>;
    fn sender_pk(&self, sid: LocSenderId) -> impl Future<Output = Option<SenderPk>>;
    fn find_data(&self, data_id: &DataId) -> impl Future<Output = Option<LocDataId>>;
    /// Read a stored payload by its local id, together with its
    /// content-addressed `DataId`: the gear-read API and the wire-context
    /// builder get both in one read.
    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<(DataId, R::Data)>>;

    /// The global content hash of a value, resolving embedded loc ids through
    /// this backend's localization maps. Routing is a pure function of this
    /// hash, so every core that asks computes the same placement.
    fn global_hash<V: GlobalHash>(
        &self,
        value: &V,
    ) -> impl Future<Output = Result<[u8; 32], GroupRouteError>>;

    // ── Layer 2: события, могут ссылаться на объекты layer 1 ─────────────

    /// Add an observation to `group`'s shard, dedup'd by `(sender, tx_id)`.
    /// `None` ⇒ nothing changed (a stale copy); otherwise the shard delta.
    ///
    /// # Panics
    ///
    /// If the group already observed `(sender, tx_id)`: supersede is a
    /// `todo!()` until observations can be ranked by authority.
    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<GroupDelta>>;

    /// Arbitrary access by slot within a group's shard.
    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: crate::types::GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>>;

    /// Ids added/removed in `group` since `since`, plus the new tip.
    fn diff_group(&self, group: LocGroupId, since: Watermark) -> impl Future<Output = GroupDiff>;

    // ── Layer 3: the gear's private view ──────────────────────────────────

    /// The view type this backend hands to a gear. Disk and RAM backends
    /// differ in representation, not in contract.
    type View<'a>: GearView
    where
        Self: 'a;

    /// Open a view scoped to `gear`. The view stages writes; the runtime
    /// commits it after the gear run.
    fn gear_view(&self, gear: &R::GearId) -> impl Future<Output = Self::View<'_>>;

    // ── Durability ────────────────────────────────────────────────────────

    /// The log's durability point. Once it returns, every event appended so
    /// far survives a crash; called once per `post_events` batch before any
    /// reply.
    fn commit_log(&self) -> impl Future<Output = io::Result<()>>;

    /// The full commit point: after a successful `flush`, everything written
    /// before it survives a crash. A flush implies [`commit_log`](Self::commit_log),
    /// then persists the remaining state in dependency order — localization,
    /// the event log, the gear views.
    fn flush(&self) -> impl Future<Output = io::Result<()>>;
}

/// A group-bound async read view over a [`Storage`] backend. It binds the
/// group once (at the gear-run boundary) so CPU algorithms below stay
/// group-agnostic: they see only `&GroupStore` plus a slot, never a
/// `LocGroupId`.
///
/// The methods are `async` and delegate unboxed to the backend (no extra
/// allocations beyond what the backend does itself).
pub struct GroupStore<'a, R: RuntimeTypes, S: IsStorage<R>> {
    storage: &'a S,
    group: LocGroupId,
    _r: PhantomData<fn() -> R>,
}

impl<'a, R: RuntimeTypes, S: IsStorage<R>> GroupStore<'a, R, S> {
    #[must_use]
    pub fn new(storage: &'a S, group: LocGroupId) -> Self {
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
