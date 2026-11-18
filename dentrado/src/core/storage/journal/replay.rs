//! Replaying journal records into any layer 1–2 sink.
//!
//! The journal is domain-blind, so this adapter is where a record regains
//! meaning: a `LocUser` becomes a localized user, a `LocGroup` carries its
//! group value under a declared schema, and an `Event` becomes an observation
//! of its group. Nested payloads (`group`, `content`, a body) are decoded with
//! the schema id the record names, after checking that the declaration is this
//! runtime's layout.
//!
//! The sink is a trait rather than a concrete fold so the only implementation
//! — the page-cache materialization — lives with the storage backend, while
//! replay itself stays a pure journal concern.
//!
//! Declarations and the log's identity are format metadata: the adapter skips
//! them. Any other schema id must name a dentrado record; an unknown one is a
//! log this build cannot interpret, not something to guess about.

use std::{fmt, ops::ControlFlow};

use dentrado_journal::{Cursor, Frame, Journal, ReplayError, SchemaId};
use serde::de::DeserializeOwned;

use super::{
    record::{Event, LocData, LocGroup, LocSender, LocUser},
    schema,
};
use crate::{
    core::api::RuntimeTypes,
    types::{
        DataId, IdentityServerPk, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId,
        SenderPk, Shape, UserId,
    },
};

/// Where replay's records go. Every method is idempotent: replay may restart
/// after a crash and apply the same record again.
// REVIEW: What's the point of this abstraction, again?
pub(crate) trait Sink<R: RuntimeTypes> {
    async fn user(&mut self, loc: LocUserId, uid: UserId) -> Result<(), String>;
    async fn sender(
        &mut self,
        loc: LocSenderId,
        pk: SenderPk,
        user: Option<LocUserId>,
    ) -> Result<(), String>;
    async fn group(
        &mut self,
        loc: LocGroupId,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> Result<(), String>;
    async fn data(
        &mut self,
        loc: LocDataId,
        data_id: DataId,
        content: R::Data,
    ) -> Result<(), String>;
    async fn event(&mut self, group: LocGroupId, ev: &Event<'_>) -> Result<(), String>;
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
/// expects. The writer ([`schema::Schemas::declare`]) assigned these ids, so
/// replay never searches by shape at runtime.
struct Replay {
    records: Vec<(SchemaId, Kind)>,
    group: String,
    data: String,
    body: String,
}

impl Replay {
    fn new<R: RuntimeTypes>(schemas: &schema::Schemas) -> Self {
        let records = vec![
            (schemas.user, Kind::User),
            (schemas.sender, Kind::Sender),
            (schemas.group, Kind::Group),
            (schemas.data, Kind::Data),
            (schemas.event, Kind::Event),
        ];
        Self {
            records,
            group: R::Group::shape(),
            data: R::Data::shape(),
            body: R::Body::shape(),
        }
    }

    async fn apply<R: RuntimeTypes, S: Sink<R> + ?Sized>(
        &self,
        journal: &Journal,
        at: Cursor,
        frame: Frame<'_>,
        sink: &mut S,
    ) -> Result<(), Error> {
        // Segment zero carries the log's own metadata, not state to apply.
        if matches!(frame.schema, SchemaId::DECL | SchemaId::IDENTITY) {
            return Ok(());
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
        let applied = match kind {
            Kind::User => {
                let rec: LocUser = decode(at, "loc-user", frame.payload)?;
                sink.user(
                    LocUserId(rec.loc),
                    UserId {
                        id: rec.id,
                        identity_server_pk: IdentityServerPk(rec.identity_server_pk),
                    },
                )
                .await
            }
            Kind::Sender => {
                let rec: LocSender = decode(at, "loc-sender", frame.payload)?;
                sink.sender(
                    LocSenderId(rec.loc),
                    SenderPk(rec.pk),
                    rec.user.map(LocUserId),
                )
                .await
            }
            Kind::Group => {
                let rec: LocGroup = decode(at, "loc-group", frame.payload)?;
                let group = decode_nested(
                    journal,
                    at,
                    "group",
                    &self.group,
                    rec.group_schema,
                    &rec.group,
                )?;
                sink.group(LocGroupId(rec.loc), LocMsgTypeId(rec.msg_type), group)
                    .await
            }
            Kind::Data => {
                let rec: LocData = decode(at, "loc-data", frame.payload)?;
                let content = decode_nested(
                    journal,
                    at,
                    "data",
                    &self.data,
                    rec.content_schema,
                    &rec.content,
                )?;
                sink.data(
                    LocDataId(rec.loc),
                    DataId {
                        timestamp: rec.timestamp,
                        hash: rec.hash,
                    },
                    content,
                )
                .await
            }
            Kind::Event => {
                let ev = Event::decode(frame.payload).ok_or(Error::ShortEvent { at })?;
                check_nested(journal, at, "event body", &self.body, ev.body_schema)?;
                sink.event(LocGroupId(ev.group), &ev).await
            }
        };
        applied.map_err(|message| Error::Apply { at, message })
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

/// Replays `[from, head)` into `sink`, returning the resume cursor.
///
/// # Errors
///
/// [`ReplayError::Log`] if the journal is damaged, [`ReplayError::Visit`] if a
/// record cannot be applied to this runtime (a layout mismatch, a loc id out of
/// order, a malformed payload).
pub(crate) async fn replay<R: RuntimeTypes, S: Sink<R> + ?Sized>(
    journal: &Journal,
    schemas: &schema::Schemas,
    from: Cursor,
    sink: &mut S,
) -> Result<Cursor, ReplayError<Error>> {
    let replay = Replay::new::<R>(schemas);
    journal
        .try_replay_from(from, async |at, frame| {
            replay
                .apply(journal, at, frame, sink)
                .await
                .map(|()| ControlFlow::Continue(()))
        })
        .await
}

/// Why replay refused a record.
#[derive(Debug)]
pub(crate) enum Error {
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
    /// The sink refused a record.
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
