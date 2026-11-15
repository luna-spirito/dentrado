//! Schema ids and the registry they are interned in.
//!
//! A schema id is a log-local `u16`, assigned once and never reused. The log
//! carries its own `id ↔ (name, shape)` mapping, so it is self-describing
//! without the application: a build that reopens a log knows exactly which
//! layout every byte was written with.

use std::collections::HashMap;

use crate::error::{Conflict, Error};

/// A schema id, dense within one log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SchemaId(pub u16);

impl SchemaId {
    /// The declaration record itself (`journal/schema`).
    pub const DECL: Self = Self(0);
    /// Reserved for a future purge record.
    pub const PURGE: Self = Self(1);
    /// First id available to the consumer; `2..16` are reserved.
    pub const USER_MIN: u16 = 16;

    /// Whether this id belongs to the consumer (as opposed to the format).
    #[must_use]
    pub const fn is_user(self) -> bool {
        self.0 >= Self::USER_MIN
    }
}

/// The largest name or shape text a declaration may carry.
pub const MAX_TEXT: usize = 4096;

/// A layout the consumer wants to write: a name for humans, a shape for the
/// registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decl<'a> {
    /// Stable identifier for the record type, e.g. `dentrado/event`.
    pub name: &'a str,
    /// The serde layout, e.g. `{loc:u64,name:Option<String>}`.
    pub shape: &'a str,
}

/// A decoded declaration record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclRecord {
    /// The interned id.
    pub id: SchemaId,
    /// `blake3(shape)`: the declaration's identity across logs.
    pub hash: [u8; 32],
    /// What the record is called.
    pub name: String,
    /// How it is laid out.
    pub shape: String,
}

impl DeclRecord {
    /// The declaration `decl` would intern under `id`.
    #[must_use]
    pub fn new(id: SchemaId, decl: Decl<'_>) -> Self {
        Self {
            id,
            hash: hash_of(decl.shape),
            name: decl.name.to_owned(),
            shape: decl.shape.to_owned(),
        }
    }

    fn describe(&self) -> String {
        format!("{} = {}", self.name, self.shape)
    }
}

/// `blake3` of a shape text.
#[must_use]
pub fn hash_of(shape: &str) -> [u8; 32] {
    *blake3::hash(shape.as_bytes()).as_bytes()
}

/// Serializes a declaration: `id u16 | hash [u8;32] | name | shape`,
/// each text being `len u32 | utf-8`.
#[must_use]
pub fn encode_decl(record: &DeclRecord) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(2 + 32 + 8 + record.name.len() + record.shape.len());
    bytes.extend_from_slice(&record.id.0.to_le_bytes());
    bytes.extend_from_slice(&record.hash);
    for text in [&record.name, &record.shape] {
        bytes.extend_from_slice(&u32::try_from(text.len()).unwrap_or(0).to_le_bytes());
        bytes.extend_from_slice(text.as_bytes());
    }
    bytes
}

/// The inverse of [`encode_decl`].
///
/// # Errors
///
/// [`Error::Malformed`] for a truncated payload, text that is not utf-8, or
/// trailing bytes.
pub fn decode_decl(bytes: &[u8]) -> Result<DeclRecord, Error> {
    let mut at = Reader::new(bytes);
    let id = SchemaId(at.u16()?);
    let hash = at.array::<32>()?;
    let name = at.text()?;
    let shape = at.text()?;
    if !at.done() {
        return Err(Error::Malformed("trailing bytes after a declaration"));
    }
    Ok(DeclRecord {
        id,
        hash,
        name,
        shape,
    })
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(len).filter(|e| *e <= self.bytes.len());
        let Some(end) = end else {
            return Err(Error::Malformed("declaration is truncated"));
        };
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        self.take(N)?
            .try_into()
            .map_err(|_| Error::Malformed("declaration is truncated"))
    }

    fn u16(&mut self) -> Result<u16, Error> {
        self.array::<2>().map(u16::from_le_bytes)
    }

    fn u32(&mut self) -> Result<u32, Error> {
        self.array::<4>().map(u32::from_le_bytes)
    }

    fn text(&mut self) -> Result<String, Error> {
        let len = self.u32()? as usize;
        if len > MAX_TEXT {
            return Err(Error::Malformed("declaration text is too long"));
        }
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec())
            .map_err(|_| Error::Malformed("declaration text is not utf-8"))
    }

    fn done(&self) -> bool {
        self.at == self.bytes.len()
    }
}

/// The ids a log has handed out, and the declarations behind them.
#[derive(Debug, Clone)]
pub struct Registry {
    by_id: Vec<Option<DeclRecord>>,
    by_hash: HashMap<[u8; 32], u16>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// A registry with only the format's own schemas in it.
    #[must_use]
    pub fn new() -> Self {
        let mut registry = Self {
            by_id: Vec::new(),
            by_hash: HashMap::new(),
        };
        for (id, name) in [
            (SchemaId::DECL, "journal/schema"),
            (SchemaId::PURGE, "journal/purge"),
        ] {
            registry.insert(DeclRecord::new(id, Decl { name, shape: name }));
        }
        registry
    }

    /// The declaration behind an id, if any.
    #[must_use]
    pub fn get(&self, id: SchemaId) -> Option<&DeclRecord> {
        self.by_id.get(id.0 as usize)?.as_ref()
    }

    /// The id a shape was interned under, if any.
    #[must_use]
    pub fn find(&self, shape: &str) -> Option<SchemaId> {
        self.by_hash.get(&hash_of(shape)).map(|id| SchemaId(*id))
    }

    /// Every declaration, in id order.
    pub fn iter(&self) -> impl Iterator<Item = &DeclRecord> {
        self.by_id.iter().flatten()
    }

    /// The id the next declaration would take.
    ///
    /// # Errors
    ///
    /// [`Error::RegistryFull`] once every user id has been handed out.
    pub fn next_id(&self) -> Result<SchemaId, Error> {
        let used = self.by_id.len().max(SchemaId::USER_MIN as usize);
        u16::try_from(used)
            .map(SchemaId)
            .map_err(|_| Error::RegistryFull)
    }

    /// Whether `record` may be inserted, and how it disagrees if it may not.
    ///
    /// # Errors
    ///
    /// [`Error::Schema`] for anything but a byte-identical repeat of an earlier
    /// declaration, which is a no-op.
    pub fn check(&self, record: &DeclRecord) -> Result<(), Error> {
        let refuse = |conflict, declared: &DeclRecord, incoming: &DeclRecord| Error::Schema {
            id: incoming.id.0,
            conflict,
            declared: declared.describe(),
            incoming: incoming.describe(),
        };
        if !record.id.is_user() {
            return Err(Error::Schema {
                id: record.id.0,
                conflict: Conflict::Reserved,
                declared: String::from("journal builtins own 0..16"),
                incoming: record.describe(),
            });
        }
        if hash_of(&record.shape) != record.hash {
            return Err(Error::Schema {
                id: record.id.0,
                conflict: Conflict::HashMismatch,
                declared: format!("blake3({})", record.shape),
                incoming: record.describe(),
            });
        }
        if let Some(existing) = self.get(record.id) {
            if existing == record {
                return Ok(());
            }
            let conflict = if existing.hash == record.hash {
                Conflict::Renamed
            } else {
                Conflict::Redefined
            };
            return Err(refuse(conflict, existing, record));
        }
        if let Some(existing) = self
            .by_hash
            .get(&record.hash)
            .and_then(|id| self.get(SchemaId(*id)))
        {
            return Err(refuse(Conflict::DuplicateShape, existing, record));
        }
        Ok(())
    }

    /// Checks and inserts.
    ///
    /// # Errors
    ///
    /// Whatever [`Registry::check`] refuses.
    pub fn declare(&mut self, record: DeclRecord) -> Result<(), Error> {
        self.check(&record)?;
        self.insert(record);
        Ok(())
    }

    /// Inserts a record that [`Registry::check`] has already accepted.
    pub(crate) fn insert(&mut self, record: DeclRecord) {
        let at = record.id.0 as usize;
        if self.by_id.len() <= at {
            self.by_id.resize(at + 1, None);
        }
        self.by_hash.insert(record.hash, record.id.0);
        if let Some(slot) = self.by_id.get_mut(at) {
            *slot = Some(record);
        }
    }

    /// `blake3` over every `(id, shape_hash)` pair in id order: the identity of
    /// this id space, for anything that materializes the log elsewhere.
    #[must_use]
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        for record in self.iter() {
            hasher.update(&record.id.0.to_le_bytes());
            hasher.update(&record.hash);
        }
        *hasher.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::{DeclRecord, Registry, SchemaId, decode_decl, encode_decl, hash_of};
    use crate::error::{Conflict, Error};

    fn record(id: u16, name: &str, shape: &str) -> DeclRecord {
        DeclRecord {
            id: SchemaId(id),
            hash: hash_of(shape),
            name: name.to_owned(),
            shape: shape.to_owned(),
        }
    }

    #[test]
    fn ids_start_after_the_reserved_range() {
        let registry = Registry::new();
        assert_eq!(registry.next_id().unwrap(), SchemaId(16));
        assert!(registry.get(SchemaId::DECL).is_some());
        assert!(registry.get(SchemaId::PURGE).is_some());
        assert!(registry.get(SchemaId(5)).is_none());
    }

    #[test]
    fn allocating_never_reuses_an_id() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        registry.declare(record(17, "b", "u32")).unwrap();
        assert_eq!(registry.next_id().unwrap(), SchemaId(18));
        assert_eq!(registry.find("u32"), Some(SchemaId(17)));
        assert_eq!(registry.iter().count(), 4);
    }

    #[test]
    fn a_repeat_is_a_no_op() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        registry.declare(record(16, "a", "u64")).unwrap();
        assert_eq!(registry.next_id().unwrap(), SchemaId(17));
    }

    fn conflict(result: Result<(), Error>) -> Conflict {
        match result {
            Err(Error::Schema { conflict, .. }) => conflict,
            other => panic!("expected a schema conflict, got {other:?}"),
        }
    }

    #[test]
    fn an_id_cannot_change_meaning() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        assert_eq!(
            conflict(registry.declare(record(16, "a", "u32"))),
            Conflict::Redefined
        );
    }

    #[test]
    fn a_shape_cannot_move() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        assert_eq!(
            conflict(registry.declare(record(17, "b", "u64"))),
            Conflict::DuplicateShape
        );
    }

    #[test]
    fn a_shape_cannot_be_renamed() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        assert_eq!(
            conflict(registry.declare(record(16, "b", "u64"))),
            Conflict::Renamed
        );
    }

    #[test]
    fn a_name_may_grow_a_new_shape() {
        let mut registry = Registry::new();
        registry.declare(record(16, "a", "u64")).unwrap();
        registry.declare(record(17, "a", "u32")).unwrap();
        assert_eq!(registry.find("u64"), Some(SchemaId(16)));
        assert_eq!(registry.find("u32"), Some(SchemaId(17)));
    }

    #[test]
    fn builtin_ids_are_not_negotiable() {
        let registry = Registry::new();
        assert_eq!(
            conflict(registry.clone().declare(record(3, "x", "u8"))),
            Conflict::Reserved
        );
    }

    #[test]
    fn a_wrong_hash_is_refused() {
        let registry = Registry::new();
        let mut lying = record(16, "a", "u64");
        lying.hash = [0; 32];
        assert_eq!(
            conflict(registry.clone().declare(lying)),
            Conflict::HashMismatch
        );
    }

    #[test]
    fn declarations_round_trip() {
        let record = record(16, "dentrado/event", "{group:u64,body:Vec<u8>}");
        assert_eq!(decode_decl(&encode_decl(&record)).unwrap(), record);
    }

    #[test]
    fn truncated_declarations_are_malformed() {
        let bytes = encode_decl(&record(16, "a", "u64"));
        for cut in 0..bytes.len() {
            assert!(decode_decl(&bytes[..cut]).is_err(), "{cut} bytes decoded");
        }
    }

    #[test]
    fn the_fingerprint_follows_the_id_space() {
        let mut registry = Registry::new();
        let before = registry.fingerprint();
        registry.declare(record(16, "a", "u64")).unwrap();
        assert_ne!(before, registry.fingerprint());
        assert_eq!(registry.fingerprint(), registry.clone().fingerprint());
    }
}
