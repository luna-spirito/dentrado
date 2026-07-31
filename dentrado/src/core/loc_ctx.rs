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
/// append-only `Vec`), its dedup index, and its `added`/`removed` changelog.
///
/// The dedup key is group-scoped — `(sender, tx_id)` — rather than the former
/// `(sender, global_core_id, tx_id)`: `global_core_id` is implied by the group
/// (one group maps to exactly one core), so it is constant within a shard and
/// carries no information. The consequence is that supersede is **always
/// intra-group by construction** — a cross-group "supersede" cannot arise, so a
/// `removed` entry never references a foreign shard.
///
/// `added`/`removed` mirror the previous per-group changelog; each references a
/// body by its `LocEventId` (a slot within this shard). Superseded bodies stay in
/// `bodies` (still fetchable); "removal" is purely a `removed` entry, never a
/// deletion.
#[derive(Debug)]
pub(crate) struct EventGroup<B> {
    pub(crate) bodies: Vec<StoredEvent<B>>,
    /// `(sender, tx_id)` -> slot of the currently-live version.
    pub(crate) dedup: HashMap<(LocSenderId, u32), u64>,
    pub(crate) added: Vec<GroupEventId>,
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
            added: Vec::new(),
            removed: Vec::new(),
        }
    }
}

pub struct StoreResultSuccess {
    pub old: Option<GroupEventId>,
    pub new: GroupEventId,
}
