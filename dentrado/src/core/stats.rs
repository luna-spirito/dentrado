//! Per-core engine introspection gauges, readable by the host application.
//!
//! Gear computation is entirely internal to the engine — event kicks, timer
//! ticks, dependency re-kicks and client-triggered reads all converge on the
//! one background task spawned per run (`run_loc_gear_task` in
//! [`core_ctx`](crate::core::core_ctx)) — so the engine is the only layer
//! that can count it, while the host is the layer that wants to log it.
//! [`CoreStats`] is the bridge: one instance per core, written by the engine
//! with relaxed atomics only, shared to the host as an [`Arc`] for
//! cross-core aggregation from the host's own thread.
//!
//! Writes stay per-core: a core only ever touches its own instance, so no
//! cache line bounces between cores and no lock is taken.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Introspection gauges for one core. Written by the engine, read by the host
/// application for periodic aggregated logging.
#[derive(Debug, Default)]
pub struct CoreStats {
    /// Live gear-computation tasks on this core — every run, whatever kicked
    /// it (event, timer, dependency, client read), is one of these. A task
    /// parked awaiting a `Follow` input counts too: "hanging on the core" is
    /// the semantic, not "burning CPU".
    gear_running: AtomicI64,
}

impl CoreStats {
    /// The current `gear_running` value.
    #[must_use]
    pub fn gear_running(&self) -> i64 {
        self.gear_running.load(Ordering::Relaxed)
    }

    /// `gear_running` +1 for the returned guard's life, −1 on its drop.
    /// Engine-internal: take it at a task's entry — drop fires on every exit
    /// path *and* on task cancellation (a dropped future drops its state), so
    /// the gauge cannot leak.
    pub(crate) fn running_guard(self: &Arc<Self>) -> RunningGuard {
        self.gear_running.fetch_add(1, Ordering::Relaxed);
        RunningGuard(self.clone())
    }
}

/// The guard returned by [`CoreStats::running_guard`].
pub(crate) struct RunningGuard(Arc<CoreStats>);

impl Drop for RunningGuard {
    fn drop(&mut self) {
        self.0.gear_running.fetch_sub(1, Ordering::Relaxed);
    }
}
