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
//! 1. **Gear views** are keyed by `R::GearId` and never touched by
//!    `evict_gear`, so the sync `Drop for Subscription` path never reaches
//!    storage on their behalf.
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

pub mod view;
pub mod in_memory;
pub mod paged;

pub use in_memory::InMemoryStorage;
pub use paged::PagedStorage;
pub use view::{DiskView, GearView, RamView};

use std::{fmt::Debug, future::Future, io, marker::PhantomData};

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{StoreResultSuccess, StoredEvent},
    },
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, LocDataId, LocGroupId, LocMsgTypeId,
        LocSenderId, LocUserId, SenderPk, UserId,
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
/// the counts of `added`/`removed` entries seen. `Watermark::default()` means
/// "from the beginning".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Watermark(pub u64, pub u64);

/// Typed physical storage for one core. `Watermark::default()` means
/// "from the beginning".
///
/// The localization allocators are idempotent: allocating an already-known
/// user/sender/group/data returns the existing `Loc*Id`. Each also maintains the
/// reverse lookup its sibling getter reads. Allocation of a fresh id uses an
/// internal monotonic counter (NOT a row count) so that future sender eviction
/// cannot alias a live id.
pub trait IsStorage<R: RuntimeTypes>: GlobalResolver /* TODO: Remove this, unviable. */ + 'static {

    // ── Layer 1: localization, идемпотентные аллокаторы ──────────────────

    fn mk_loc_user(&self, uid: UserId) -> impl Future<Output = LocUserId>;
    /// То же для сендеров; при наличии `uid` пользователь тоже регистрируется.
    fn mk_loc_sender(&self, pk: SenderPk, uid: Option<UserId>)
    -> impl Future<Output = LocSenderId>;
    /// То же для `R::Group`.
    fn mk_loc_group(
        &self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> impl Future<Output = LocGroupId>;
    /// То же для content-addressed `R::Data`; `data_id.hash` проверяется
    /// против `R::Data::global_hash`.
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
    /// Прочитать ранее сохранённый payload по локальному id (вместе с его
    /// content-addressed `DataId` — запись целиком: gear-read API и билдер
    /// wire-контекста получают `DataId` и контент одним чтением).
    fn fetch_data(&self, did: LocDataId) -> impl Future<Output = Option<(DataId, R::Data)>>;

    // ── Layer 2: события, могут ссылаться на объекты layer 1 ─────────────

    /// Добавить событие в шард группы с dedup по `(sender, tx_id)`.
    /// `None` ⇒ изменение не применено; иначе `old` — вытесненный слот.
    /// NOTE: побеждает СТАРШЕЕ (по `(timestamp, source_node)`) наблюдение.
    fn store_event(
        &self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> impl Future<Output = Option<StoreResultSuccess>>;

    /// Произвольный доступ по слоту внутри шарда группы.
    fn fetch_event(
        &self,
        group: LocGroupId,
        slot: crate::types::GroupEventId,
    ) -> impl Future<Output = Option<StoredEvent<R::Body>>>;

    /// Ids, добавленные/вытесненные в `group` с момента `since`, плюс новый tip.
    fn diff_group(
        &self,
        group: LocGroupId,
        since: Watermark,
    ) -> impl Future<Output = GroupDiff>;

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

    /// Commit point: после успешного `flush` всё, записанное до него,
    /// переживает крах процесса. Порядок внутри: layer 1 → layer 2 →
    /// gear view.
    fn flush(&self) -> impl Future<Output = io::Result<()>>;
}

/// Группа-bound асинхронный read-view над бекендом [`Storage`]. Связывает группу
/// один раз (на границе запуска gear), чтобы CPU-алгоритмы ниже оставались
/// group-agnostic: они видят только `&GroupStore` + slot, никогда `LocGroupId`.
///
/// Методы `async` и делегируют unboxed в [`Storage`] (без аллокаций сверх тех,
/// что делает сам бекенд). Заменяет прежнюю синхронную пару
/// `loc_ctx::GroupStore`/`EventStore`, теперь когда бекенд — `async`.
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

    /// Привязанная группа (один gear = одна группа).
    #[must_use]
    pub fn group(&self) -> LocGroupId {
        self.group
    }

    /// Тело события по слоту в этой группе.
    pub async fn stored_event(&self, slot: GroupEventId) -> Option<StoredEvent<R::Body>> {
        self.storage.fetch_event(self.group, slot).await
    }

    /// Локальный user сендера.
    pub async fn sender_user(&self, sid: LocSenderId) -> Option<LocUserId> {
        self.storage.sender_user(sid).await
    }

    /// Публичный ключ сендера.
    pub async fn sender_pk(&self, sid: LocSenderId) -> Option<SenderPk> {
        self.storage.sender_pk(sid).await
    }

    /// Запись данных `(DataId, content)` по локальному id.
    pub async fn data(&self, did: LocDataId) -> Option<(DataId, R::Data)> {
        self.storage.fetch_data(did).await
    }
}
