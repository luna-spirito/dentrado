//! Replaying journal records into the cache.
//!
//! The journal is domain-blind, so this adapter is where a record regains
//! meaning: a `LocUser` becomes a localized user, a `LocGroup` carries its
//! group value under a declared schema, and an `Event` becomes an observation
//! of its group. Nested payloads (`group`, `content`) are decoded with the
//! schema id the record names, after checking that the declaration is this
//! runtime's layout.
//!
//! Replay drives the same [`fold`] the live path uses; only the sink differs
//! (replay writes straight to the cache).

use std::{fmt, io, ops::ControlFlow};

use dentrado_cache::{Storage, disk::IsFilesystem};
use dentrado_journal::{Cursor, Frame, Journal, ReplayError, SchemaId};
use serde::de::DeserializeOwned;

use super::{codec, delta::Sequencer, events, fold, live, rows::Row, tables::Tables};
use crate::{
    core::{
        api::RuntimeTypes,
        storage::journal::{
            record::{Event, LocData, LocGroup, LocSender, LocUser},
            schema,
        },
    },
    types::{
        DataId, IdentityServerPk, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId,
        SenderPk, Shape, UserId,
    },
};

/// Replay's sink: each record's rows land in one cache session.
struct CacheSink<'a, D: IsFilesystem> {
    cache: &'a Storage<D>,
}

impl<D: IsFilesystem> super::rows::Sink for CacheSink<'_, D> {
    async fn get(&mut self, table: dentrado_cache::TableId, key: &[u8]) -> Option<Vec<u8>> {
        self.cache.get(table, key).await.expect("read the cache")
    }

    fn put(&mut self, rows: Vec<Row>) {
        let mut session = self.cache.session();
        for row in rows {
            session
                .put(row.table, &row.key, &row.value, &[], &[])
                .expect("stage a replayed row");
        }
        session.commit().expect("commit a replayed row");
    }
}

/// What a declaration is, once its shape matches one of dentrado's records.
#[derive(Clone, Copy)]
enum Kind {
    User,
    Sender,
    Group,
    Data,
    Event,
}

/// The record schemas this log declared, plus the nested layouts the runtime
/// expects.
struct Replay {
    records: Vec<(SchemaId, Kind)>,
    group: String,
    data: String,
}

impl Replay {
    fn new<R: RuntimeTypes>(schemas: &schema::Schemas) -> Self {
        Self {
            records: vec![
                (schemas.user, Kind::User),
                (schemas.sender, Kind::Sender),
                (schemas.group, Kind::Group),
                (schemas.data, Kind::Data),
                (schemas.event, Kind::Event),
            ],
            group: R::Group::shape(),
            data: R::Data::shape(),
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn apply<R: RuntimeTypes, D: IsFilesystem>(
        &self,
        journal: &Journal,
        at: Cursor,
        frame: Frame<'_>,
        cache: &Storage<D>,
        tables: &Tables,
        seq: &Sequencer,
    ) -> Result<bool, Error> {
        // Segment zero carries the log's own metadata, not state to apply.
        if matches!(frame.schema, SchemaId::DECL | SchemaId::IDENTITY) {
            return Ok(false);
        }
        let Some(kind) = self
            .records
            .iter()
            .find_map(|&(id, kind)| (id == frame.schema).then_some(kind))
        else {
            return Err(Error::UnexpectedRecord {
                at,
                id: frame.schema.0,
            });
        };
        let mut sink = CacheSink { cache };
        match kind {
            Kind::User => {
                let rec: LocUser = decode(at, "loc-user", frame.payload)?;
                let loc = LocUserId(rec.loc);
                fold::user(
                    &mut sink,
                    tables,
                    loc,
                    UserId {
                        id: rec.id,
                        identity_server_pk: IdentityServerPk(rec.identity_server_pk),
                    },
                )
                .await
                .map_err(|message| Error::Apply { at, message })?;
                seq.observe(super::delta::Counter::User, rec.loc);
            }
            Kind::Sender => {
                let rec: LocSender = decode(at, "loc-sender", frame.payload)?;
                fold::sender(
                    &mut sink,
                    tables,
                    LocSenderId(rec.loc),
                    SenderPk(rec.pk),
                    rec.user.map(LocUserId),
                )
                .await
                .map_err(|message| Error::Apply { at, message })?;
                seq.observe(super::delta::Counter::Sender, rec.loc);
            }
            Kind::Group => {
                let rec: LocGroup = decode(at, "loc-group", frame.payload)?;
                // Decode only to prove the named schema is this runtime's
                // group layout; the cache keeps the bytes verbatim.
                let _: R::Group = decode_nested(
                    journal,
                    at,
                    "group",
                    &self.group,
                    rec.group_schema,
                    &rec.group,
                )?;
                fold::group(
                    &mut sink,
                    tables,
                    LocGroupId(rec.loc),
                    LocMsgTypeId(rec.msg_type),
                    &rec.group,
                )
                .await
                .map_err(|message| Error::Apply { at, message })?;
                seq.observe(super::delta::Counter::Group, rec.loc);
            }
            Kind::Data => {
                let rec: LocData = decode(at, "loc-data", frame.payload)?;
                let content: R::Data = decode_nested(
                    journal,
                    at,
                    "data",
                    &self.data,
                    rec.content_schema,
                    &rec.content,
                )?;
                let data_id = DataId {
                    timestamp: rec.timestamp,
                    hash: rec.hash,
                };
                live::verify_data::<R, D>(cache, tables, seq, data_id, &content)
                    .await
                    .map_err(|e| Error::Apply {
                        at,
                        message: e.to_string(),
                    })?;
                fold::data(&mut sink, tables, LocDataId(rec.loc), data_id, &rec.content)
                    .await
                    .map_err(|message| Error::Apply { at, message })?;
                seq.observe(super::delta::Counter::Data, rec.loc);
            }
            Kind::Event => {
                let ev = Event::decode(frame.payload).ok_or(Error::ShortEvent { at })?;
                let value =
                    codec::event_bytes(ev.sender, ev.tx_id, ev.timestamp, ev.source_node, ev.body);
                events::replay(
                    cache,
                    tables,
                    LocGroupId(ev.group),
                    LocSenderId(ev.sender),
                    ev.tx_id,
                    &value,
                )
                .await
                .map_err(|message| Error::Apply { at, message })?;
            }
        }
        Ok(true)
    }
}

/// Decode a nested payload under the schema the record names, after checking
/// that the declaration is this runtime's layout.
fn decode_nested<T: DeserializeOwned>(
    journal: &Journal,
    at: Cursor,
    what: &'static str,
    expected: &str,
    id: u16,
    bytes: &[u8],
) -> Result<T, Error> {
    check_nested(journal, at, what, expected, id)?;
    decode(at, what, bytes)
}

/// Check that `id` names a declaration with the runtime's `expected` layout,
/// without decoding the payload.
fn check_nested(
    journal: &Journal,
    at: Cursor,
    what: &'static str,
    expected: &str,
    id: u16,
) -> Result<(), Error> {
    let id = SchemaId(id);
    let decl = journal
        .decls()
        .get(id)
        .ok_or(Error::Undeclared { at, id: id.0 })?;
    if decl.shape != expected {
        return Err(Error::WrongShape {
            at,
            what,
            expected: expected.to_owned(),
            found: decl.shape.clone(),
        });
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(at: Cursor, what: &'static str, bytes: &[u8]) -> Result<T, Error> {
    postcard::from_bytes(bytes).map_err(|source| Error::Decode { at, what, source })
}

/// Replays `[from, head)` into the cache, returning how many records were
/// applied.
pub(super) async fn replay<R: RuntimeTypes, D: IsFilesystem>(
    journal: &Journal,
    schemas: &schema::Schemas,
    from: Cursor,
    cache: &Storage<D>,
    tables: &Tables,
    seq: &Sequencer,
) -> Result<usize, ReplayError<Error>> {
    let replay = Replay::new::<R>(schemas);
    let mut applied = 0usize;
    journal
        .try_replay_from(from, async |at, frame| {
            if replay
                .apply::<R, D>(journal, at, frame, cache, tables, seq)
                .await?
            {
                applied += 1;
            }
            Ok(ControlFlow::Continue(()))
        })
        .await?;
    Ok(applied)
}

/// Why replay refused a record.
#[derive(Debug)]
pub(super) enum Error {
    /// A declared record that is neither dentrado's nor format metadata.
    UnexpectedRecord { at: Cursor, id: u16 },
    /// A nested payload names a schema the log never declared.
    Undeclared { at: Cursor, id: u16 },
    /// A nested payload was written with a layout this runtime does not use.
    WrongShape {
        at: Cursor,
        what: &'static str,
        expected: String,
        found: String,
    },
    /// A record's postcard payload does not decode.
    Decode {
        at: Cursor,
        what: &'static str,
        source: postcard::Error,
    },
    /// An event record is shorter than its fixed envelope.
    ShortEvent { at: Cursor },
    /// The cache refused a record.
    Apply { at: Cursor, message: String },
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedRecord { at, id } => {
                write!(f, "{at}: schema {id} is not a dentrado record")
            }
            Self::Undeclared { at, id } => {
                write!(f, "{at}: schema {id} was never declared")
            }
            Self::WrongShape {
                at,
                what,
                expected,
                found,
            } => write!(
                f,
                "{at}: {what} has shape `{found}`, this runtime writes `{expected}`"
            ),
            Self::Decode { at, what, source } => {
                write!(f, "{at}: {what} does not decode: {source}")
            }
            Self::ShortEvent { at } => {
                write!(f, "{at}: an event record is shorter than its header")
            }
            Self::Apply { at, message } => write!(f, "{at}: {message}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Apply {
            at: Cursor::START,
            message: e.to_string(),
        }
    }
}
