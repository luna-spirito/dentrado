use slotmap::{SlotMap, new_key_type};
use std::{cell::UnsafeCell, fmt::Debug, ops::Deref, ptr::NonNull, rc::Weak};

use crate::core::gear::IsRuntime;

// ── shared-output family ───────────────────────────────────────────────────
//
// A `Shared` output is neither cheap-to-clone (→ `Ship`) nor pinned to one
// core (→ `Local`): an opaque, `Sync`, potentially large value that many
// consumers — on one core or several — read **by reference**, not by copy.
//
// The immutable payload ([`SharedData`]) and the cross-core refcount are kept
// in **separate** allocations on purpose: the refcount lives in an owner-local
// generational [`SharedArena`], so bumping/decrementing it never dirties the
// cache line(s) the payload occupies. Foreign cores — which only ever read the
// payload — thus never see a cache invalidation from refcount churn.
//
// Refcounting is **two-level**, so an individual clone or drop never leaves the
// core it happens on (no per-handle inter-core message):
//
//   - [`SharedArena`] slot `xcount` (cross-core count, owner-local, never sent)
//     = the number of cores that currently hold ≥1 local handle. The owner
//     bumps it *once*, right before shipping the payload pointer to another
//     core; a core that has dropped its last local handle tells the owner to
//     decrement (one `SharedUnref` per core, not per handle). `xcount == 0`
//     ⟹ retire the arena slot *and* reclaim the payload.
//
//   - [`SharedLocal`] `lcount` (local count — one cell per `(core, allocation)`
//     pair, never shared across a thread) = the number of [`Shared`] handles on
//     *this* core pointing at the allocation. `Clone`/`Drop` of a handle bump /
//     decrement it directly — a non-atomic field write on this core's thread.
//     `lcount == 0` ⟹ the core releases its cross-core claim (owner: dec the
//     arena `xcount` directly; foreign: one `SharedUnref` to the owner) and
//     frees the `SharedLocal` cell.
//
// So:
//   - local clone/drop: one non-atomic `lcount` write — no messages, ever;
//   - a whole core losing interest: one `SharedUnref` (not one per handle);
//   - the payload itself is never copied across cores.
//
// `Shared` reaches its core's send-plumbing through a `Weak<dyn SharedBus>`
// (kept `R`-free: the key it carries is type-erased, so neither `R` nor `S`
// leaks into the trait object). `Weak`, not `Rc`, breaks the
// `Core → ActiveGear → Shared → Core` cycle, and `std::rc::Weak::upgrade` is
// non-atomic.
//
// The raw-pointer/`Sync` plumbing below is the only `unsafe` in the crate; the
// crate-level `#![deny(unsafe_code)]` is lifted on exactly these items. The
// unref-message protocol that upholds it lives in `core_ctx` and is
// `unsafe`-free — it only calls the safe methods exposed here.

/// The immutable payload of a shared output: written once, before any sharing,
/// thereafter read-only on every core. Lives in its **own** heap allocation,
/// deliberately separated from any refcount, so cross-core refcount mutation
/// on the owner never dirties this cache line. The only `unsafe` here is the
/// `Sync` impl — sound because nothing mutates the value after construction.
pub(crate) struct SharedData<R: IsRuntime>(pub(crate) R::GearOutShared);

#[allow(unsafe_code)]
// SAFETY: written once before sharing; thereafter immutable across all cores.
unsafe impl<R: IsRuntime> Sync for SharedData<R> {}

impl<R: IsRuntime> Debug for SharedData<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&self.0, f)
    }
}

impl<R: IsRuntime> SharedData<R> {
    /// Box the payload and return a leaked pointer; [`SharedArena::dec`] hands
    /// it back to [`SharedData::reclaim`] when the cross-core count hits zero.
    pub(crate) fn new(v: R::GearOutShared) -> NonNull<Self> {
        NonNull::from(Box::leak(Box::new(Self(v))))
    }

    /// Reclaim a payload leaked by [`SharedData::new`]. The arena confirms zero
    /// outstanding cross-core claims before handing the pointer here, and this
    /// is called only after the arena's `RefCell` borrow is released, so the
    /// payload's own `Drop` (the opaque `R::GearOutShared`) can never reenter
    /// the arena.
    #[allow(unsafe_code)]
    pub(crate) fn reclaim(data: NonNull<Self>) {
        // SAFETY: `data` came from `new` (a `Box::leak`); the caller guarantees
        // no live handle remains.
        unsafe {
            drop(Box::from_raw(data.as_ptr()));
        }
    }
}

new_key_type! {
    /// Opaque, `R`-free generational handle into the owner core's [`SharedArena`].
    /// Crosses cores inside `SubscriptionUpdateShared` / `SharedUnref`; the owner
    /// resolves it only in its own `R`-typed arena, so no `R` leaks onto the wire.
    pub(crate) struct SharedKey;
}

/// One slot per live shared allocation: the payload back-pointer plus its
/// cross-core `xcount`. Lives inside the owner's [`SharedArena`].
struct SlotEntry<R: IsRuntime> {
    data: NonNull<SharedData<R>>,
    xcount: u64,
}

/// Owner-local generational arena of cross-core refcounts (`xcount`), backed by
/// `slotmap`'s free-list + generational key. Never sent across a core
/// (`!Send`); mutated only on the owner thread, behind a `RefCell`, so `xcount`
/// is a plain `u64` — no atomics, no interior mutability, no `unsafe`.
pub(crate) struct SharedArena<R: IsRuntime>(SlotMap<SharedKey, SlotEntry<R>>);

impl<R: IsRuntime> Debug for SharedArena<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedArena")
            .field("live", &self.0.len())
            .finish()
    }
}

impl<R: IsRuntime> SharedArena<R> {
    pub(crate) fn new() -> Self {
        Self(SlotMap::with_key())
    }

    /// Register a fresh payload with `xcount = 1` (the owner core's own claim).
    pub(crate) fn insert(&mut self, data: NonNull<SharedData<R>>) -> SharedKey {
        self.0.insert(SlotEntry { data, xcount: 1 })
    }

    /// `xcount += 1`. Owner-only, right before shipping the pointer to another
    /// core. A stale key (freed-then-reused slot) is a silent no-op — slotmap's
    /// generational key makes it safe, and the exact-count invariant makes it
    /// impossible in practice.
    pub(crate) fn inc(&mut self, key: SharedKey) {
        if let Some(entry) = self.0.get_mut(key) {
            entry.xcount += 1;
        }
    }

    /// `xcount -= 1`; on reaching zero, remove the slot (slotmap bumps its key
    /// generation automatically) and return the payload pointer for the caller
    /// to [`SharedData::reclaim`]. Otherwise `None`.
    pub(crate) fn dec(&mut self, key: SharedKey) -> Option<NonNull<SharedData<R>>> {
        let xcount = self.0.get_mut(key).map(|entry| {
            entry.xcount -= 1;
            entry.xcount
        });
        if xcount == Some(0) {
            self.0.remove(key).map(|entry| entry.data)
        } else {
            None
        }
    }
}

/// A per-core cell tracking **this core's local** refcount for one shared
/// allocation. One `SharedLocal` per `(core, allocation)` pair — all
/// [`Shared`] handles on this core for that allocation share it (Rc-like, via
/// the inline non-atomic `lcount`). `!Send`: it never crosses a thread.
struct SharedLocal(UnsafeCell<u64>);

impl SharedLocal {
    /// Allocate with `lcount = 1` (the initial handle).
    #[allow(unsafe_code)]
    fn new() -> NonNull<Self> {
        NonNull::from(Box::leak(Box::new(Self(UnsafeCell::new(1)))))
    }

    #[allow(unsafe_code)]
    fn lcount_inc(this: NonNull<Self>) {
        // SAFETY: single-threaded (this core only).
        unsafe {
            *this.as_ref().0.get() += 1;
        }
    }

    /// `lcount -= 1`. Returns `true` iff it reached zero (the caller then frees
    /// the core's cross-core claim); the cell itself is freed on zero.
    #[allow(unsafe_code)]
    fn lcount_dec(this: NonNull<Self>) -> bool {
        // SAFETY: single-threaded (this core only).
        let c = unsafe { this.as_ref().0.get() };
        unsafe {
            *c -= 1;
        }
        if unsafe { *c } == 0 {
            // SAFETY: `this` came from `SharedLocal::new` (a `Box::leak`).
            unsafe {
                drop(Box::from_raw(this.as_ptr()));
            }
            true
        } else {
            false
        }
    }
}

/// `Send` wrapper around a raw pointer to a `Sync` shared payload, for the
/// cross-core channel. Sound because the pointee is `Sync` and the owner
/// retains it until every outstanding handle is released.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RemoteShared<T: Sync + ?Sized>(NonNull<T>);
#[allow(unsafe_code)]
unsafe impl<T: Sync + ?Sized> Send for RemoteShared<T> {}
#[allow(unsafe_code)]
unsafe impl<T: Sync + ?Sized> Sync for RemoteShared<T> {}

impl<T: Sync + ?Sized> RemoteShared<T> {
    pub(crate) fn from_ptr(p: NonNull<T>) -> Self {
        Self(p)
    }
    pub(crate) fn as_ptr(&self) -> NonNull<T> {
        self.0
    }
}

/// How a [`Shared`] handle routes the release of a core's cross-core claim when
/// it is that core's *last* local handle. Implemented by `Core`, held as
/// `Weak<dyn SharedBus>` so `Shared<R>` stays free of `S`. The two methods
/// distinguish direction: an **owner** handle releases directly in the owner's
/// arena (no message); a **foreign** handle forwards a `SharedUnref` to the
/// owner over the inter-core channel. There is intentionally **no retain**: a
/// foreign core can only ever drop a claim it already holds.
pub(crate) trait SharedBus {
    /// Owner thread, last owner-local handle dropped: decrement the owner's own
    /// arena slot directly.
    fn shared_local_unref(&self, key: SharedKey);
    /// Foreign thread, last foreign handle dropped: tell `owner` to release this
    /// core's claim.
    fn shared_unref(&self, owner: u32, key: SharedKey);
}

/// A refcounted handle to a shared output. `!Send`: it lives on whichever core
/// minted it; cross-core traffic carries a [`RemoteShared`], not the handle.
///
/// `Clone`/`Drop` are **local-only** — they bump/decrement this core's
/// [`SharedLocal`] `lcount` (no messages, no atomics). Only the *last* handle on
/// a core (its `lcount` → 0) crosses a boundary: an owner-local handle
/// decrements the arena `xcount` directly; a foreign handle sends one
/// `SharedUnref` to the owner.
pub struct Shared<R: IsRuntime> {
    local: NonNull<SharedLocal>,
    data: NonNull<SharedData<R>>,
    key: SharedKey,
    bus: Weak<dyn SharedBus>,
    owner: u32,
    is_owner: bool,
}

impl<R: IsRuntime> Shared<R> {
    /// Mint the **owner-local** initial handle: a fresh [`SharedLocal`] cell
    /// (`lcount = 1`) over an arena-registered payload. Only
    /// `Core::install_produce` (on the owner) calls this.
    pub(crate) fn new_owner(
        data: NonNull<SharedData<R>>,
        key: SharedKey,
        bus: Weak<dyn SharedBus>,
        owner: u32,
    ) -> Self {
        Self {
            local: SharedLocal::new(),
            data,
            key,
            bus,
            owner,
            is_owner: true,
        }
    }

    /// Mint the **foreign** initial handle for a payload received via a
    /// `SubscriptionUpdateShared` (the owner already bumped `xcount` for this
    /// core's claim). Only `Core::shared_from_remote` calls this.
    pub(crate) fn new_foreign(
        data: NonNull<SharedData<R>>,
        key: SharedKey,
        bus: Weak<dyn SharedBus>,
        owner: u32,
    ) -> Self {
        Self {
            local: SharedLocal::new(),
            data,
            key,
            bus,
            owner,
            is_owner: false,
        }
    }

    /// The immutable payload pointer, for the owner to ship to a subscriber.
    pub(crate) fn data(&self) -> NonNull<SharedData<R>> {
        self.data
    }

    /// The arena key, paired with [`Shared::data`] in a cross-core push.
    pub(crate) fn key(&self) -> SharedKey {
        self.key
    }
}

impl<R: IsRuntime> Clone for Shared<R> {
    fn clone(&self) -> Self {
        SharedLocal::lcount_inc(self.local);
        Self {
            local: self.local,
            data: self.data,
            key: self.key,
            bus: Weak::clone(&self.bus),
            owner: self.owner,
            is_owner: self.is_owner,
        }
    }
}

impl<R: IsRuntime> Drop for Shared<R> {
    fn drop(&mut self) {
        if !SharedLocal::lcount_dec(self.local) {
            return;
        }
        // Last handle on this core: release its cross-core claim.
        let Some(bus) = self.bus.upgrade() else {
            // The owning Core is already gone — the arena (owner) or the unref
            // channel (foreign) is unreachable, so the payload leaks. Expected
            // only at process shutdown; flagged so a mid-run leak is noticed.
            // TODO
            eprintln!(
                "dentrado: warning: a Shared handle outlived its Core — leaking its \
                 allocation (i'm not dealing with it rn)"
            );
            return;
        };
        if self.is_owner {
            bus.shared_local_unref(self.key);
        } else {
            bus.shared_unref(self.owner, self.key);
        }
    }
}

impl<R: IsRuntime> Deref for Shared<R> {
    type Target = R::GearOutShared;
    #[allow(unsafe_code)]
    fn deref(&self) -> &Self::Target {
        // SAFETY: a live `Shared` holds a claim on the payload, which is
        // immutable after construction.
        unsafe { &self.data.as_ref().0 }
    }
}

impl<R: IsRuntime> Debug for Shared<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Debug::fmt(&**self, f)
    }
}
