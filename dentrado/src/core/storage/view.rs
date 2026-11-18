//! The per-gear view over the store: a flat key-value overlay scoped to
//! one gear.
//!
//! A gear never names the store's tables. It gets a [`GearView`] whose
//! keys are prefixed by a stable per-gear subkey, so two gears can use
//! the same key names without colliding. The view is a thin wrapper
//! around one [`Session`]: reads use the session's snapshot
//! ([`Session::get_old`]/[`Session::scan_old`]), writes are staged by
//! [`Session::put`]/[`Session::del`], and [`Session::commit`] publishes
//! the whole batch atomically.
//!
//! The base view is deliberately flat: values must fit one leaf record.
//! A gear that needs bigger values stores several records (e.g.
//! `article_latest_parsed`'s zstd blocks) or builds a chunking layer on
//! top of the raw session; the store itself does not chunk.
//!
//! Each gear **kind** gets its own table (see
//! [`RuntimeTypes::gear_kind`](crate::core::api::RuntimeTypes::gear_kind) and
//! [`GearTable`]), so a kind can be dropped whole without walking the others.
//! Per-gear isolation *within* a kind is still the key prefix.

use std::{
    hash::{Hash, Hasher},
    rc::Rc,
    sync::OnceLock,
};

use dentrado_cache::{
    Session, Storage,
    disk::{FsDisk, IsFilesystem, MemDisk},
};

use crate::core::api::{GearTable, RuntimeTypes};

/// The gear-view tables this runtime declares, at the current layout version.
pub(crate) fn gear_tables<R: RuntimeTypes>() -> Vec<GearTable<'static>> {
    R::gear_kinds()
        .iter()
        .map(|kind| GearTable::of(kind))
        .collect()
}

/// The flat store spec per table. A view value names no pages.
pub(crate) fn gear_specs(tables: &[GearTable<'_>]) -> Vec<dentrado_cache::TableSpec> {
    tables
        .iter()
        .map(|table| dentrado_cache::TableSpec::flat(table.encode()))
        .collect()
}

/// The pre-per-kind table id (`postcard(("dentrado/gear-view", 0))`). It
/// predates the `gears` namespace, so the GC needs an explicit check for it.
pub(crate) fn legacy_gear_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        postcard::to_stdvec(&("dentrado/gear-view", 0u64)).expect("a flat tuple serializes")
    })
}

/// Could one leaf hold this record? The store would refuse the whole
/// flush on `put`; the view must refuse it at the same point.
fn check_leaf(key: &[u8], value: &[u8]) {
    assert!(
        dentrado_cache::core::tree::leaf_holds(key, value),
        "gear view record ({} byte key, {} byte value) does not fit one leaf; chunk it",
        key.len(),
        value.len(),
    );
}

/// A stable per-gear subkey: `blake3` over the gear id's `Hash`.
///
/// `GearId` is only `Hash + Eq + Clone`; hashing is the one generic way
/// to turn it into bytes without adding a serialization bound to every
/// runtime. The key is a cache key, not a content address: a changed id
/// simply cold-starts the gear.
pub(crate) fn gear_key<R: RuntimeTypes>(gear: &R::GearId) -> [u8; 32] {
    let mut hasher = GearHasher(blake3::Hasher::new());
    gear.hash(&mut hasher);
    *hasher.0.finalize().as_bytes()
}

/// A deterministic `Hasher` feeding blake3. Integers are written
/// little-endian (the default `Hasher` methods use native endianness),
/// so a store copied between architectures keeps the same keys.
struct GearHasher(blake3::Hasher);

impl Hasher for GearHasher {
    fn finish(&self) -> u64 {
        let bytes = self.0.finalize();
        u64::from_le_bytes(bytes.as_bytes()[..8].try_into().expect("8 bytes"))
    }

    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn write_u8(&mut self, i: u8) {
        self.0.update(&[i]);
    }

    fn write_u16(&mut self, i: u16) {
        self.0.update(&i.to_le_bytes());
    }

    fn write_u32(&mut self, i: u32) {
        self.0.update(&i.to_le_bytes());
    }

    fn write_u64(&mut self, i: u64) {
        self.0.update(&i.to_le_bytes());
    }

    fn write_u128(&mut self, i: u128) {
        self.0.update(&i.to_le_bytes());
    }

    fn write_usize(&mut self, i: usize) {
        self.0.update(&(i as u64).to_le_bytes());
    }
}

/// The gear-facing view: keys are gear-local, the implementation adds the
/// gear prefix. Writes are staged in the underlying session and become
/// visible only at [`commit`](GearView::commit).
///
/// Reads are the **old** world: a value written earlier in the same run is not
/// visible until commit. This is deliberate — a gear reads its previous cache,
/// computes, and stages the next one.
pub struct GearView {
    kind: ViewKind,
}

enum ViewKind {
    Ram(DiskView<MemDisk>),
    Fs(DiskView<FsDisk>),
}

/// The two disks are distinct types, so every method repeats the same
/// two-arm dispatch. This is the one place that shape lives.
macro_rules! on_view {
    ($view:expr, |$v:ident| $call:expr) => {
        match &mut $view.kind {
            ViewKind::Ram($v) => $call,
            ViewKind::Fs($v) => $call,
        }
    };
}

impl GearView {
    pub(crate) fn ram(
        store: Rc<Storage<MemDisk>>,
        table: GearTable<'static>,
        prefix: Vec<u8>,
    ) -> Self {
        Self {
            kind: ViewKind::Ram(DiskView::new(store, table, prefix)),
        }
    }

    pub(crate) fn fs(
        store: Rc<Storage<FsDisk>>,
        table: GearTable<'static>,
        prefix: Vec<u8>,
    ) -> Self {
        Self {
            kind: ViewKind::Fs(DiskView::new(store, table, prefix)),
        }
    }

    /// The old value for `key`, or `None`.
    pub async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        on_view!(self, |view| view.get(key).await)
    }

    /// The old records under `prefix`, in key order. Returned keys keep the
    /// caller's prefix (only the gear prefix is stripped), so they can be
    /// passed straight back to [`del`](Self::del).
    pub async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        on_view!(self, |view| view.scan(prefix).await)
    }

    /// Stage a value. It must fit one leaf record (the view panics otherwise);
    /// the view does not chunk.
    pub fn put(&mut self, key: &[u8], value: &[u8]) {
        on_view!(self, |view| view.put(key, value))
    }

    /// Stage a deletion.
    pub fn del(&mut self, key: &[u8]) {
        on_view!(self, |view| view.del(key))
    }

    /// Publish everything staged. Idempotent; a gear may call it, and the
    /// runtime calls it again after the output is installed.
    pub fn commit(&mut self) {
        on_view!(self, |view| view.commit())
    }
}

/// A view over one [`Session`] of the store.
struct DiskView<D: IsFilesystem> {
    store: Rc<Storage<D>>,
    /// `None` between a commit and the next touch: the commit consumes the
    /// session, and a fresh one (seeing the commit) opens on demand.
    session: Option<Session<D>>,
    /// The gear kind's table, encoded once here ([`GearTable::encode`]); the
    /// key prefix only separates gears within it.
    table: Vec<u8>,
    prefix: Vec<u8>,
}

impl<D: IsFilesystem> DiskView<D> {
    fn new(store: Rc<Storage<D>>, table: GearTable<'static>, prefix: Vec<u8>) -> Self {
        Self {
            session: Some(store.session()),
            store,
            table: table.encode(),
            prefix,
        }
    }

    fn full(&self, key: &[u8]) -> Vec<u8> {
        let mut full = Vec::with_capacity(self.prefix.len() + key.len());
        full.extend_from_slice(&self.prefix);
        full.extend_from_slice(key);
        full
    }

    async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let full = self.full(key);
        session(&mut self.session, &self.store)
            .get_old(&self.table, &full)
            .await
            .expect("gear view get")
    }

    async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let full = self.full(prefix);
        let rows = session(&mut self.session, &self.store)
            .scan_old(&self.table, &full, &[])
            .await
            .expect("gear view scan");
        let strip = self.prefix.len();
        rows.into_iter()
            .map(|(key, value)| (key[strip..].to_vec(), value))
            .collect()
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        let full = self.full(key);
        check_leaf(&full, value);
        session(&mut self.session, &self.store)
            .put(&self.table, &full, value, &[], &[])
            .expect("gear view put");
    }

    fn del(&mut self, key: &[u8]) {
        let full = self.full(key);
        session(&mut self.session, &self.store)
            .del(&self.table, &full, &[])
            .expect("gear view del");
    }

    fn commit(&mut self) {
        if let Some(session) = self.session.take() {
            session.commit().expect("gear view commit");
        }
    }
}

/// The session, opening a fresh one (seeing the last commit) on demand. A free
/// function so the caller can keep borrowing `self.table`/`self.store`.
fn session<'s, D: IsFilesystem>(
    slot: &'s mut Option<Session<D>>,
    store: &Rc<Storage<D>>,
) -> &'s mut Session<D> {
    slot.get_or_insert_with(|| store.session())
}
