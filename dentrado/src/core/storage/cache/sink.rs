//! The replay sink over the cache.
//!
//! Replay itself is a journal concern ([`crate::core::storage::journal::replay`]);
//! this is the only implementation of its [`Sink`], and it applies the very
//! same [`Ops`] transitions the live path uses. That is what makes replay and
//! the live path unable to disagree about dedup, slots, or counters.

use super::{
    codec,
    ops::{EventOutcome, Ops},
};
use crate::{
    core::{
        api::RuntimeTypes,
        storage::journal::{record, replay::Sink},
    },
    types::{
        DataId, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// Replay's destination: layer 1–2 applies plus a count of what was applied.
pub(super) struct CacheSink<'a, R: RuntimeTypes, D: dentrado_cache::disk::IsFilesystem> {
    pub(super) ops: Ops<'a, R, D>,
    pub(super) replayed: usize,
}

impl<R: RuntimeTypes, D: dentrado_cache::disk::IsFilesystem> Sink<R> for CacheSink<'_, R, D> {
    async fn user(&mut self, loc: LocUserId, uid: UserId) -> Result<(), String> {
        self.replayed += 1;
        self.ops.apply_user(loc, uid).await
    }

    async fn sender(
        &mut self,
        loc: LocSenderId,
        pk: SenderPk,
        user: Option<LocUserId>,
    ) -> Result<(), String> {
        self.replayed += 1;
        self.ops.apply_sender(loc, pk, user).await
    }

    async fn group(
        &mut self,
        loc: LocGroupId,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> Result<(), String> {
        self.replayed += 1;
        self.ops.apply_group(loc, msg_type, &group).await
    }

    async fn data(
        &mut self,
        loc: LocDataId,
        data_id: DataId,
        content: R::Data,
    ) -> Result<(), String> {
        self.replayed += 1;
        self.ops.apply_data(loc, data_id, &content).await
    }

    async fn event(&mut self, group: LocGroupId, ev: &record::Event<'_>) -> Result<(), String> {
        self.replayed += 1;
        let value = codec::event_bytes(ev.sender, ev.tx_id, ev.timestamp, ev.source_node, ev.body);
        match self
            .ops
            .event_outcome(group, LocSenderId(ev.sender), ev.tx_id, &value)
            .await
        {
            EventOutcome::New(slot) => self.ops.put_event(group, slot, &value).await,
            EventOutcome::Same(_) => {}
            EventOutcome::Conflict(_) => {
                todo!("supersede: rank by (authoritative, timestamp, source_node)")
            }
        }
        Ok(())
    }
}
