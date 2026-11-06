#![feature(impl_trait_in_assoc_type, never_type)]
//! Atomic cache store — a direct port of the frozen Lean model
//! `lean/storage-model` (spec `thinking/sep22-atomic-no-rc.md`).
//!
//! The model's effect layer (the `Sched` runner, the world's one-shot
//! channels) is dissolved into infrastructure: an atomic `Op.state`
//! section is a short borrow of the store's single `RefCell` with no
//! await inside (the store is single-threaded, like the model's
//! scheduler), `Send/Recv` are `futures::channel::oneshot`, a process
//! crash is fault injection on [`MemDisk`] (`Fault::At` ≈ a crash at
//! model tick n, `torn_by` tears uncommitted pages into `0xAA` noise).
//! Disk operations go straight over the pool's arena slots (compio
//! buffers) — no copies on load or save.
//!
//! Layout: [`page`] — the page and its byte primitives (every page
//! kind's codec lives with its machine); [`disk`] — the [`Storage`]
//! trait and its implementations (memory with fault injection, an OS
//! file on compio); [`blockcache`] — the page cache (the arena of
//! slots, the flight registry, the queues); [`crate::core`] — the
//! store's flush-agnostic core: the machines (`freelist` — the
//! two-pool page dispenser, `ebr` — deaths), the drafts arena and the
//! named tables (each a B-tree plus its delta; the set unions at open,
//! tables enumerate and drop), and the page codecs beside them (`val`,
//! `tree`, `sb`, `freelist::chunk`); [`persist`] — the persistence
//! overlay (the round agent and its mailbox, the generation, the
//! poison flag), the flush round, recovery; [`store`] — the async
//! surface: drafts and the cache, the read guard. Semantics and the
//! derived loop bounds live in the doc comments — ported from the
//! model.
//!
//! The store is a component, not a trust boundary: any error or panic
//! inside the plumbing poisons it terminally (every semantic op refuses
//! with [`StoreError::Poisoned`] while the io drains), and the caller
//! degrades — see [`persist`] and [`StoreError`].
//!
//! The crate's `unsafe` is confined to `blockcache`: two page-lease
//! fabrications (`&'static (mut) Page` from the arena's cells — the
//! uniqueness vouched for by the slot state every access asserts, the
//! lifetime by the `Rc` the lease holds) and one uninit view of
//! initialized bytes. Everything else is safe code over those leases.

#![deny(unsafe_code)]

mod blockcache;
pub mod core;
pub mod disk;
mod error;
pub mod page;
mod persist;
pub mod schema;
pub mod storage;

pub use crate::core::freelist::ChainEntry;
pub use crate::core::sb::Sb;
pub use crate::core::{CacheValue, DraftId};
pub use disk::{Fault, FsDisk, IsFilesystem, MemDisk, MemFile};
pub use error::StoreError;
pub use page::{Page, PageId};
pub use schema::{NO_DEPS, NO_DEPS_PAGE, NoDeps, PageSchema, TableSchema, TableSpec};
pub use storage::{DebugState, INLINE_MAX, Rec, Storage, Violation};
