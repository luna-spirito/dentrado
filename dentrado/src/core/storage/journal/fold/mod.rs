//! Replaying journal records into a [`Fold`].
//!
//! The journal is domain-blind, so this adapter is where a record regains
//! meaning: a `LocUser` becomes a localized user, a `LocGroup` carries its
//! group's bytes under a declared schema, and an `Event` becomes a
//! [`StoredEvent`] applied to its group's shard. Nested payloads (`group`,
//! `content`, a body) are decoded with the schema id the record names, after
//! checking that the declaration is this runtime's layout.
//!
//! Replay calls the same [`Fold`] transitions the live path uses, so dedup,
//! replacement slots, and the `added`/`removed` changelog are rebuilt by
//! construction rather than by a parallel implementation. Recorded loc ids are
//! restored, not re-derived where that is observable.
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
    core::{
        api::RuntimeTypes,
        loc_ctx::{GroupDelta, StoredEvent},
        storage::fold::{Fold, RestoreError},
    },
    types::{
        DataId, IdentityServerPk, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId,
        NodeId, SenderPk, Shape, UserId,
    },
};

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

    fn apply<R: RuntimeTypes>(
        &self,
        journal: &Journal,
        at: Cursor,
        frame: Frame<'_>,
        fold: &mut Fold<R, ()>,
        on_event: &mut impl FnMut(Cursor, &Event<'_>, GroupDelta),
    ) -> Result<(), FoldError> {
        // Segment zero carries the log's own metadata, not state to fold.
        if matches!(frame.schema, SchemaId::DECL | SchemaId::IDENTITY) {
            return Ok(());
        }
        let Some(kind) = self
            .records
            .iter()
            .find_map(|&(id, kind)| (id == frame.schema).then_some(kind))
        else {
            return Err(FoldError::UnexpectedRecord {
                at,
                id: frame.schema.0,
            });
        };
        match kind {
            Kind::User => {
                let rec: LocUser = decode(at, "loc-user", frame.payload)?;
                fold.restore_user(
                    LocUserId(rec.loc),
                    UserId {
                        id: rec.id,
                        identity_server_pk: IdentityServerPk(rec.identity_server_pk),
                    },
                )
                .map_err(|source| FoldError::Restore { at, source })?;
            }
            Kind::Sender => {
                let rec: LocSender = decode(at, "loc-sender", frame.payload)?;
                fold.restore_sender(
                    LocSenderId(rec.loc),
                    SenderPk(rec.pk),
                    rec.user.map(LocUserId),
                )
                .map_err(|source| FoldError::Restore { at, source })?;
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
                fold.restore_group(LocGroupId(rec.loc), LocMsgTypeId(rec.msg_type), group)
                    .map_err(|source| FoldError::Restore { at, source })?;
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
                fold.restore_data(
                    LocDataId(rec.loc),
                    DataId {
                        timestamp: rec.timestamp,
                        hash: rec.hash,
                    },
                    content,
                )
                .map_err(|source| FoldError::Restore { at, source })?;
            }
            Kind::Event => {
                let ev = Event::decode(frame.payload).ok_or(FoldError::ShortEvent { at })?;
                check_nested(journal, at, "event body", &self.body, ev.body_schema)?;
                if let Some(delta) = fold.store_event(
                    LocGroupId(ev.group),
                    StoredEvent {
                        sender: LocSenderId(ev.sender),
                        tx_id: ev.tx_id,
                        timestamp: ev.timestamp,
                        source_node: NodeId(ev.source_node),
                        body: (),
                    },
                ) {
                    on_event(at, &ev, delta);
                }
            }
        }
        Ok(())
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
) -> Result<T, FoldError> {
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
) -> Result<(), FoldError> {
    let id = SchemaId(id);
    let decl = journal
        .decls()
        .get(id)
        .ok_or(FoldError::Undeclared { at, id: id.0 })?;
    if decl.shape != expected {
        return Err(FoldError::WrongShape {
            at,
            what,
            expected: expected.to_owned(),
            found: decl.shape.clone(),
        });
    }
    Ok(())
}

fn decode<T: DeserializeOwned>(
    at: Cursor,
    what: &'static str,
    bytes: &[u8],
) -> Result<T, FoldError> {
    postcard::from_bytes(bytes).map_err(|source| FoldError::Decode { at, what, source })
}

/// Replays `[from, head)` into `fold`, returning the resume cursor.
///
/// `on_event` sees every event that changed its group (a stale observation is
/// folded but not reported), with the cursor of its record and the slot it
/// landed in. The journaled backend uses it to materialize bodies into its
/// cache; tests pass a no-op.
///
/// # Errors
///
/// [`ReplayError::Log`] if the journal is damaged, [`ReplayError::Visit`] if a
/// record cannot be applied to this runtime's fold (a layout mismatch, a loc
/// id out of order, a malformed payload).
pub(crate) async fn replay<R: RuntimeTypes, F>(
    journal: &Journal,
    schemas: &schema::Schemas,
    from: Cursor,
    fold: &mut Fold<R, ()>,
    mut on_event: F,
) -> Result<Cursor, ReplayError<FoldError>>
where
    F: FnMut(Cursor, &Event<'_>, GroupDelta),
{
    let replay = Replay::new::<R>(schemas);
    journal
        .try_replay_from(from, async |at, frame| {
            replay
                .apply(journal, at, frame, fold, &mut on_event)
                .map(|()| ControlFlow::Continue(()))
        })
        .await
}

/// Why replay refused a record.
#[derive(Debug)]
pub(crate) enum FoldError {
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
    /// A recorded assignment contradicts the fold built so far.
    Restore { at: Cursor, source: RestoreError },
}

impl fmt::Display for FoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedRecord { at, id } => {
                write!(f, "{at}: schema {id} is not a dentrado record")
            }
            Self::Undeclared { at, id } => write!(f, "{at}: schema {id} was never declared"),
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
            Self::Restore { at, source } => write!(f, "{at}: {source}"),
        }
    }
}

impl std::error::Error for FoldError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Decode { source, .. } => Some(source),
            Self::Restore { source, .. } => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
