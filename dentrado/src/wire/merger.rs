use crate::{
    core::{
        gear::IsRuntime,
        loc_ctx::{StoreResultSuccess, StoredEvent},
        storage::Storage,
    },
    types::{LocDataId, LocGroupId, LocSenderId, LocUserId, Localizable, NodeId, Remapper},
    wire::format::{MergeError, WireEventBody, WireLocCtx},
};

pub(crate) struct WireLocCtxMerger<'a, R: IsRuntime, S: Storage<R>> {
    source: &'a WireLocCtx<R>,
    target: &'a S,
    user_map: Vec<Option<LocUserId>>,
    sender_map: Vec<Option<LocSenderId>>,
    data_map: Vec<Option<LocDataId>>,
}

struct MergerRemapper<'a, 'b, R: IsRuntime, S: Storage<R>> {
    merger: &'b mut WireLocCtxMerger<'a, R, S>,
    allowed_before: usize,
}

impl<'a, R: IsRuntime, S: Storage<R>> WireLocCtxMerger<'a, R, S> {
    pub(crate) fn new(source: &'a WireLocCtx<R>, target: &'a S) -> Self {
        Self {
            source,
            target,
            user_map: vec![None; source.users.len()],
            sender_map: vec![None; source.senders.len()],
            data_map: vec![None; source.data.len()],
        }
    }

    pub(crate) async fn remap<L: Localizable>(&mut self, obj: L) -> Result<L, MergeError> {
        obj.localize(&mut MergerRemapper {
            allowed_before: self.source.data.len(),
            merger: self,
        })
        .await
    }

    pub(crate) async fn import_new_event(
        &mut self,
        event: WireEventBody<R::Group, R::Body>,
        timestamp: u32,
        source_node: NodeId,
    ) -> Result<(LocGroupId, Option<StoreResultSuccess>), MergeError> {
        let sender = self.remap(event.sender).await?;
        let group = self.remap(event.group).await?;
        let body = self.remap(event.body).await?;

        let group_id = self.target.mk_loc_group(event.msg_type, group).await;
        Ok((
            group_id,
            self.target
                .store_event(
                    group_id,
                    StoredEvent {
                        sender,
                        tx_id: event.tx_id,
                        timestamp,
                        source_node,
                        body,
                    },
                )
                .await,
        ))
    }
}

impl<R: IsRuntime, S: Storage<R>> Remapper for MergerRemapper<'_, '_, R, S> {
    type Err = MergeError;
    async fn remap_user(&mut self, lid: LocUserId) -> Result<LocUserId, MergeError> {
        let idx = lid.0 as usize;

        if idx >= self.merger.source.users.len() {
            return Err(MergeError::UserOutOfBounds {
                idx: lid.0,
                len: self.merger.source.users.len(),
            });
        }

        if let Some(mapped) = self.merger.user_map[idx] {
            return Ok(mapped);
        }

        let uid = self.merger.source.users[idx];

        let local_id = self.merger.target.mk_loc_user(uid).await;
        self.merger.user_map[idx] = Some(local_id);
        Ok(local_id)
    }

    async fn remap_sender(&mut self, sid: LocSenderId) -> Result<LocSenderId, MergeError> {
        let idx = sid.0 as usize;

        if idx >= self.merger.source.senders.len() {
            return Err(MergeError::SenderOutOfBounds {
                idx: sid.0,
                len: self.merger.source.senders.len(),
            });
        }

        if let Some(mapped) = self.merger.sender_map[idx] {
            return Ok(mapped);
        }

        let (pk, user_idx) = &self.merger.source.senders[idx];
        let user_idx_val = *user_idx as usize;
        self.remap_user(LocUserId(user_idx_val as u64)).await?;

        if user_idx_val >= self.merger.source.users.len() {
            return Err(MergeError::SenderUserOutOfBounds {
                sender_idx: sid.0,
                user_idx: *user_idx,
                users_len: self.merger.source.users.len(),
            });
        }
        let uid = self.merger.source.users[user_idx_val];

        let local_id = self.merger.target.mk_loc_sender(*pk, Some(uid)).await;
        self.merger.sender_map[idx] = Some(local_id);
        Ok(local_id)
    }

    async fn remap_data(&mut self, did: LocDataId) -> Result<LocDataId, MergeError> {
        let idx = did.0 as usize;

        if idx >= self.merger.source.data.len() {
            return Err(MergeError::DataOutOfBounds {
                idx: did.0,
                len: self.merger.source.data.len(),
            });
        }

        if idx >= self.allowed_before {
            return Err(MergeError::DataForwardReference {
                idx: did.0,
                allowed_before: self.allowed_before as u64,
            });
        }

        if let Some(mapped) = self.merger.data_map[idx] {
            return Ok(mapped);
        }

        let (data_id, content) = &self.merger.source.data[idx];

        if let Some(existing) = self.merger.target.find_data(data_id).await {
            self.merger.data_map[idx] = Some(existing);
            return Ok(existing);
        }

        let next_max = idx; // data[i]'s content may only reference data[0..i)
        // Box the recursive `localize`: `remap_data` → `R::Data::localize` →
        // `remap_data` is a cycle, so the recursive future must be type-erased
        // to keep `remap_data`'s own future finite.
        let localized = Box::pin(content.clone().localize(&mut MergerRemapper {
            merger: &mut *self.merger,
            allowed_before: next_max,
        }))
        .await?;

        let new_did = self
            .merger
            .target
            .mk_data(*data_id, localized)
            .await
            .map_err(MergeError::DataVerify)?;

        self.merger.data_map[idx] = Some(new_did);
        Ok(new_did)
    }
}
