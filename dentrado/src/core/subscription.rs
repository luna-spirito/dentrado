//! Direct, worker-side subscription to a gear's output, and the per-gear change
//! signal that backs reactivity.
//!
//! [`Epoch`] replaces `synchrony::unsync::event::Event` in `ActiveGear`.
//! It is a single-threaded monotonic epoch plus a list of parked wakers: a
//! [`Epoch::bump`] advances the epoch and wakes every waiter once; a
//! waiter that observed epoch `seen` parks until the epoch strictly differs.
//! There is no notification *token* to consume, so a waiter that wakes, reads,
//! drops, and re-registers can never double-spend a notification — the bug that
//! made `local-event` 0.1.2 ping-pong forever when two subscribers shared a gear.

use std::rc::Rc;
use std::task::{Context, Waker};

use crate::core::core_ctx::{Core, GearKey};
use crate::core::gear::{GearResult, IsRuntime};
use crate::core::storage::Storage;

/// A minimal single-threaded change signal: a monotonic epoch plus the wakers of
/// everyone parked waiting for the next change. Lives inside `ActiveGear` and is
/// driven entirely under the core's `inner` borrow, so it needs no atomics.
#[derive(Debug)]
pub(crate) struct Epoch {
    epoch: u64,
    wakers: Vec<Waker>,
}

impl Epoch {
    pub(crate) const fn new() -> Self {
        Self {
            epoch: 0,
            wakers: Vec::new(),
        }
    }

    /// Current epoch — advanced once per [`Self::bump`].
    pub(crate) fn current(&self) -> u64 {
        self.epoch
    }

    /// Advance the epoch and wake every parked waiter exactly once (draining the
    /// list, so each registered waker fires at most once per bump).
    pub(crate) fn bump(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        for w in self.wakers.drain(..) {
            w.wake();
        }
    }

    /// Register `cx`'s waker for the next [`Self::bump`]. Dedups against an
    /// identical waker already parked, so re-polling a pending waiter (the
    /// runtime polls, sees no change, re-registers) cannot pile up clones.
    pub(crate) fn park(&mut self, cx: &Context<'_>) {
        let w = cx.waker();
        if !self.wakers.iter().any(|stored| stored.will_wake(w)) {
            self.wakers.push(w.clone());
        }
    }
}

/// RAII handle for a direct, worker-side subscription to a gear's output.
///
/// Dropping it decrements the gear's direct-subscriber count and rebalances the
/// gear (demoting it to limbo, or evicting it under pressure). Naturally
/// `!Sync` via `Rc<Core>`: it must live on the owning core's thread.
#[must_use]
pub struct Subscription<R: IsRuntime, S: Storage<R>> {
    pub(crate) core: Rc<Core<R, S>>,
    /// The arena key, stored directly (not the `R::GearId`) so `current`/`next`/
    /// `Drop` skip the `gear_index` lookup. Safe because a live `Subscription`
    /// holds `direct_subscriber_count >= 1`, so `has_interest` is true and the
    /// gear cannot be evicted (its key cannot go stale) for as long as this
    /// handle exists.
    pub(crate) key: GearKey,
}

impl<R: IsRuntime, S: Storage<R>> Subscription<R, S> {
    /// Read the gear's currently-cached output. The value is guaranteed to be
    /// present (subscribe awaits the first computation).
    #[must_use]
    pub fn current(&self) -> GearResult<R> {
        self.core
            .current_output_key(self.key)
            .expect("Subscription::current: gear has no output")
    }

    /// Wait for the next output update (the gear's [`Epoch`] bumps after
    /// each completed run / `SubscriptionUpdate`) and return the new value.
    pub async fn next(&self) -> GearResult<R> {
        let seen = self
            .core
            .change_epoch(self.key)
            .expect("Subscription::next: gear evicted while subscribed");
        self.core.wait_change(self.key, seen).await;
        self.current()
    }
}

impl<R: IsRuntime, S: Storage<R>> Drop for Subscription<R, S> {
    fn drop(&mut self) {
        if self.core.release_direct_subscriber(self.key) {
            self.core.rebalance_key(self.key);
        }
    }
}
