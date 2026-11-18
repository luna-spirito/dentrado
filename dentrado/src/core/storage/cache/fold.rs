//! The one interpretation of a journal record: identity bytes become forward
//! and reverse directory rows. Live and replay both call these functions; only
//! the [`Sink`] differs.
//!
//! The `*_rows` half is pure: the live path already established that the key
//! is absent, reserves the id under the journal lock, and stages these rows in
//! the same lock section as the append. The `Sink` half is replay's: it reads
//! the existing directory first, repairs a half-written pair, and refuses a
//! different assignment.

use super::{
    codec,
    rows::{Row, Sink},
    tables::Tables,
};
use crate::types::{
    DataId, GroupEventId, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk,
    UserId,
};

pub(crate) fn user_rows(tables: &Tables, loc: LocUserId, uid: UserId) -> Vec<Row> {
    vec![
        Row::new(
            tables.users_fwd,
            codec::user_fwd(uid),
            codec::loc_bytes(loc.0),
        ),
        Row::new(
            tables.users_rev,
            codec::user_rev(loc),
            codec::user_bytes(uid),
        ),
    ]
}

pub(crate) fn sender_rows(
    tables: &Tables,
    loc: LocSenderId,
    pk: SenderPk,
    user: Option<LocUserId>,
) -> Vec<Row> {
    vec![
        Row::new(
            tables.senders_fwd,
            codec::sender_fwd(pk),
            codec::sender_fwd_bytes(loc, user),
        ),
        Row::new(
            tables.senders_rev,
            codec::sender_rev(loc),
            codec::sender_rev_bytes(pk, user),
        ),
    ]
}

pub(crate) fn group_rows(
    tables: &Tables,
    loc: LocGroupId,
    msg_type: LocMsgTypeId,
    group: &[u8],
) -> Vec<Row> {
    vec![Row::new(
        tables.groups_fwd,
        codec::group_fwd(msg_type.0, group),
        codec::loc_bytes(loc.0),
    )]
}

pub(crate) fn data_rows(
    tables: &Tables,
    loc: LocDataId,
    data_id: DataId,
    content: &[u8],
) -> Vec<Row> {
    let mut value = Vec::with_capacity(36 + content.len());
    value.extend_from_slice(&data_id.timestamp.to_le_bytes());
    value.extend_from_slice(&data_id.hash);
    value.extend_from_slice(content);
    vec![
        Row::new(
            tables.data_fwd,
            codec::data_fwd(data_id),
            codec::loc_bytes(loc.0),
        ),
        Row::new(tables.data_rev, codec::data_rev(loc), value),
    ]
}

/// The two rows one event adds. The shard watermark is committed separately.
pub(crate) fn event_rows(
    tables: &Tables,
    group: LocGroupId,
    slot: GroupEventId,
    sender: LocSenderId,
    tx_id: u32,
    value: &[u8],
) -> Vec<Row> {
    vec![
        Row::new(tables.events, codec::event(group, slot), value),
        Row::new(
            tables.event_keys,
            codec::event_key(group, sender, tx_id),
            codec::loc_bytes(slot.0),
        ),
    ]
}

pub(crate) async fn user<S: Sink>(
    sink: &mut S,
    tables: &Tables,
    loc: LocUserId,
    uid: UserId,
) -> Result<(), String> {
    let fk = codec::user_fwd(uid);
    let rk = codec::user_rev(loc);
    let fwd = sink.get(tables.users_fwd, &fk).await;
    let rev = sink.get(tables.users_rev, &rk).await;
    let fwd_ok = fwd.as_deref().and_then(codec::parse_loc) == Some(loc.0);
    let rev_ok = rev.as_deref().and_then(codec::parse_user) == Some(uid);
    match (fwd.is_some(), rev.is_some()) {
        (false, false) => sink.put(user_rows(tables, loc, uid)),
        (true, true) if fwd_ok && rev_ok => {}
        (true, false) if fwd_ok => {
            sink.put(vec![Row::new(tables.users_rev, rk, codec::user_bytes(uid))]);
        }
        (false, true) if rev_ok => {
            sink.put(vec![Row::new(
                tables.users_fwd,
                fk,
                codec::loc_bytes(loc.0),
            )]);
        }
        _ => {
            return Err(format!(
                "user {} (loc {}) is already interned differently",
                uid.id, loc.0
            ));
        }
    }
    Ok(())
}

pub(crate) async fn sender<S: Sink>(
    sink: &mut S,
    tables: &Tables,
    loc: LocSenderId,
    pk: SenderPk,
    user: Option<LocUserId>,
) -> Result<(), String> {
    if let Some(user) = user
        && sink
            .get(tables.users_rev, &codec::user_rev(user))
            .await
            .is_none()
    {
        return Err(format!(
            "sender names user loc {}, which was not applied",
            user.0
        ));
    }
    let fk = codec::sender_fwd(pk);
    let rk = codec::sender_rev(loc);
    let fwd_value = codec::sender_fwd_bytes(loc, user);
    let rev_value = codec::sender_rev_bytes(pk, user);
    let fwd = sink.get(tables.senders_fwd, &fk).await;
    let rev = sink.get(tables.senders_rev, &rk).await;
    match (fwd.as_deref(), rev.as_deref()) {
        (None, None) => sink.put(sender_rows(tables, loc, pk, user)),
        (Some(f), Some(r)) if f == fwd_value.as_slice() && r == rev_value.as_slice() => {}
        (Some(f), None) if f == fwd_value.as_slice() => {
            sink.put(vec![Row::new(tables.senders_rev, rk, rev_value)]);
        }
        (None, Some(r)) if r == rev_value.as_slice() => {
            sink.put(vec![Row::new(tables.senders_fwd, fk, fwd_value)]);
        }
        _ => return Err("a sender is already interned differently".to_owned()),
    }
    Ok(())
}

pub(crate) async fn group<S: Sink>(
    sink: &mut S,
    tables: &Tables,
    loc: LocGroupId,
    msg_type: LocMsgTypeId,
    group: &[u8],
) -> Result<(), String> {
    let key = codec::group_fwd(msg_type.0, group);
    match sink.get(tables.groups_fwd, &key).await {
        Some(bytes) if codec::parse_loc(&bytes) == Some(loc.0) => Ok(()),
        Some(_) => Err(format!(
            "group (msg type {}) is already interned differently",
            msg_type.0
        )),
        None => {
            sink.put(group_rows(tables, loc, msg_type, group));
            Ok(())
        }
    }
}

pub(crate) async fn data<S: Sink>(
    sink: &mut S,
    tables: &Tables,
    loc: LocDataId,
    data_id: DataId,
    content: &[u8],
) -> Result<(), String> {
    let fk = codec::data_fwd(data_id);
    let rk = codec::data_rev(loc);
    let rows = data_rows(tables, loc, data_id, content);
    let value = &rows[1].value;

    let fwd = sink.get(tables.data_fwd, &fk).await;
    let rev = sink.get(tables.data_rev, &rk).await;
    let fwd_ok = fwd.as_deref().and_then(codec::parse_loc) == Some(loc.0);
    let rev_ok = rev
        .as_deref()
        .is_some_and(|bytes| bytes == value.as_slice());
    match (fwd.is_some(), rev.is_some()) {
        (false, false) => sink.put(rows),
        (true, true) if fwd_ok && rev_ok => {}
        (true, false) if fwd_ok => sink.put(vec![rows.into_iter().nth(1).expect("rev row")]),
        (false, true) if rev_ok => {
            sink.put(vec![Row::new(tables.data_fwd, fk, codec::loc_bytes(loc.0))]);
        }
        _ => {
            return Err(format!(
                "data {:?} (loc {}) is already interned differently",
                data_id, loc.0
            ));
        }
    }
    Ok(())
}
