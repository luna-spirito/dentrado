//! The live path: allocating loc ids, appending to the journal, applying to
//! the cache.
//!
//! Every operation reads through [`Ops`], appends its journal record first
//! (buffered), and then applies to the cache in one session. The two steps
//! are safe in that order: replay re-derives a cache write whose journal
//! append was lost, and [`Ready::commit_log`] makes the append durable before
//! the cache's applied cursor may advance.

use std::{io, marker::PhantomData, rc::Rc};

use dentrado_cache::{Storage, disk::IsFilesystem};
use dentrado_journal::{Frame, Journal, SchemaId};
use serde::Serialize;

use super::{
    codec,
    ops::{Counter, EventOutcome, Ops},
    tables::{self, Tables},
};
use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
        storage::journal::{record, schema},
    },
    types::{
        DataId, DataVerifyError, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId,
        SenderPk, UserId,
    },
};

/// The opened backend: the cache, its tables, and (optionally) the log it is a
/// materialization of.
pub(super) struct Ready<R: RuntimeTypes, D: IsFilesystem> {
    pub(super) cache: Rc<Storage<D>>,
    pub(super) tables: Tables,
    pub(super) fingerprint: [u8; 32],
    pub(super) journal: Option<JournalState>,
    /// Records applied by the startup replay, for the log and tests.
    pub(super) replayed: usize,
    pub(super) _r: PhantomData<fn() -> R>,
}

pub(super) struct JournalState {
    pub(super) log: Journal,
    pub(super) schemas: schema::Schemas,
    pub(super) identity: [u8; 16],
}

impl<R: RuntimeTypes, D: IsFilesystem> Ready<R, D> {
    pub(super) fn ops(&self) -> Ops<'_, R, D> {
        Ops::new(&self.cache, &self.tables)
    }

    /// Append a record to the journal, if there is one. The append is
    /// buffered; [`Ready::commit_log`] is what makes it durable.
    ///
    /// The schema is picked from the journal itself: without a journal there
    /// is nothing to name, and the caller cannot know a log-local id.
    async fn append<T: Serialize>(
        journal: Option<&mut JournalState>,
        schema: impl FnOnce(&JournalState) -> SchemaId,
        record: &T,
    ) {
        let Some(journal) = journal else {
            return;
        };
        let bytes = schema::encode(record).expect("a journal record serializes");
        let schema = schema(journal);
        journal
            .log
            .append(&[Frame::new(schema, &bytes)])
            .await
            .expect("append to the journal");
    }

    async fn append_event(&mut self, group: LocGroupId, ev: &StoredEvent<R::Body>, body: &[u8]) {
        let Some(journal) = &mut self.journal else {
            return;
        };
        let record = record::Event {
            group: group.0,
            sender: ev.sender.0,
            tx_id: ev.tx_id,
            timestamp: ev.timestamp,
            source_node: ev.source_node.0,
            body,
        };
        journal
            .log
            .append(&[Frame::new(journal.schemas.event, &record.encode())])
            .await
            .expect("append an event to the journal");
    }

    pub(super) async fn ensure_user(&mut self, uid: UserId) -> LocUserId {
        let loc = {
            let ops = self.ops();
            if let Some(loc) = ops.user_loc(uid).await {
                return loc;
            }
            LocUserId(ops.next(Counter::User).await)
        };
        Self::append(
            self.journal.as_mut(),
            |j| j.schemas.user,
            &record::LocUser {
                loc: loc.0,
                id: uid.id,
                identity_server_pk: uid.identity_server_pk.0,
            },
        )
        .await;
        self.ops().apply_user(loc, uid).await.expect("apply a user");
        loc
    }

    pub(super) async fn ensure_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        let user = match uid {
            Some(uid) => Some(self.ensure_user(uid).await),
            None => None,
        };
        let loc = {
            let ops = self.ops();
            if let Some(loc) = ops.sender_loc(pk).await {
                return loc;
            }
            LocSenderId(ops.next(Counter::Sender).await)
        };
        Self::append(
            self.journal.as_mut(),
            |j| j.schemas.sender,
            &record::LocSender {
                loc: loc.0,
                pk: pk.0,
                user: user.map(|u| u.0),
            },
        )
        .await;
        self.ops()
            .apply_sender(loc, pk, user)
            .await
            .expect("apply a sender");
        loc
    }

    pub(super) async fn ensure_group(
        &mut self,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> LocGroupId {
        let bytes = postcard::to_stdvec(&group).expect("a group value serializes");
        let loc = {
            let ops = self.ops();
            if let Some(loc) = ops.group_loc(msg_type, &bytes).await {
                return loc;
            }
            LocGroupId(ops.next(Counter::Group).await)
        };
        if let Some(journal) = &self.journal {
            let record = record::LocGroup {
                loc: loc.0,
                msg_type: msg_type.0,
                group_schema: journal.schemas.group_value.0,
                group: bytes,
            };
            Self::append(self.journal.as_mut(), |j| j.schemas.group, &record).await;
        }
        self.ops()
            .apply_group(loc, msg_type, &group)
            .await
            .expect("apply a group");
        loc
    }

    pub(super) async fn ensure_data(
        &mut self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        let loc = {
            let ops = self.ops();
            if let Some(loc) = ops.data_loc(data_id).await {
                return Ok(loc);
            }
            LocDataId(ops.next(Counter::Data).await)
        };
        self.ops().verify_data(data_id, &content).await?;
        if let Some(journal) = &self.journal {
            let record = record::LocData {
                loc: loc.0,
                timestamp: data_id.timestamp,
                hash: data_id.hash,
                content_schema: journal.schemas.data_value.0,
                content: postcard::to_stdvec(&content).expect("a data value serializes"),
            };
            Self::append(self.journal.as_mut(), |j| j.schemas.data, &record).await;
        }
        self.ops()
            .put_data(loc, data_id, &content)
            .await
            .expect("apply a data value");
        Ok(loc)
    }

    pub(super) async fn store_event(
        &mut self,
        group: LocGroupId,
        ev: StoredEvent<R::Body>,
    ) -> Option<GroupDelta> {
        let body = schema::encode(&ev.body).expect("an event body serializes");
        let value =
            codec::event_bytes(ev.sender.0, ev.tx_id, ev.timestamp, ev.source_node.0, &body);
        let outcome = self
            .ops()
            .event_outcome(group, ev.sender, ev.tx_id, &value)
            .await;
        match outcome {
            EventOutcome::Conflict => {
                todo!("supersede: rank by (authoritative, timestamp, source_node)")
            }
            EventOutcome::Same => {
                // The cache may hold an observation whose journal append was
                // lost; a retry must make it durable. Replay treats the
                // duplicate as a no-op.
                self.append_event(group, &ev, &body).await;
                None
            }
            EventOutcome::New(slot) => {
                self.append_event(group, &ev, &body).await;
                self.ops().put_event(group, slot, &value).await;
                Some(GroupDelta {
                    added: Some(slot),
                    removed: None,
                })
            }
        }
    }

    /// Sync the journal, then advance the cache's applied cursor.
    ///
    /// The caller holds the operation gate, so no read can observe the new
    /// cursor before the data it covers. The page store itself is flushed separately
    /// (a crash before that flush simply replays from the older cursor).
    pub(super) async fn commit_log(&mut self) -> io::Result<()> {
        let (head, identity) = {
            let Some(journal) = &mut self.journal else {
                return Ok(());
            };
            journal.log.sync().await.map_err(io::Error::other)?;
            (journal.log.head(), journal.identity)
        };
        let mut session = self.cache.session();
        tables::write_meta(&mut session, &self.tables, identity, self.fingerprint, head)
            .map_err(io::Error::other)?;
        session.commit().map_err(io::Error::other)
    }
}
