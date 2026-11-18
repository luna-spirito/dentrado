use crate::types::{GroupEventId, LocSenderId, NodeId};

#[derive(Clone, Debug)]
pub struct StoredEvent<B> {
    pub sender: LocSenderId,
    pub tx_id: u32,
    pub timestamp: u32,
    pub source_node: NodeId,
    pub body: B,
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
