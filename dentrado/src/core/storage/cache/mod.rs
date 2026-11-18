//! The cache-backed layer 1–2 backend.
//!
//! The page cache is the complete materialization of localization, the event
//! shards, content data, and the gear views. A journaled backend pairs it with
//! the append-only log: the journal is the source of truth, the cache is a
//! checkpoint, and startup replays only the tail past [`tables::Meta`]'s
//! cursor. The RAM backend runs the very same code over
//! [`MemDisk`](dentrado_cache::disk::MemDisk) and simply has no journal, so
//! nothing survives the process.
//!
//! ## Ordering
//!
//! An operation appends its journal record first (buffered), then commits the
//! cache state. Durability is [`IsStorage::commit_log`]: the journal is synced,
//! and *then* the cache's applied cursor advances. [`IsStorage::flush`] does
//! the same and additionally flushes the page store. A crash therefore loses
//! cache writes past the last applied cursor, which replay re-derives from the
//! journal; it can never lose a journal record the cache claimed to have.
//!
//! ## Idempotence
//!
//! Every apply is idempotent, so replay may restart after a crash and re-apply
//! records already in the image. A conflicting observation is a loud
//! `todo!()` until observations can be ranked by authority.
//!
//! ## Layout
//!
//! - [`codec`] — key and value encodings;
//! - [`tables`] — table ids, layout, meta;
//! - [`ops`] — the idempotent transitions shared by live writes and replay;
//! - [`sink`] — replay's destination over the cache;
//! - [`live`] — loc allocation, journal appends, event storage;
//! - [`storage`] — the handle, lazy open, startup replay, durability;
//! - [`backend`] — the [`IsStorage`] implementation.
//!
//! [`IsStorage`]: crate::core::storage::IsStorage

pub(crate) mod codec;
pub(crate) mod tables;
mod ops;
mod sink;
mod live;
mod storage;
mod backend;
#[cfg(test)]
mod tests;

pub use storage::{CacheStorage, InMemoryStorage};
