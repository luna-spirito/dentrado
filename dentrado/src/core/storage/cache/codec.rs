//! The cache's key and value encodings.
//!
//! Every encoding is little-endian. Keys are fixed-width or a fixed-width
//! prefix followed by the value's bytes; values are fixed-width records or raw
//! postcard bytes. The tables are separate, so keys carry no entity tags.

use crate::types::{
    DataId, GroupEventId, LocDataId, LocGroupId, LocSenderId, LocUserId, SenderPk, UserId,
};

fn key(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(parts.iter().map(|p| p.len()).sum());
    for part in parts {
        out.extend_from_slice(part);
    }
    out
}

fn le8(value: u64) -> [u8; 8] {
    value.to_le_bytes()
}

// ── Localization keys ────────────────────────────────────────────────────

pub(crate) fn user_fwd(uid: UserId) -> Vec<u8> {
    key(&[&uid.id.to_le_bytes(), &uid.identity_server_pk.0])
}

pub(crate) fn user_rev(loc: LocUserId) -> [u8; 8] {
    le8(loc.0)
}

pub(crate) fn sender_fwd(pk: SenderPk) -> [u8; 32] {
    pk.0
}

pub(crate) fn sender_rev(loc: LocSenderId) -> [u8; 8] {
    le8(loc.0)
}

pub(crate) fn group_fwd(msg_type: u64, group: &[u8]) -> Vec<u8> {
    key(&[&le8(msg_type), group])
}

pub(crate) fn data_fwd(data: DataId) -> Vec<u8> {
    key(&[&data.timestamp.to_le_bytes(), &data.hash])
}

pub(crate) fn data_rev(loc: LocDataId) -> [u8; 8] {
    le8(loc.0)
}

// ── Event keys ───────────────────────────────────────────────────────────

pub(crate) fn event(group: LocGroupId, slot: GroupEventId) -> Vec<u8> {
    key(&[&le8(group.0), &le8(slot.0)])
}

/// The dedup key: `group | sender | tx_id`. One group maps to one core, so the
/// core id would carry no information and is not part of the key.
pub(crate) fn event_key(group: LocGroupId, sender: LocSenderId, tx_id: u32) -> Vec<u8> {
    key(&[&le8(group.0), &le8(sender.0), &tx_id.to_le_bytes()])
}

pub(crate) fn shard(group: LocGroupId) -> [u8; 8] {
    le8(group.0)
}

// ── Values ───────────────────────────────────────────────────────────────

/// A local id value (forward maps, dedup slots).
pub(crate) fn loc_bytes(loc: u64) -> [u8; 8] {
    le8(loc)
}

pub(crate) fn parse_loc(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?))
}

/// The reverse data value: the content address and the raw content bytes.
pub(crate) fn parse_data_value(bytes: &[u8]) -> Option<(DataId, &[u8])> {
    Some((
        DataId {
            timestamp: u32::from_le_bytes(bytes.get(..4)?.try_into().ok()?),
            hash: bytes.get(4..36)?.try_into().ok()?,
        },
        bytes.get(36..)?,
    ))
}

/// A user's raw identity: `id | identity_server_pk`.
pub(crate) type UserBytes = [u8; 40];

pub(crate) fn user_bytes(uid: UserId) -> UserBytes {
    let mut out = [0u8; 40];
    out[..8].copy_from_slice(&uid.id.to_le_bytes());
    out[8..].copy_from_slice(&uid.identity_server_pk.0);
    out
}

pub(crate) fn parse_user(bytes: &[u8]) -> Option<UserId> {
    let id = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let pk = bytes.get(8..40)?.try_into().ok()?;
    Some(UserId {
        id,
        identity_server_pk: crate::types::IdentityServerPk(pk),
    })
}

/// The forward sender value: `loc | has_user | [user_loc]`.
pub(crate) fn sender_fwd_bytes(loc: LocSenderId, user: Option<LocUserId>) -> Vec<u8> {
    let mut out = Vec::with_capacity(17);
    out.extend_from_slice(&loc.0.to_le_bytes());
    append_user(&mut out, user);
    out
}

/// The reverse sender value: `pk | has_user | [user_loc]`.
pub(crate) fn sender_rev_bytes(pk: SenderPk, user: Option<LocUserId>) -> Vec<u8> {
    let mut out = Vec::with_capacity(41);
    out.extend_from_slice(&pk.0);
    append_user(&mut out, user);
    out
}

fn append_user(out: &mut Vec<u8>, user: Option<LocUserId>) {
    match user {
        Some(user) => {
            out.push(1);
            out.extend_from_slice(&user.0.to_le_bytes());
        }
        None => out.push(0),
    }
}

/// A parsed sender's optional user reference: the sender either has no local
/// user or is bound to one.
enum SenderUser {
    Absent,
    Bound(LocUserId),
}

impl SenderUser {
    fn option(self) -> Option<LocUserId> {
        match self {
            Self::Absent => None,
            Self::Bound(user) => Some(user),
        }
    }
}

fn parse_user_ref(bytes: &[u8], at: usize) -> Option<SenderUser> {
    match bytes.get(at)? {
        0 => Some(SenderUser::Absent),
        1 => Some(SenderUser::Bound(LocUserId(u64::from_le_bytes(
            bytes.get(at + 1..at + 9)?.try_into().ok()?,
        )))),
        _ => None,
    }
}

pub(crate) fn parse_sender_fwd(bytes: &[u8]) -> Option<(LocSenderId, Option<LocUserId>)> {
    let loc = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    Some((LocSenderId(loc), parse_user_ref(bytes, 8)?.option()))
}

pub(crate) fn parse_sender_rev(bytes: &[u8]) -> Option<(SenderPk, Option<LocUserId>)> {
    let pk = bytes.get(..32)?.try_into().ok()?;
    Some((SenderPk(pk), parse_user_ref(bytes, 32)?.option()))
}

// ── Event values ─────────────────────────────────────────────────────────

/// Event value: `sender | tx_id | timestamp | source_node | body`.
pub(crate) fn event_bytes(
    sender: u64,
    tx_id: u32,
    timestamp: u32,
    source_node: u32,
    body: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(20 + body.len());
    out.extend_from_slice(&sender.to_le_bytes());
    out.extend_from_slice(&tx_id.to_le_bytes());
    out.extend_from_slice(&timestamp.to_le_bytes());
    out.extend_from_slice(&source_node.to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// The parsed event envelope plus the raw body bytes.
pub(crate) struct EventParts<'a> {
    pub sender: u64,
    pub tx_id: u32,
    pub timestamp: u32,
    pub source_node: u32,
    pub body: &'a [u8],
}

pub(crate) fn parse_event(bytes: &[u8]) -> Option<EventParts<'_>> {
    let sender = u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?);
    let tx_id = u32::from_le_bytes(bytes.get(8..12)?.try_into().ok()?);
    let timestamp = u32::from_le_bytes(bytes.get(12..16)?.try_into().ok()?);
    let source_node = u32::from_le_bytes(bytes.get(16..20)?.try_into().ok()?);
    Some(EventParts {
        sender,
        tx_id,
        timestamp,
        source_node,
        body: bytes.get(20..)?,
    })
}

/// A shard's counters: `added | removed`.
pub(crate) fn shard_bytes(added: u64, removed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(16);
    out.extend_from_slice(&added.to_le_bytes());
    out.extend_from_slice(&removed.to_le_bytes());
    out
}

pub(crate) fn parse_shard(bytes: &[u8]) -> Option<(u64, u64)> {
    Some((
        u64::from_le_bytes(bytes.get(..8)?.try_into().ok()?),
        u64::from_le_bytes(bytes.get(8..16)?.try_into().ok()?),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::IdentityServerPk;

    #[test]
    fn a_user_round_trips() {
        let uid = UserId {
            id: 7,
            identity_server_pk: IdentityServerPk([9; 32]),
        };
        assert_eq!(parse_user(&user_bytes(uid)), Some(uid));
    }

    #[test]
    fn a_sender_round_trips_with_and_without_a_user() {
        let sid = LocSenderId(3);
        let user = Some(LocUserId(2));
        assert_eq!(
            parse_sender_fwd(&sender_fwd_bytes(sid, user)),
            Some((sid, user))
        );
        assert_eq!(
            parse_sender_rev(&sender_rev_bytes(SenderPk([1; 32]), None)),
            Some((SenderPk([1; 32]), None))
        );
        assert_eq!(parse_sender_fwd(&[0u8; 8]), None);
    }

    #[test]
    fn an_event_round_trips_its_body_verbatim() {
        let value = event_bytes(1, 2, 3, 4, b"body");
        let parts = parse_event(&value).unwrap();
        assert_eq!(
            (
                parts.sender,
                parts.tx_id,
                parts.timestamp,
                parts.source_node,
                parts.body
            ),
            (1, 2, 3, 4, &b"body"[..])
        );
        assert!(parse_event(&[0u8; 19]).is_none());
    }
}
