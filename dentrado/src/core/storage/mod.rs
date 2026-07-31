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

pub use in_memory::InMemoryStorage;

use std::{fmt::Debug, future::Future, io, marker::PhantomData};

use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
    },
    types::{
        DataId, DataVerifyError, GlobalResolver, GroupEventId, LocDataId, LocGroupId, LocMsgTypeId,
        LocSenderId, LocUserId, SenderPk, UserId,
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

/// Page size of the write-once page substrate ([`AlignedPage`]). Matches the
/// on-disk direct-IO page (see `fs`), so a page is one IO unit.
pub const PAGE_SIZE: usize = 4096;

/// Positional id of a write-once (immutable) page. Identity is by position, not
/// by content hash. A raw `PageId` carries **no** reference-counting: it is
/// what [`Storage::read_page`] takes, and it is the key a gear cache (via
/// [`CacheSer::page_roots`]) or a parent page's `refs` roots. Dormant today: the
/// substrate for future disk-spilling persistent maps; unused by dentrado yet.
/// Page content is raw bytes by design — its layout is defined by the spill-map
/// format, not a typed domain object.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageId(pub u64);

/// Owned, reference-counted handle to a page — what [`Storage::write_page`]
/// hands back and [`Storage::drop_page_handle`] consumes. While a handle (or a
/// rooting edge: a parent page's `refs`, or a cache's `page_roots`) exists, the
/// page is live; dropping the last such reference cascade-frees it. Wraps the
/// positional [`PageId`] so the runtime can still [`Storage::read_page`] by id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PageHandle(pub PageId); // TODO: Doesn't call drop right now, causing leak.

/// A single aligned page — the write-once storage unit. `#[repr(align(4096))]`
/// so it can later be handed to direct IO verbatim; owned by value (move on
/// write, clone on read).
#[repr(align(4096))]
#[derive(Clone, Debug)]
pub struct AlignedPage(pub [u8; PAGE_SIZE]);

/// (De)serialization + page-rooting interface a gear cache (`R::GearCache`)
/// implements so a [`Storage`] backend can persist it and track the pages it
/// roots.
///
/// `page_roots` is the only member the storage layer calls today: a cache
/// declares the pages it directly roots, and storage reference-counts them
/// (incref on [`Storage::put_cache`] **before** decref of the superseded
/// cache's roots, so a shared root never dips to zero mid-swap).
/// TODO: Add <GearId> parameter to make key-aware deserialization
pub trait CacheSer {
    /// The pages this cache directly roots.
    fn page_roots(&self) -> &[PageId];
}

impl CacheSer for () {
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
pub trait Storage<R: IsRuntime>: GlobalResolver /* TODO: Remove this, unviable. */ + 'static {
    type Watermark: Clone + Debug + Default + 'static;

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
        since: Self::Watermark,
    ) -> impl Future<Output = GroupDiff<Self::Watermark>>;

    // ── Layer 3: gear cache + страницы (CoW, reference counting) ─────────

    /// Получить cache по id. Десериализует и возвращает typed cache.
    fn get_cache(&self, gear: &R::GearId) -> impl Future<Output = Option<R::GearCache<Self::Watermark>>>;

    /// Перезаписать cache. Storage сам извлекает корни старой и новой записи
    /// через [`CacheSer::page_roots`]: incref новых ДО decref старых, чтобы
    /// общий корень никогда не проваливался в ноль между проходами.
    fn put_cache(&self, gear: R::GearId, cache: R::GearCache<Self::Watermark>) -> impl Future<Output = ()>;

    /// Записать новую страницу, вернуть owned handle. `refs` — дочерние
    /// страницы (их refcount инкрементится). Страница immutable.
    fn write_page(&self, data: AlignedPage, refs: &[PageId]) -> impl Future<Output = PageHandle>;

    /// Прочитать страницу по id. Паника на несуществующем id — чтение
    /// не-живой страницы это баг вызывающего (live-страница не может быть
    /// освобождена, пока на неё есть handle или ссылка).
    fn read_page(&self, id: PageId) -> impl Future<Output = AlignedPage>;

    /// Вызывается рантаймом при дропе handle. Дочерние refs страницы storage
    /// знает сам (они записаны вместе со страницей) — каскадный decref.
    fn drop_page_handle(&self, handle: PageHandle) -> impl Future<Output = ()>;

    // ── Durability ────────────────────────────────────────────────────────

    /// Commit point: после успешного `flush` всё, записанное до него,
    /// переживает крах процесса. Порядок внутри: layer 1 → layer 2 →
    /// gear cache → страницы.
    fn flush(&self) -> impl Future<Output = io::Result<()>>;
}

/// Группа-bound асинхронный read-view над бекендом [`Storage`]. Связывает группу
/// один раз (на границе запуска gear), чтобы CPU-алгоритмы ниже оставались
/// group-agnostic: они видят только `&GroupStore` + slot, никогда `LocGroupId`.
///
/// Методы `async` и делегируют unboxed в [`Storage`] (без аллокаций сверх тех,
/// что делает сам бекенд). Заменяет прежнюю синхронную пару
/// `loc_ctx::GroupStore`/`EventStore`, теперь когда бекенд — `async`.
pub struct GroupStore<'a, R: IsRuntime, S: Storage<R>> {
    storage: &'a S,
    group: LocGroupId,
    _r: PhantomData<fn() -> R>,
}

impl<'a, R: IsRuntime, S: Storage<R>> GroupStore<'a, R, S> {
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
