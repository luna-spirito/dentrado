//! The live path: reserve an id, append its journal record, and stage the rows.
//!
//! Localization rows are staged in the delta in the same journal-lock section
//! as the append, so the next operation in the batch can resolve them. Events
//! are staged in the pending set only; readers see them after the commit that
//! writes them to the cache.

use std::{io, marker::PhantomData, rc::Rc};

use dentrado_cache::{
    Storage,
    disk::{FsDisk, IsFilesystem, MemDisk},
};
use dentrado_journal::{Cursor, Frame, Journal, SchemaId};
use futures::lock::Mutex;

use super::{
    codec,
    delta::{Commit, Counter, EventFlight, Sequencer},
    fold,
    tables::{self, Tables},
};
use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
        storage::journal::{record, schema},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GlobalResolver, GroupRouteError, LocDataId,
        LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// The log and the schema ids this build interned in it.
pub(super) struct JournalState {
    pub log: Journal,
    pub schemas: schema::Schemas,
    pub identity: [u8; 16],
}

/// The opened backend, with the disk erased exactly as before.
pub(super) enum Disk {
    Ram {
        cache: Rc<Storage<MemDisk>>,
    },
    Fs {
        cache: Rc<Storage<FsDisk>>,
        journal: Rc<Mutex<JournalState>>,
        identity: [u8; 16],
        fingerprint: [u8; 32],
    },
}

/// The opened backend plus the sequencer.
pub(super) struct Opened<R: RuntimeTypes> {
    pub disk: Disk,
    pub tables: Tables,
    pub seq: Rc<Sequencer>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub replayed: usize,
    pub _r: PhantomData<fn() -> R>,
}

/// Reserve an id or slot, append its journal record, and stage the rows it
/// describes. `reserve` runs under the journal lock, and `stage` runs before
/// that lock is released, so a later operation in the batch can resolve the
/// new id.
async fn append<T: Copy>(
    journal: Option<&Rc<Mutex<JournalState>>>,
    reserve: impl FnOnce() -> T,
    record: impl FnOnce(&JournalState, T) -> (SchemaId, Vec<u8>),
    stage: impl FnOnce(T, Option<Cursor>),
) -> T {
    if let Some(journal) = journal {
        let mut journal = journal.lock().await;
        let id = reserve();
        let (schema, bytes) = record(&journal, id);
        let cursor = journal
            .log
            .append(&[Frame::new(schema, &bytes)])
            .await
            .expect("append to the journal");
        stage(id, Some(cursor));
        id
    } else {
        let id = reserve();
        stage(id, None);
        id
    }
}

pub(super) async fn ensure_user<D: IsFilesystem>(
    cache: Rc<Storage<D>>,
    tables: Tables,
    seq: Rc<Sequencer>,
    journal: Option<Rc<Mutex<JournalState>>>,
    uid: UserId,
) -> LocUserId {
    if let Some(loc) = seq
        .get(&cache, tables.users_fwd, &codec::user_fwd(uid))
        .await
    {
        return LocUserId(codec::parse_loc(&loc).expect("a cached user loc"));
    }
    let id = append(
        journal.as_ref(),
        || seq.take_next(Counter::User),
        |journal, id| {
            let rec = record::LocUser {
                loc: id,
                id: uid.id,
                identity_server_pk: uid.identity_server_pk.0,
            };
            (
                journal.schemas.user,
                schema::encode(&rec).expect("a journal record serializes"),
            )
        },
        |id, cursor| seq.stage(fold::user_rows(&tables, LocUserId(id), uid), cursor),
    )
    .await;
    LocUserId(id)
}

pub(super) async fn ensure_sender<D: IsFilesystem>(
    cache: Rc<Storage<D>>,
    tables: Tables,
    seq: Rc<Sequencer>,
    journal: Option<Rc<Mutex<JournalState>>>,
    pk: SenderPk,
    uid: Option<UserId>,
) -> LocSenderId {
    let user = match uid {
        Some(uid) => Some(
            ensure_user::<D>(
                Rc::clone(&cache),
                tables.clone(),
                Rc::clone(&seq),
                journal.clone(),
                uid,
            )
            .await,
        ),
        None => None,
    };
    if let Some(loc) = seq
        .get(&cache, tables.senders_fwd, &codec::sender_fwd(pk))
        .await
    {
        return codec::parse_sender_fwd(&loc)
            .expect("a cached sender loc")
            .0;
    }
    let id = append(
        journal.as_ref(),
        || seq.take_next(Counter::Sender),
        |journal, id| {
            let rec = record::LocSender {
                loc: id,
                pk: pk.0,
                user: user.map(|u| u.0),
            };
            (
                journal.schemas.sender,
                schema::encode(&rec).expect("a journal record serializes"),
            )
        },
        |id, cursor| {
            seq.stage(
                fold::sender_rows(&tables, LocSenderId(id), pk, user),
                cursor,
            );
        },
    )
    .await;
    LocSenderId(id)
}

pub(super) async fn ensure_group<D: IsFilesystem>(
    cache: Rc<Storage<D>>,
    tables: Tables,
    seq: Rc<Sequencer>,
    journal: Option<Rc<Mutex<JournalState>>>,
    msg_type: LocMsgTypeId,
    bytes: Vec<u8>,
) -> LocGroupId {
    if let Some(loc) = seq
        .get(
            &cache,
            tables.groups_fwd,
            &codec::group_fwd(msg_type.0, &bytes),
        )
        .await
    {
        return LocGroupId(codec::parse_loc(&loc).expect("a cached group loc"));
    }
    let id = append(
        journal.as_ref(),
        || seq.take_next(Counter::Group),
        |journal, id| {
            let rec = record::LocGroup {
                loc: id,
                msg_type: msg_type.0,
                group_schema: journal.schemas.group_value.0,
                group: bytes.clone(),
            };
            (
                journal.schemas.group,
                schema::encode(&rec).expect("a journal record serializes"),
            )
        },
        |id, cursor| {
            seq.stage(
                fold::group_rows(&tables, LocGroupId(id), msg_type, &bytes),
                cursor,
            );
        },
    )
    .await;
    LocGroupId(id)
}

pub(super) async fn ensure_data<R: RuntimeTypes, D: IsFilesystem>(
    cache: Rc<Storage<D>>,
    tables: Tables,
    seq: Rc<Sequencer>,
    journal: Option<Rc<Mutex<JournalState>>>,
    data_id: DataId,
    content: R::Data,
) -> Result<LocDataId, DataVerifyError> {
    if let Some(loc) = seq
        .get(&cache, tables.data_fwd, &codec::data_fwd(data_id))
        .await
    {
        return Ok(LocDataId(
            codec::parse_loc(&loc).expect("a cached data loc"),
        ));
    }
    verify_data::<R, D>(&cache, &tables, &seq, data_id, &content).await?;
    let content = postcard::to_stdvec(&content).expect("a data value serializes");
    let id = append(
        journal.as_ref(),
        || seq.take_next(Counter::Data),
        |journal, id| {
            let rec = record::LocData {
                loc: id,
                timestamp: data_id.timestamp,
                hash: data_id.hash,
                content_schema: journal.schemas.data_value.0,
                content: content.clone(),
            };
            (
                journal.schemas.data,
                schema::encode(&rec).expect("a journal record serializes"),
            )
        },
        |id, cursor| {
            seq.stage(
                fold::data_rows(&tables, LocDataId(id), data_id, &content),
                cursor,
            );
        },
    )
    .await;
    Ok(LocDataId(id))
}

pub(super) async fn ensure_event<R: RuntimeTypes, D: IsFilesystem>(
    cache: Rc<Storage<D>>,
    tables: Tables,
    seq: Rc<Sequencer>,
    journal: Option<Rc<Mutex<JournalState>>>,
    group: LocGroupId,
    ev: StoredEvent<R::Body>,
    value: Vec<u8>,
) -> EventFlight {
    let key = (group, ev.sender, ev.tx_id);
    if let Some((_, stored)) = seq.event_at(&cache, &tables, key).await {
        if stored == value {
            return EventFlight { delta: None, value };
        }
        todo!("supersede: rank by (authoritative, timestamp, source_node)");
    }
    seq.ensure_shard(&cache, &tables, group).await;
    let body = codec::parse_event(&value)
        .expect("an encoded event parses")
        .body;
    let slot = append(
        journal.as_ref(),
        || seq.reserve_slot(group),
        |journal, _slot| {
            let rec = record::Event {
                group: group.0,
                sender: ev.sender.0,
                tx_id: ev.tx_id,
                timestamp: ev.timestamp,
                source_node: ev.source_node.0,
                body,
            };
            (journal.schemas.event, rec.encode())
        },
        |slot, cursor| {
            let rows = fold::event_rows(&tables, group, slot, ev.sender, ev.tx_id, &value);
            seq.stage_event(rows, cursor, key, slot, value.clone());
        },
    )
    .await;
    EventFlight {
        delta: Some(GroupDelta {
            added: Some(slot),
            removed: None,
        }),
        value,
    }
}

/// A `GlobalHash` resolver over the sequencer (delta first, cache second).
pub(crate) struct Resolver<'a, D: IsFilesystem> {
    pub cache: &'a Storage<D>,
    pub tables: &'a Tables,
    pub seq: &'a Sequencer,
}

impl<D: IsFilesystem> GlobalResolver for Resolver<'_, D> {
    async fn resolve_user(&self, lid: LocUserId) -> Result<UserId, GroupRouteError> {
        let bound =
            usize::try_from(self.seq.next_values()[Counter::User.index()]).unwrap_or(usize::MAX);
        let bytes = self
            .seq
            .get(self.cache, self.tables.users_rev, &codec::user_rev(lid))
            .await
            .ok_or(GroupRouteError::UserIdOutOfBounds {
                idx: lid.0,
                users_len: bound,
            })?;
        codec::parse_user(&bytes).ok_or(GroupRouteError::UserIdOutOfBounds {
            idx: lid.0,
            users_len: bound,
        })
    }

    async fn resolve_data(&self, did: LocDataId) -> Result<DataId, GroupRouteError> {
        let bound =
            usize::try_from(self.seq.next_values()[Counter::Data.index()]).unwrap_or(usize::MAX);
        let bytes = self
            .seq
            .get(self.cache, self.tables.data_rev, &codec::data_rev(did))
            .await
            .ok_or(GroupRouteError::DataIdOutOfBounds {
                idx: did.0,
                objects_len: bound,
            })?;
        let (data_id, _) =
            codec::parse_data_value(&bytes).ok_or(GroupRouteError::DataIdOutOfBounds {
                idx: did.0,
                objects_len: bound,
            })?;
        Ok(data_id)
    }
}

pub(crate) async fn verify_data<R: RuntimeTypes, D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    seq: &Sequencer,
    data_id: DataId,
    content: &R::Data,
) -> Result<(), DataVerifyError> {
    let resolver = Resolver { cache, tables, seq };
    match content.global_hash(&resolver).await {
        Ok(hash) if hash == data_id.hash => Ok(()),
        Ok(computed_hash) => Err(DataVerifyError::HashMismatch {
            claimed: data_id,
            computed_hash,
        }),
        Err(e) => Err(DataVerifyError::UnresolvableId(e)),
    }
}

/// Write one commit's rows and shard watermarks, then release its delta rows.
/// A refusal re-queues the batch: the journal already holds the records, and
/// the next commit retries them without serving anything early.
pub(super) fn commit<D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    seq: &Sequencer,
    meta: Option<([u8; 16], [u8; 32], Cursor)>,
    commit: Commit,
) -> io::Result<()> {
    match stage(cache, tables, meta, &commit) {
        Ok(()) => {
            seq.finish_commit(&commit);
            Ok(())
        }
        Err(e) => {
            seq.abort_commit(commit);
            Err(e)
        }
    }
}

/// One atomic cache session: the batch's rows, the shard tips, and the
/// applied cursor.
fn stage<D: IsFilesystem>(
    cache: &Storage<D>,
    tables: &Tables,
    meta: Option<([u8; 16], [u8; 32], Cursor)>,
    commit: &Commit,
) -> io::Result<()> {
    let mut session = cache.session();
    for row in commit.rows() {
        session
            .put(row.table, &row.key, &row.value, &[], &[])
            .map_err(|e| io::Error::other(format!("stage a cache row: {e:?}")))?;
    }
    for &(group, added, removed) in commit.shards() {
        session
            .put(
                tables.shards,
                &codec::shard(group),
                &codec::shard_bytes(added, removed),
                &[],
                &[],
            )
            .map_err(|e| io::Error::other(format!("stage a shard: {e:?}")))?;
    }
    if let Some((identity, fingerprint, cursor)) = meta {
        tables::write_meta(
            &mut session,
            tables,
            identity,
            fingerprint,
            cursor,
            commit.next(),
        )
        .map_err(|e| io::Error::other(format!("stage cache meta: {e:?}")))?;
    }
    session
        .commit()
        .map_err(|e| io::Error::other(format!("commit cache rows: {e:?}")))
}
