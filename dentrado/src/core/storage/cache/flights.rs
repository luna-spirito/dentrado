//! Per-key single-flight dispatch for the opened backend.
//!
//! Every allocation routes through [`coalesce`](super::delta::coalesce), so
//! concurrent callers for one identity share a result while different
//! identities proceed independently. The disk is erased here: both arms call
//! the same `live` function, only the cache/journal handles differ.

use std::rc::Rc;

use dentrado_cache::disk::{FsDisk, MemDisk};

use super::{
    delta::{EventFlight, Flight, coalesce},
    live::{self, Disk, Opened},
};
use crate::{
    core::{api::RuntimeTypes, loc_ctx::StoredEvent},
    types::{
        DataId, DataVerifyError, LocDataId, LocGroupId, LocMsgTypeId, LocSenderId, LocUserId,
        SenderPk, UserId,
    },
};

/// Start (or join) the flight for `$key`, running `live::$call` over either
/// disk. `$call` may name one runtime type (`ensure_data<R>`); the disk type is
/// appended here, and `$call` receives `(cache, tables, seq, journal, $args…)`.
macro_rules! flight {
    ($self:ident, $map:ident, $key:expr, $call:ident $(<$gen:ty>)? $(, $arg:expr)*) => {
        match &$self.disk {
            Disk::Ram { cache } => coalesce(
                &$self.seq.$map,
                $key,
                live::$call::<$($gen,)? MemDisk>(
                    Rc::clone(cache),
                    $self.tables.clone(),
                    Rc::clone(&$self.seq),
                    None,
                    $($arg),*
                ),
            ),
            Disk::Fs { cache, journal, .. } => coalesce(
                &$self.seq.$map,
                $key,
                live::$call::<$($gen,)? FsDisk>(
                    Rc::clone(cache),
                    $self.tables.clone(),
                    Rc::clone(&$self.seq),
                    Some(Rc::clone(journal)),
                    $($arg),*
                ),
            ),
        }
    };
}

impl<R: RuntimeTypes> Opened<R> {
    pub(super) fn user_flight(&self, uid: UserId) -> Flight<LocUserId> {
        flight!(self, users, uid, ensure_user, uid).0
    }

    pub(super) fn sender_flight(&self, pk: SenderPk, uid: Option<UserId>) -> Flight<LocSenderId> {
        flight!(self, senders, pk, ensure_sender, pk, uid).0
    }

    pub(super) fn group_flight(
        &self,
        msg_type: LocMsgTypeId,
        bytes: Vec<u8>,
    ) -> Flight<LocGroupId> {
        let key = (msg_type, bytes.clone().into_boxed_slice());
        flight!(self, groups, key, ensure_group, msg_type, bytes).0
    }

    pub(super) fn data_flight(
        &self,
        data_id: DataId,
        content: R::Data,
    ) -> Flight<Result<LocDataId, DataVerifyError>> {
        flight!(self, data, data_id, ensure_data<R>, data_id, content).0
    }

    /// The event flight and whether this caller created it. A joiner sees the
    /// same observation but must not report the owner's delta.
    pub(super) fn event_flight(
        &self,
        key: (LocGroupId, LocSenderId, u32),
        ev: StoredEvent<R::Body>,
        value: Vec<u8>,
    ) -> (Flight<EventFlight>, bool) {
        flight!(self, event_flights, key, ensure_event<R>, key.0, ev, value)
    }
}
