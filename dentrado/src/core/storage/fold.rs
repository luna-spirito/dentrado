//! The layer 1–2 state machine: localization maps, per-group event shards,
//! and content-addressed data.
//!
//! This is the state every backend must keep, separated from *where* it is
//! kept. [`InMemoryStorage`](super::in_memory::InMemoryStorage) owns one behind
//! a `RefCell` and serves it from RAM; the journaled backend
//! (`journal::JournalStorage`) owns one and rebuilds it by replaying the log.
//! Both call the same transitions, so the live path and replay cannot disagree
//! about dedup, slots, or the `removed` changelog.
//!
//! The fold is synchronous: it has no IO and no views. A backend wraps it in
//! whatever interior mutability its callers need.
//!
//! ## Allocators are counters, not row counts
//!
//! A fresh loc id is `next_*`, a monotonic counter, never the current map
//! length. Replay restores recorded ids, so the counter must be at least one
//! past every id it has seen; using a length would alias an id as soon as the
//! recorded sequence has a hole (eviction, purge, a future migration). The
//! recorded ids are dense today, and [`Fold::restore_*`] enforces that, but the
//! allocator stays correct if that changes.

use std::collections::HashMap;
use std::fmt;

use crate::{
    core::{
        api::RuntimeTypes,
        loc_ctx::{EventGroup, GroupDelta, StoredEvent},
        storage::{GroupDiff, Watermark},
    },
    types::{
        DataId, DataVerifyError, GlobalHash, GlobalResolver, GroupEventId, GroupRouteError,
        LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId, SenderPk, UserId,
    },
};

/// The layer 1–2 state: everything below the gear's private view.
///
/// `B` is how a backend chooses to keep event bodies: the RAM backend stores
/// them (`B = R::Body`), while the journaled backend keeps only the envelope in
/// the fold and materializes bodies in its cache (`B = ()`). The transitions
/// that decide slots, dedup, and the changelog do not depend on the body, so
/// both backends run the same code.
pub(crate) struct Fold<R: RuntimeTypes, B> {
    // localization
    pk_to_sender: HashMap<SenderPk, LocSenderId>,
    sender_to_pk: HashMap<LocSenderId, SenderPk>,
    sender_to_user: HashMap<LocSenderId, LocUserId>,
    user_id_to_local: HashMap<UserId, LocUserId>,
    local_to_user_id: HashMap<LocUserId, UserId>,
    // event log: one append-only shard per group (bodies + dedup + changelog).
    events_by_group: HashMap<LocGroupId, EventGroup<B>>,
    // content-addressed data
    data_by_id: Vec<(DataId, R::Data)>,
    data_id_to_local: HashMap<DataId, LocDataId>,
    // group localization
    group_by_key: HashMap<(LocMsgTypeId, R::Group), LocGroupId>,
    // allocators: one past the last id ever assigned, never a map length.
    next_user: u64,
    next_sender: u64,
    next_group: u64,
    next_data: u64,
}

/// What a replay built, for the startup log: how much localization, content
/// and event state the fold now holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct FoldSize {
    pub(crate) users: usize,
    pub(crate) senders: usize,
    pub(crate) groups: usize,
    pub(crate) data: usize,
    /// Every observation that became a slot (superseded bodies stay).
    pub(crate) events: usize,
    /// Slots still live after supersede.
    pub(crate) live: usize,
}

impl fmt::Display for FoldSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} users, {} senders, {} groups, {} data, {} events ({} live)",
            self.users, self.senders, self.groups, self.data, self.events, self.live
        )
    }
}

impl<R: RuntimeTypes, B> Fold<R, B> {
    pub(crate) fn new() -> Self {
        Self {
            pk_to_sender: HashMap::new(),
            sender_to_pk: HashMap::new(),
            sender_to_user: HashMap::new(),
            user_id_to_local: HashMap::new(),
            local_to_user_id: HashMap::new(),
            events_by_group: HashMap::new(),
            data_by_id: Vec::new(),
            data_id_to_local: HashMap::new(),
            group_by_key: HashMap::new(),
            next_user: 0,
            next_sender: 0,
            next_group: 0,
            next_data: 0,
        }
    }

    /// How much state this fold holds, for the startup log.
    pub(crate) fn size(&self) -> FoldSize {
        FoldSize {
            users: self.local_to_user_id.len(),
            senders: self.sender_to_pk.len(),
            groups: self.group_by_key.len(),
            data: self.data_by_id.len(),
            events: self.events_by_group.values().map(|g| g.bodies.len()).sum(),
            live: self.events_by_group.values().map(|g| g.dedup.len()).sum(),
        }
    }

    // ── localization: idempotent allocators ───────────────────────────────

    /// The local id `uid` already has, without allocating one.
    pub(crate) fn loc_user(&self, uid: UserId) -> Option<LocUserId> {
        self.user_id_to_local.get(&uid).copied()
    }

    /// The local id `pk` already has, without allocating one.
    pub(crate) fn loc_sender(&self, pk: SenderPk) -> Option<LocSenderId> {
        self.pk_to_sender.get(&pk).copied()
    }

    /// The local id `(msg_type, group)` already has, without allocating one.
    pub(crate) fn loc_group(&self, msg_type: LocMsgTypeId, group: &R::Group) -> Option<LocGroupId> {
        self.group_by_key.get(&(msg_type, group.clone())).copied()
    }

    /// The local id of `uid`, assigned on first sight.
    pub(crate) fn mk_loc_user(&mut self, uid: UserId) -> LocUserId {
        if let Some(loc) = self.loc_user(uid) {
            return loc;
        }
        let loc = LocUserId(self.next_user);
        self.next_user += 1;
        self.attach_user(loc, uid);
        loc
    }

    /// The local id of `pk`, assigned on first sight. With a `uid`, the user
    /// is interned first (inline, because this is one borrow scope).
    pub(crate) fn mk_loc_sender(&mut self, pk: SenderPk, uid: Option<UserId>) -> LocSenderId {
        let user = uid.map(|uid| self.mk_loc_user(uid));
        if let Some(loc) = self.loc_sender(pk) {
            return loc;
        }
        let loc = LocSenderId(self.next_sender);
        self.next_sender += 1;
        self.pk_to_sender.insert(pk, loc);
        self.sender_to_pk.insert(loc, pk);
        if let Some(user) = user {
            self.sender_to_user.insert(loc, user);
        }
        loc
    }

    /// The local id of `(msg_type, group)`, assigned on first sight.
    pub(crate) fn mk_loc_group(&mut self, msg_type: LocMsgTypeId, group: R::Group) -> LocGroupId {
        if let Some(loc) = self.loc_group(msg_type, &group) {
            return loc;
        }
        let loc = LocGroupId(self.next_group);
        self.next_group += 1;
        self.group_by_key.insert((msg_type, group), loc);
        loc
    }

    /// The local id of `data_id`, verified against `content` before it is
    /// recorded.
    ///
    /// # Errors
    ///
    /// [`DataVerifyError`] if `content` does not hash to `data_id`, or embeds a
    /// [`LocDataId`] the fold cannot resolve.
    pub(crate) fn mk_data(
        &mut self,
        data_id: DataId,
        content: R::Data,
    ) -> Result<LocDataId, DataVerifyError> {
        if let Some(&loc) = self.data_id_to_local.get(&data_id) {
            return Ok(loc);
        }
        self.verify_data(data_id, &content)?;
        let loc = LocDataId(self.next_data);
        self.next_data += 1;
        self.data_id_to_local.insert(data_id, loc);
        self.data_by_id.push((data_id, content));
        Ok(loc)
    }

    fn verify_data(&self, data_id: DataId, content: &R::Data) -> Result<(), DataVerifyError> {
        match content.global_hash(self) {
            Ok(hash) if hash == data_id.hash => Ok(()),
            Ok(computed_hash) => Err(DataVerifyError::HashMismatch {
                claimed: data_id,
                computed_hash,
            }),
            Err(e) => Err(DataVerifyError::UnresolvableId(e)),
        }
    }

    // ── localization: reverse lookups ─────────────────────────────────────

    pub(crate) fn user_by_local(&self, loc: LocUserId) -> Option<UserId> {
        self.local_to_user_id.get(&loc).copied()
    }

    pub(crate) fn sender_user(&self, loc: LocSenderId) -> Option<LocUserId> {
        self.sender_to_user.get(&loc).copied()
    }

    pub(crate) fn sender_pk(&self, loc: LocSenderId) -> Option<SenderPk> {
        self.sender_to_pk.get(&loc).copied()
    }

    pub(crate) fn find_data(&self, data_id: &DataId) -> Option<LocDataId> {
        self.data_id_to_local.get(data_id).copied()
    }

    pub(crate) fn fetch_data(&self, loc: LocDataId) -> Option<(DataId, R::Data)> {
        self.data_by_id
            .get(loc.0 as usize)
            .map(|(id, content)| (*id, content.clone()))
    }

    // ── event log: one append-only shard per group ───────────────────────

    /// Add `ev` to `group`'s shard with dedup by `(sender, tx_id)`.
    ///
    /// Returns the delta the observation applied, or `None` when it changes
    /// nothing. Nothing is stale today — a conflicting observation is a
    /// `todo!()` — so this is always `Some`. Bodies are never deleted: a
    /// superseded version stays fetchable by slot, and removal is a changelog
    /// entry.
    ///
    /// # Supersede is not implemented
    ///
    /// A second observation of a known `(sender, tx_id)` is a `todo!()`. The
    /// decided tiebreak is `(authoritative, timestamp, source_node)`: an
    /// observation made by the group's target homeserver outranks a relayed
    /// copy of the same event, and the earliest wins within one rank. The
    /// engine records neither a group's target homeserver nor a node's
    /// homeserver, so nothing can rank an observation yet — failing loudly
    /// beats silently picking one, which is what the old
    /// `(timestamp, source_node)` order did.
    ///
    /// # Panics
    ///
    /// If `group` already observed `(ev.sender, ev.tx_id)`.
    pub(crate) fn store_event(
        &mut self,
        group: LocGroupId,
        ev: StoredEvent<B>,
    ) -> Option<GroupDelta> {
        let g = self.events_by_group.entry(group).or_default();
        let key = (ev.sender, ev.tx_id);
        if g.dedup.contains_key(&key) {
            todo!("supersede: rank by (authoritative, timestamp, source_node)");
        }
        let slot = GroupEventId(g.bodies.len() as u64);
        g.bodies.push(ev);
        g.dedup.insert(key, slot.0);
        Some(GroupDelta {
            added: Some(slot),
            removed: None,
        })
    }

    pub(crate) fn fetch_event(
        &self,
        group: LocGroupId,
        slot: GroupEventId,
    ) -> Option<StoredEvent<B>>
    where
        B: Clone,
    {
        self.events_by_group
            .get(&group)
            .and_then(|g| g.bodies.get(slot.0 as usize))
            .cloned()
    }

    pub(crate) fn diff_group(&self, group: LocGroupId, since: Watermark) -> GroupDiff {
        let Some(g) = self.events_by_group.get(&group) else {
            return GroupDiff {
                added: Vec::new(),
                removed: Vec::new(),
                watermark: since,
            };
        };
        // Only an accepted observation gets a body, so the added slots are
        // exactly `0..bodies.len()`: no changelog to walk.
        let a = (since.added as usize).min(g.bodies.len());
        let r = (since.removed as usize).min(g.removed.len());
        GroupDiff {
            added: (a..g.bodies.len())
                .map(|slot| GroupEventId(slot as u64))
                .collect(),
            removed: g.removed[r..].to_vec(),
            watermark: Watermark {
                added: g.bodies.len() as u64,
                removed: g.removed.len() as u64,
            },
        }
    }

    // ── replay: restoring recorded assignments ─────────────────────────────
    //
    // The live allocators derive the next id from a counter; replay must keep
    // the id the log recorded. These methods are the only way a fold accepts
    // an assignment that was not just allocated, and they refuse anything the
    // live path could not have produced.

    pub(crate) fn restore_user(&mut self, loc: LocUserId, uid: UserId) -> Result<(), RestoreError> {
        expect_next("user", loc.0, self.next_user)?;
        if self.user_id_to_local.contains_key(&uid) {
            return Err(RestoreError::Duplicate {
                kind: "user",
                loc: loc.0,
            });
        }
        self.next_user = loc.0 + 1;
        self.attach_user(loc, uid);
        Ok(())
    }

    pub(crate) fn restore_sender(
        &mut self,
        loc: LocSenderId,
        pk: SenderPk,
        user: Option<LocUserId>,
    ) -> Result<(), RestoreError> {
        expect_next("sender", loc.0, self.next_sender)?;
        if self.pk_to_sender.contains_key(&pk) {
            return Err(RestoreError::Duplicate {
                kind: "sender",
                loc: loc.0,
            });
        }
        if let Some(user) = user
            && !self.local_to_user_id.contains_key(&user)
        {
            return Err(RestoreError::UnknownUser { loc: user.0 });
        }
        self.next_sender = loc.0 + 1;
        self.pk_to_sender.insert(pk, loc);
        self.sender_to_pk.insert(loc, pk);
        if let Some(user) = user {
            self.sender_to_user.insert(loc, user);
        }
        Ok(())
    }

    pub(crate) fn restore_group(
        &mut self,
        loc: LocGroupId,
        msg_type: LocMsgTypeId,
        group: R::Group,
    ) -> Result<(), RestoreError> {
        expect_next("group", loc.0, self.next_group)?;
        let key = (msg_type, group);
        if self.group_by_key.contains_key(&key) {
            return Err(RestoreError::Duplicate {
                kind: "group",
                loc: loc.0,
            });
        }
        self.next_group = loc.0 + 1;
        self.group_by_key.insert(key, loc);
        Ok(())
    }

    pub(crate) fn restore_data(
        &mut self,
        loc: LocDataId,
        data_id: DataId,
        content: R::Data,
    ) -> Result<(), RestoreError> {
        expect_next("data", loc.0, self.next_data)?;
        if self.data_id_to_local.contains_key(&data_id) {
            return Err(RestoreError::Duplicate {
                kind: "data",
                loc: loc.0,
            });
        }
        self.verify_data(data_id, &content)
            .map_err(RestoreError::Data)?;
        self.next_data = loc.0 + 1;
        self.data_id_to_local.insert(data_id, loc);
        self.data_by_id.push((data_id, content));
        Ok(())
    }

    fn attach_user(&mut self, loc: LocUserId, uid: UserId) {
        self.user_id_to_local.insert(uid, loc);
        self.local_to_user_id.insert(loc, uid);
    }
}

impl<R: RuntimeTypes, B> Default for Fold<R, B> {
    fn default() -> Self {
        Self::new()
    }
}

// `mk_data` verifies content hashes by resolving embedded `LocDataId` refs
// against this fold, so the fold is its own resolver.
impl<R: RuntimeTypes, B> GlobalResolver for Fold<R, B> {
    fn resolve_user(&self, loc: LocUserId) -> Result<UserId, GroupRouteError> {
        self.local_to_user_id
            .get(&loc)
            .copied()
            .ok_or(GroupRouteError::UserIdOutOfBounds {
                idx: loc.0,
                users_len: self.user_id_to_local.len(),
            })
    }

    fn resolve_data(&self, loc: LocDataId) -> Result<DataId, GroupRouteError> {
        self.data_by_id
            .get(loc.0 as usize)
            .map(|(id, _)| *id)
            .ok_or(GroupRouteError::DataIdOutOfBounds {
                idx: loc.0,
                objects_len: self.data_by_id.len(),
            })
    }
}

/// Why a recorded assignment cannot be restored into a fold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreError {
    /// The record's loc id is not the next one. Assignments are replayed in
    /// the order they were allocated, so a hole means a missing record.
    OutOfOrder {
        kind: &'static str,
        loc: u64,
        next: u64,
    },
    /// The object is already interned, under another loc id.
    Duplicate { kind: &'static str, loc: u64 },
    /// A sender names a user whose record was not replayed first.
    UnknownUser { loc: u64 },
    /// A data record's content does not hash to the address it claims.
    Data(DataVerifyError),
}

impl fmt::Display for RestoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfOrder { kind, loc, next } => {
                write!(f, "{kind} loc {loc} is out of order (next is {next})")
            }
            Self::Duplicate { kind, loc } => {
                write!(f, "{kind} loc {loc} is already assigned")
            }
            Self::UnknownUser { loc } => {
                write!(f, "sender names user loc {loc}, which was not restored")
            }
            Self::Data(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for RestoreError {}

fn expect_next(kind: &'static str, loc: u64, next: u64) -> Result<(), RestoreError> {
    if loc == next {
        Ok(())
    } else {
        Err(RestoreError::OutOfOrder { kind, loc, next })
    }
}
