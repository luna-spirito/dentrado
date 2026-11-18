//! The cache-backed layer 1–2 backend.
//!
//! The page cache is the complete materialization of localization, the event
//! shards, content data, and the gear views. A journaled backend pairs it with
//! the append-only log: the journal is the source of truth, the cache is a
//! checkpoint, and startup replays only the tail past `tables::Meta`'s
//! cursor. The RAM backend runs the very same code over
//! [`MemDisk`](dentrado_cache::disk::MemDisk) and simply has no journal, so
//! nothing survives the process.
//!
//! ## Ordering
//!
//! An operation appends its journal record first (buffered), then stages the
//! rows it will make visible. Localization rows enter the RAM delta at once
//! and are served from it; events stay pending and are not served at all until
//! [`CacheStorage::commit_log`] writes them to the cache. Durability is the
//! same call: the journal is synced, and *then* the cache's applied cursor
//! advances in one atomic session with the rows it covers.
//! [`CacheStorage::flush`] does the same and additionally flushes the page
//! store. A crash therefore loses cache writes past the last applied cursor,
//! which replay re-derives from the journal; it can never lose a journal record
//! the cache claimed to have.
//!
//! ## Idempotence
//!
//! Every apply is idempotent, so replay may restart after a crash and re-apply
//! records already in the image. A conflicting observation is a loud
//! `todo!()` until observations can be ranked by authority.
//!
//! ## Layout
//!
//! - `codec` — key and value encodings;
//! - `tables` — table ids, layout, meta;
//! - `rows` — a staged row and the sink the fold writes through;
//! - `delta` — the RAM coordination layer: counters, delta, per-key queues;
//! - `fold` — the one record → rows interpretation;
//! - `events` — committed event reads and replay;
//! - `live` — loc allocation, journal appends, event storage, commit;
//! - `replay` — journal records into those transitions;
//! - `flights` — per-key single-flight dispatch for the opened backend;
//! - `open` — startup replay and the materialized image;
//! - `storage` — the handle, lazy open, durability.

pub(crate) mod codec;
pub(crate) mod tables;
mod rows;
mod delta;
mod fold;
mod events;
mod live;
mod replay;
mod flights;
mod open;
mod storage;
#[cfg(test)]
mod tests;

pub use storage::CacheStorage;
