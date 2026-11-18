use std::collections::HashMap;

use crate::types::{GroupEventId, LocSenderId, NodeId};

#[derive(Clone, Debug)]
pub struct StoredEvent<B> {
    pub sender: LocSenderId,
    pub tx_id: u32,
    pub timestamp: u32,
    pub source_node: NodeId,
    pub body: B,
}

/// Per-group event shard. Owns, for one group: its event bodies (a flat,
/// append-only `Vec`), its dedup index, and its `removed` changelog.
///
/// `added` is implicit: only an accepted observation gets a body, so the
/// added slots are exactly `0..bodies.len()`. `removed` is stored because a
/// superseded slot is not derivable from the bodies alone.
///
/// The dedup key is group-scoped — `(sender, tx_id)` — rather than the former
/// `(sender, global_core_id, tx_id)`: `global_core_id` is implied by the group
/// (one group maps to exactly one core), so it is constant within a shard and
/// carries no information. The consequence is that supersede is **always
/// intra-group by construction** — a cross-group "supersede" cannot arise, so a
/// `removed` entry never references a foreign shard.
///
/// Bodies stay in `bodies` when superseded (still fetchable); "removal" is
/// purely a `removed` entry, never a deletion.
#[derive(Debug)]
pub(crate) struct EventGroup<B> {
    pub(crate) bodies: Vec<StoredEvent<B>>,
    /// `(sender, tx_id)` -> slot of the currently-live version.
    pub(crate) dedup: HashMap<(LocSenderId, u32), u64>,
    pub(crate) removed: Vec<GroupEventId>,
}

// Hand-written (not derived) so it does NOT impose `B: Default` (which
// `#[derive(Default)]` would — `R::Body` is not `Default`). `Vec`/`HashMap`
// default without any bound on `B`.
impl<B> Default for EventGroup<B> {
    fn default() -> Self {
        Self {
            bodies: Vec::new(),
            dedup: HashMap::new(),
            removed: Vec::new(),
        }
    }
}

/// What one observation changed in its group's shard: the slot it landed in
/// and the slot it superseded, if any.
///
/// An observation that changes nothing (a stale copy) stores no body and has
/// no delta, so [`store_event`] returns `None` rather than an empty one.
///
/// [`store_event`]: crate::core::storage::IsStorage::store_event
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GroupDelta {
    pub added: Option<GroupEventId>,
    pub removed: Option<GroupEventId>,
}
