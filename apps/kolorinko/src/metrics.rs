//! Runtime observability: three numbers, written per core, aggregated when
//! logged.
//!
//! Every core writes only to its own [`Metrics`] instance (and the engine
//! only to its own [`CoreStats`]) — relaxed atomics on per-core data, so
//! recording never bounces a cache line between cores and never allocates.
//! [`log_loop`], spawned once, periodically sums all cores into a single
//! `info!` line; that aggregation is the only cross-core read, off every hot
//! path.
//!
//! - `subs_active` — live WebTransport subscriptions on the core (a gauge,
//!   held by each `subscription_stream` for its whole life).
//! - `gear_running` — gear computations hanging on the core (a gauge, fed by
//!   the engine itself: every run, whatever kicked it, is one background
//!   task — see [`CoreStats`]).
//! - `sub_first` — `Subscribe` frame → first response (push or hash-skip):
//!   the server-side share of what a client waits for its subscription's
//!   first answer (a log-bucket histogram, µs, 25% bucket resolution).
//!
//! Filter for the aggregate line: `RUST_LOG=kolorinko::metrics=info`.

use std::num::NonZero;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use compio::time::sleep;
use dentrado::core::stats::CoreStats;
use log::info;

/// Aggregation cadence of [`log_loop`].
const LOG_EVERY: Duration = Duration::from_secs(60);
/// Histogram bucket count. Bucket `4·(e−2)+m+4` covers
/// `[2ᵉ·(1+m/4), 2ᵉ·(1+(m+1)/4))` µs for `e ≥ 2`; 0–3 µs map to themselves.
/// 128 buckets reach ~8.6 s, beyond which everything piles into the last.
const BUCKETS: usize = 128;

/// The per-core metrics: written only from the owning core's runtime.
pub(crate) struct Metrics {
    subs_active: AtomicI64,
    sub_first: Histo,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            subs_active: AtomicI64::new(0),
            sub_first: Histo::default(),
        }
    }
}

/// The per-core instances, indexed by `core_id` — created once by [`init`].
static CORES: OnceLock<Box<[Arc<Metrics>]>> = OnceLock::new();
/// The engine's per-core gauges, registered by each core's worker at startup.
/// A slot not yet written reads as zero: cores start within microseconds of
/// each other, the first log tick is a full [`LOG_EVERY`] later.
static ENGINE: OnceLock<Box<[OnceLock<Arc<CoreStats>>]>> = OnceLock::new();

/// Create the per-core instances, sized by core count. Runs once, before any
/// core starts serving.
pub(crate) fn init(cores: NonZero<u32>) {
    let n = cores.get() as usize;
    let _ = CORES.set(
        std::iter::repeat_with(|| Arc::new(Metrics::default()))
            .take(n)
            .collect(),
    );
    let _ = ENGINE.set(std::iter::repeat_with(OnceLock::new).take(n).collect());
}

/// Hand the engine's gauges for `core_id` to the aggregator. Called by each
/// core's worker, once.
pub(crate) fn register(core_id: u32, stats: &Arc<CoreStats>) {
    let _ =
        ENGINE.get().expect("metrics::init before serving")[core_id as usize].set(stats.clone());
}

/// The instance owned by `core_id`. Panics before [`init`] — servers start
/// only after it.
pub(crate) fn for_core(core_id: u32) -> &'static Metrics {
    &CORES.get().expect("metrics::init before serving")[core_id as usize]
}

impl Metrics {
    /// `subs_active` +1 for the guard's life — the guard *is* one live
    /// subscription.
    pub(crate) fn subs_hold(&'static self) -> SubsGuard {
        self.subs_active.fetch_add(1, Ordering::Relaxed);
        SubsGuard(&self.subs_active)
    }

    /// Record a `sub_first` sample: `Subscribe` read → first response.
    pub(crate) fn first_answer(&self, waited: Duration) {
        self.sub_first.record(waited);
    }
}

pub(crate) struct SubsGuard(&'static AtomicI64);

impl Drop for SubsGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// A fixed log-bucket histogram over durations: recording is one bucket
/// computation and one relaxed `fetch_add`; merging is per-bucket sums.
pub(crate) struct Histo {
    buckets: [AtomicU64; BUCKETS],
}

impl Default for Histo {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
        }
    }
}

impl Histo {
    /// Record one duration. No allocation, no lock.
    pub(crate) fn record(&self, d: Duration) {
        let us = d.as_micros() as u64;
        let b = if us < 4 {
            us as usize
        } else {
            let e = 63 - us.leading_zeros(); // ≥ 2, since us ≥ 4
            ((e - 2) as usize * 4 + ((us >> (e - 2)) & 3) as usize + 4).min(BUCKETS - 1)
        };
        self.buckets[b].fetch_add(1, Ordering::Relaxed);
    }

    /// Per-bucket counts **since the last call** — the window's histogram —
    /// resetting the buckets: each snapshot line describes only the interval
    /// it closes, so a slow day shows up immediately instead of being diluted
    /// by a month of fast history. Atomic with concurrent `record`s — a
    /// sample lands in this window or the next, never both, never lost.
    fn take(&self) -> [u64; BUCKETS] {
        std::array::from_fn(|i| self.buckets[i].swap(0, Ordering::Relaxed))
    }
}

/// A bucket's lower bound in µs — the honest reading of a percentile out of
/// bucketed data ("at least this").
fn lower_us(b: usize) -> u64 {
    if b < 4 {
        b as u64
    } else {
        let (e, m) = ((b - 4) / 4 + 2, (b - 4) % 4);
        (4 + m as u64) << (e - 2)
    }
}

/// Percentile over merged counts, as a bucket lower bound (µs). `None` while
/// empty.
fn percentile(counts: &[u64; BUCKETS], q: f64) -> Option<u64> {
    let total: u64 = counts.iter().sum();
    if total == 0 {
        return None;
    }
    let target = (q * total as f64).ceil() as u64;
    let mut acc = 0u64;
    for (b, &c) in counts.iter().enumerate() {
        acc += c;
        if acc >= target {
            return Some(lower_us(b));
        }
    }
    unreachable!("bucket counts sum to `total`")
}

/// µs → human.
fn fmt_us(us: u64) -> String {
    match us {
        0..=999 => format!("{us}µs"),
        1_000..=999_999 => format!("{:.1}ms", us as f64 / 1e3),
        _ => format!("{:.2}s", us as f64 / 1e6),
    }
}

/// The aggregate line: gauges summed over cores (instantaneous), `sub_first`
/// merged over the last [`LOG_EVERY`] window (buckets reset per snapshot).
fn snapshot() -> String {
    let cores = CORES.get().expect("metrics::init before serving");
    let engine = ENGINE.get().expect("metrics::init before serving");
    let subs: i64 = cores
        .iter()
        .map(|m| m.subs_active.load(Ordering::Relaxed))
        .sum();
    let gears: i64 = engine
        .iter()
        .map(|s| s.get().map_or(0, |c| c.gear_running()))
        .sum();
    let mut merged = [0u64; BUCKETS];
    for m in cores.iter() {
        for (acc, c) in merged.iter_mut().zip(m.sub_first.take()) {
            *acc += c;
        }
    }
    let q = |p: f64| {
        percentile(&merged, p)
            .map(fmt_us)
            .unwrap_or_else(|| "-".into())
    };
    let max = merged
        .iter()
        .rposition(|&c| c > 0)
        .map(|b| fmt_us(lower_us(b)))
        .unwrap_or_else(|| "-".into());
    let n: u64 = merged.iter().sum();
    format!(
        "subs_active={subs} gear_running={gears} sub_first n={n} p50={} p99={} max={max}",
        q(0.5),
        q(0.99)
    )
}

/// Aggregate all cores into one log line per [`LOG_EVERY`]. Spawned once (on
/// core 0's worker); dies with its core — but a core death kills the whole
/// `Db` anyway, so the process is going down regardless.
pub(crate) async fn log_loop() {
    loop {
        sleep(LOG_EVERY).await;
        info!(target: "kolorinko::metrics", "{}", snapshot());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Recording a spread of durations lands each in a distinct bucket whose
    /// lower bound covers it — the bucket/lower-bound pair is monotone and
    /// invertible, which is all percentiles rely on.
    #[test]
    fn histo_buckets_cover_and_climb() {
        let h = Histo::default();
        for us in [0u64, 1, 3, 4, 7, 8, 1_000, 100_000, 10_000_000] {
            h.record(Duration::from_micros(us));
        }
        let lowers: Vec<u64> = h
            .take()
            .iter()
            .enumerate()
            .filter(|(_, c)| **c > 0)
            .map(|(b, _)| lower_us(b))
            .collect();
        // Each value's bucket lower bound ≤ the value, and distinct values
        // in distinct buckets with ascending bounds (25% grid).
        assert_eq!(lowers, [0, 1, 3, 4, 7, 8, 896, 98_304, 8_388_608]);
        // Taking again yields an empty window: buckets reset per snapshot.
        assert_eq!(h.take().iter().sum::<u64>(), 0);
    }
}
