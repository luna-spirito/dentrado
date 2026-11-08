//! The per-gear view over the store: a flat key-value overlay scoped to
//! one gear.
//!
//! A gear never names the store's tables. It gets a [`GearView`] whose
//! keys are prefixed by a stable per-gear subkey, so two gears can use
//! the same key names without colliding. The disk view is a thin wrapper
//! around one [`Session`]: reads use the session's snapshot
//! ([`Session::get_old`]/[`Session::scan_old`]), writes are staged by
//! [`Session::put`]/[`Session::del`], and [`Session::commit`] publishes
//! the whole batch atomically. The RAM view is the same contract over a
//! `BTreeMap`, used by [`InMemoryStorage`](super::InMemoryStorage).
//!
//! The base view is deliberately flat: values must fit one leaf record.
//! A gear that needs bigger values stores several records (e.g.
//! `article_latest_parsed`'s zstd blocks) or builds a chunking layer on
//! top of the raw session; the store itself does not chunk.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    hash::{Hash, Hasher},
    rc::Rc,
    sync::OnceLock,
};

use dentrado_cache::{Session, Storage, disk::FsDisk};

use crate::core::api::RuntimeTypes;

/// The view table's identity. One table for every gear; per-gear
/// isolation comes from the key prefix. A future version can move to
/// one table per gear kind without changing the view contract.
pub(crate) fn gear_table() -> &'static [u8] {
    static TABLE: OnceLock<Vec<u8>> = OnceLock::new();
    TABLE.get_or_init(|| {
        postcard::to_stdvec(&("dentrado/gear-view", 0u64)).expect("a flat tuple serializes")
    })
}

/// The table spec the store opens with. Flat: a view value names no
/// pages.
pub(crate) fn gear_spec() -> dentrado_cache::TableSpec {
    dentrado_cache::TableSpec::flat(gear_table().to_vec())
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

/// The gear-facing view. Keys are gear-local; the implementation adds
/// the gear prefix. Writes are staged in the underlying session and
/// become visible only at [`commit`](GearView::commit).
///
/// Reads are the **old** world: a value written earlier in the same run
/// is not visible until commit. This is deliberate — a gear reads its
/// previous cache, computes, and stages the next one.
pub trait GearView {
    /// The old value for `key`, or `None`.
    async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>>;

    /// The old records under `prefix`, in key order. Returned keys keep
    /// the caller's prefix (only the gear prefix is stripped), so they
    /// can be passed straight back to [`del`](GearView::del).
    async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)>;

    /// Stage a value. It must fit one leaf record; the view does not
    /// chunk.
    fn put(&mut self, key: &[u8], value: &[u8]);

    /// Stage a deletion.
    fn del(&mut self, key: &[u8]);

    /// Publish everything staged. Idempotent; a gear may call it, and
    /// the runtime calls it again after the output is installed.
    fn commit(&mut self);
}

/// A view over one [`Session`] of the disk store.
pub struct DiskView<'a> {
    store: Rc<Storage<FsDisk>>,
    /// `None` between a commit and the next touch: the commit consumes the
    /// session, and a fresh one (seeing the commit) opens on demand.
    session: Option<Session<FsDisk>>,
    prefix: Vec<u8>,
    dirty: &'a Cell<bool>,
}

impl<'a> DiskView<'a> {
    pub(crate) fn new(store: Rc<Storage<FsDisk>>, prefix: Vec<u8>, dirty: &'a Cell<bool>) -> Self {
        Self {
            session: Some(store.session()),
            store,
            prefix,
            dirty,
        }
    }

    fn full(&self, key: &[u8]) -> Vec<u8> {
        let mut full = Vec::with_capacity(self.prefix.len() + key.len());
        full.extend_from_slice(&self.prefix);
        full.extend_from_slice(key);
        full
    }

    fn session(&mut self) -> &mut Session<FsDisk> {
        if self.session.is_none() {
            self.session = Some(self.store.session());
        }
        self.session.as_mut().expect("just opened")
    }
}

impl GearView for DiskView<'_> {
    async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let full = self.full(key);
        self.session()
            .get_old(gear_table(), &full)
            .await
            .expect("gear view get")
    }

    async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let full = self.full(prefix);
        let rows = self
            .session()
            .scan_old(gear_table(), &full, &[])
            .await
            .expect("gear view scan");
        let strip = self.prefix.len();
        rows.into_iter()
            .map(|(key, value)| (key[strip..].to_vec(), value))
            .collect()
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        let full = self.full(key);
        self.session()
            .put(gear_table(), &full, value, &[], &[])
            .expect("gear view put");
    }

    fn del(&mut self, key: &[u8]) {
        let full = self.full(key);
        self.session()
            .del(gear_table(), &full, &[])
            .expect("gear view del");
    }

    fn commit(&mut self) {
        if let Some(session) = self.session.take() {
            session.commit().expect("gear view commit");
            self.dirty.set(true);
        }
    }
}

/// A view over a plain `BTreeMap`, keyed by the full (prefixed) key. The
/// staging contract matches [`DiskView`]: reads see the committed map,
/// writes land on [`commit`](GearView::commit).
pub struct RamView<'a> {
    map: &'a RefCell<BTreeMap<Vec<u8>, Vec<u8>>>,
    /// Staged ops by full key; `None` is a deletion.
    staged: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
    prefix: Vec<u8>,
}

impl<'a> RamView<'a> {
    pub(crate) fn new(map: &'a RefCell<BTreeMap<Vec<u8>, Vec<u8>>>, prefix: Vec<u8>) -> Self {
        Self {
            map,
            staged: BTreeMap::new(),
            prefix,
        }
    }

    fn full(&self, key: &[u8]) -> Vec<u8> {
        let mut full = Vec::with_capacity(self.prefix.len() + key.len());
        full.extend_from_slice(&self.prefix);
        full.extend_from_slice(key);
        full
    }
}

impl GearView for RamView<'_> {
    async fn get(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        self.map.borrow().get(&self.full(key)).cloned()
    }

    async fn scan(&mut self, prefix: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
        let full = self.full(prefix);
        let strip = self.prefix.len();
        let map = self.map.borrow();
        map.range(full.clone()..)
            .take_while(|(key, _)| key.starts_with(&full))
            .map(|(key, value)| (key[strip..].to_vec(), value.clone()))
            .collect()
    }

    fn put(&mut self, key: &[u8], value: &[u8]) {
        self.staged.insert(self.full(key), Some(value.to_vec()));
    }

    fn del(&mut self, key: &[u8]) {
        self.staged.insert(self.full(key), None);
    }

    fn commit(&mut self) {
        if self.staged.is_empty() {
            return;
        }
        let mut map = self.map.borrow_mut();
        for (key, value) in std::mem::take(&mut self.staged) {
            match value {
                Some(value) => drop(map.insert(key, value)),
                None => drop(map.remove(&key)),
            }
        }
    }
}
