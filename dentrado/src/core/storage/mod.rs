//! Per-core, single-threaded storage contract — **typed over `R`**.
//!
//! `Storage<R>` describes, directly and with full type safety, the operations
//! dentrado performs against persistent (restart-surviving) state. No byte
//! keys, no serialization at this level: each operation is a named, typed
//! dentrado interaction, and the in-memory implementation is the `HashMap`s
//! from `loc_ctx.rs` behind the trait — zero overhead.
//!
//! ## Why typed (and generic over `R`)
//!//! A bytes/generic-KV trait would force even the in-memory backend to serialize
//! every `SenderPk`/`UserId`/`R::Group`/`R::Data`/`R::Body` on every op —
//! pointless work in RAM and a source of key/value bugs. Since dentrado ships
//! exactly one disk backend (which knows `R` anyway), the "backend reusable
//! without `R`" flexibility is worthless; a typed trait gives zero-overhead
//! in-memory, a self-documenting spec, and type safety.
//!
//! ## Durability of the gear cache
//!
//! The gear cache (`get_cache` / `put_cache` below) is restart-surviving state
//! on the same footing as everything else in this trait: a disk backend
//! persists it via `flush` so a gear resumes from its old working state after
//! a restart (in particular, the event watermark it stores — losing it would
//! mean re-processing the group's entire history on every cold start, which
//! is correct but O(history) slow, and for oracle gears means re-hitting
//! outside systems). It is *recomputable* from the event log (so correctness
//! holds even when a backend cannot persist it), but that is an availability
//! fallback, not a reason to skip persistence where it's feasible.
//!
//! (It is on this trait at all — rather than a separate field on `Core` — so
//! that `Core<R, S: Storage>` reaches it through a single `storage` field.
//! It can be `async` because the `Drop`-driven eviction path no longer touches
//! it: `evict_gear` leaves the cache intact and it is keyed by the stable
//! `R::GearId`.)
//!
//! ## Flush ordering
//!
//! A backend `flush` must commit in this order so that, after a crash, every
//! upper layer is interpretable in terms of the layers below it:
//!
//! 1. **localization** — every durable event's `LocSenderId` resolves;
//! 2. **event log** — the dedup index + log itself commit atomically;
//! 3. **gear cache** — carries each gear's event watermark, so it must land
//!    strictly *after* the log (a watermark surviving past its events would
//!    point past the tail);
//! 4. **pages** (when used) — the write-once substrate for spill maps.
//!
//! Everything in this trait is restart-surviving state. A *RAM-only* backend
//! like [`InMemoryStorage`](crate::core::storage::in_memory::InMemoryStorage)
//! loses it all on process death — but that is a property of the backend
//! (equally true of its events and localization), not of any one layer here.
//!
//! ## Concurrency
//!
//! All methods take `&self` and return `impl Future`: backends are
//! interior-mutable (mirroring `fs::Fs`) so no `RefCell` borrow is held across
//! an `.await` on the caller side. `!Send` by construction; used via generics
//! (monomorphized, unboxed futures), not `dyn`.
//!
//! ## Integration status (unblocked)
//!
//! `Core` is ready to be wired through this trait. The two `Drop`-driven
//! obstacles that previously forced localization to stay synchronous are now
//! resolved:
//!
//! 1. **Gear cache** is keyed by `R::GearId` and no longer touched by
//!    `evict_gear`, so the sync `Drop for Subscription` path never reaches
//!    storage on its behalf.
//! 2. **Subscription stop** carries only an opaque session token
//!    (`InterCoreMsg::StopSubscription { session, from_core }`), so
//!    `evict_gear` → `send_stop` no longer reads localization to build a wire
//!    message. The entire `Drop` path is now pure RAM + a sync `mpsc::send`.
//!
//! What remains for integration is mechanical, all of it already inside async
//! tasks (`run_loc_gear_task`, `handle_intercore_msg` spawns, `run_gear`):
//! make `WireLocCtxMerger`, `EventContext`, and the `EventStore` read paths
//! `async`; lift `LocCtx`'s fields behind `Storage<R>` (with `InMemoryStorage<R>`
//! as the 1:1 RAM backend); then drop the now-redundant `EventContext` trait.
//! No new sync↔async boundary is introduced.

pub mod in_memory;

use std::{fmt::Debug, future::Future, io};

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
    },
    types::{
        DataId, DataVerifyError, GroupEventId, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId,
        LocUserId, SenderPk, UserId,
    },
};

/// Changes to a group since a watermark: ids of events added and ids of events
/// superseded/removed, plus the new tip watermark to persist. Bodies are
/// fetched on demand via [`Storage::fetch_event`] — mirrors the current
/// `query_events` + `get_stored_event` two-step and avoids loading bodies a
/// gear never reads.
#[derive(Debug, Clone)]
pub struct GroupDiff<W> {
    pub added: Vec<GroupEventId>,
    pub removed: Vec<GroupEventId>,
    pub watermark: W,
}

/// Positional, write-once (immutable) page handle. "Modify" = allocate a new
/// page. Identity is by position, not by content hash. Dormant: the substrate
/// for the future disk-spilling persistent maps; unused by dentrado today.
/// Page content is raw bytes by design — its layout is defined by the spill-map
/// format, not a typed domain object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageId(pub u64);

/// A value that may directly root (pin) storage pages. Storage
/// reference-counts the pages each value declares, so they stay alive while
/// reachable and are reclaimed (cascading) once not.
///
/// The point of this trait: a gear cache (`R::GearCache`) that *is* a
/// spill-map already knows the pages it roots — they are part of the cache,
/// not separate data. So storage asks the cache for them (`page_roots`)
/// instead of taking a redundant `roots` argument on [`Storage::put_cache`].
/// A plain in-memory cache that holds no pages returns `&[]`.
pub trait PageRooted {
    /// The pages this value directly roots.
    fn page_roots(&self) -> &[PageId];
}

impl PageRooted for () {
    fn page_roots(&self) -> &[PageId] {
        &[]
    }
}

/// Typed physical storage for one core. `Watermark::default()` means
/// "from the beginning".
///
/// The localization allocators are idempotent: allocating an already-known
/// user/sender/group/data returns the existing `Loc*Id`. Each also maintains the
/// reverse lookup its sibling getter reads. Allocation of a fresh id uses an
/// internal monotonic counter (NOT a row count) so that future sender eviction
/// cannot alias a live id.
pub trait Storage<R: IsRuntime> {
    type Watermark: Clone + Debug + Default;

    // ── localization: idempotent allocators ───────────────────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId>;
    fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>)
    -> impl Future<Output = LocSenderId>;
    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId>;
    /// Content-addressed: verifies `content` against `data_id.hash` (resolving
    /// embedded `LocDataId` references via this same store) before recording.
    fn mk_data(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> impl Future<Output = Result<LocDataId, DataVerifyError>>;

    // ── localization: reverse lookups ─────────────────────────────────────

    fn user_by_local(&self, lid: LocUserId) -> impl Future<Output = Option<UserId>>;
    fn sender_user(&self, sid: LocSenderId) -> impl Future<Output = Option<LocUserId>>;
    fn sender_pk(&self, sid: LocSenderId) -> impl Future<Output = Option<SenderPk>>;
    fn find_data(&self, data_id: &DataId) -> impl Future<Output = Option<LocDataId>>;
    /// Read a previously-stored data payload by local id.
    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<R::Data>>;

    // ── event log (owns its dedup index internally) ───────────────────────

    /// Append an event to `group`'s shard, deduping on `(sender, tx_id)` (the
    /// group is the shard, so it is not part of the dedup key nor of the
    /// payload — see [`StoredEvent`]). `None` ⇒ the event was stale (an
    /// at-least-as-fresh one exists) and was not stored; otherwise `old` is the
    /// slot superseded, if any.
    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<StoreResultSuccess>>;

    /// Random access by slot within `group`'s shard.
    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>>;

    /// Ids added to / superseded in `group` since `since`, plus the new tip.
    fn diff_group(
        &self,
        group: LocGroupId,
        since: Self::Watermark,
    ) -> impl Future<Output = GroupDiff<Self::Watermark>>;

    // ── gear cache (restart-surviving working state — see module docs) ────
    //
    // Per-gear `R::GearCache`, keyed by the stable `R::GearId`. Typed
    // end-to-end — zero serialization in `InMemoryStorage`. Restart-surviving
    // (persisted by `flush`, strictly after the event log, so its event
    // watermark never outlives the events it indexes) AND persistent across
    // eviction/reactivation within a run (untouched by `evict_gear`): a
    // returning gear resumes from its old working state either way.
    //
    // Page rooting is *not* a separate argument: a cache declares the pages it
    // roots via [`PageRooted::page_roots`], so storage derives them from the
    // cache itself (and re-derives the superseded cache's roots to decref).
    fn get_cache(&self, gear: &R::GearId) -> impl Future<Output = Option<R::GearCache>>;
    fn put_cache(&self, gear: R::GearId, cache: R::GearCache) -> impl Future<Output = ()>;

    // ── pages: write-once substrate (dormant; for spill maps) ─────────────
    //
    // A page lives only while reachable: from a gear cache's `roots` (above)
    // and/or from another page's `refs` (here). `refs` declares this page's
    // direct children; storage increfs them on write and cascade-decrefs on
    // reclaim. A freshly-written page starts unreferenced — it becomes live
    // only once a `put_cache` (or a parent page) references it. Pages orphaned
    // by a crash mid-construction are reclaimed at the next `flush`.
    fn write_page(&self, data: &[u8], refs: &[PageId]) -> impl Future<Output = PageId>;
    fn read_page(&self, id: PageId) -> impl Future<Output = Option<Box<[u8]>>>;
    fn drop_page(&self, id: PageId);

    // ── durability (localization → event log → gear cache → pages, internally)

    fn flush(&self) -> impl Future<Output = io::Result<()>>;
}
