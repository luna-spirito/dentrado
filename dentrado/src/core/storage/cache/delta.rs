//! The RAM coordination layer: counters, the delta overlay, per-key queues,
//! and the batches waiting for a commit.
//!
//! Localization rows live in `delta` from the moment their journal record is
//! appended, so a later operation in the same batch can resolve them. Event
//! rows do not: they stay in `pending` and are invisible to readers until the
//! `commit_log` that makes them durable writes them to the cache. The delta is
//! bounded by writes in flight, never by the number of identities on disk.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    future::Future,
    rc::Rc,
};

use dentrado_cache::{Storage, TableId, disk::IsFilesystem};
use dentrado_journal::Cursor;
use futures::{
    FutureExt,
    future::{LocalBoxFuture, Shared},
};

use super::{codec, rows::Row};
use crate::{
    core::{
        loc_ctx::GroupDelta,
        storage::{GroupDiff, Watermark},
    },
    types::{DataId, GroupEventId, LocDataId, LocGroupId, LocSenderId, LocUserId, UserId},
};

/// The allocator families.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Counter {
    User,
    Sender,
    Group,
    Data,
}

impl Counter {
    pub(crate) const fn index(self) -> usize {
        match self {
            Self::User => 0,
            Self::Sender => 1,
            Self::Group => 2,
            Self::Data => 3,
        }
    }
}

/// A shared, cloneable result. The first caller runs the work; everyone who
/// arrives before it completes awaits the same future.
pub(crate) type Flight<T> = Shared<LocalBoxFuture<'static, T>>;

/// The in-flight table for one key type.
pub(crate) type Flights<K, V> = Rc<RefCell<HashMap<K, Flight<V>>>>;

/// What one event flight produced. `value` is the encoded observation, so a
/// concurrent caller with the same `(sender, tx_id)` but different bytes can
/// still detect the conflict.
#[derive(Clone)]
pub(crate) struct EventFlight {
    pub delta: Option<GroupDelta>,
    pub value: Vec<u8>,
}

/// The dedup key of an observation.
pub(crate) type EventKey = (LocGroupId, LocSenderId, u32);

/// The overlay: uncommitted localization rows, grouped by table so a lookup
/// borrows the key instead of copying it.
type Delta = HashMap<TableId, HashMap<Vec<u8>, Vec<u8>>>;

/// A group's next slot and removal counter.
#[derive(Clone, Copy)]
struct Shard {
    next: u64,
    removed: u64,
}

/// One journal append waiting for the commit that makes it durable.
struct Pending {
    cursor: Option<Cursor>,
    rows: Vec<Row>,
    /// Events only: the dedup key and slot to release after the commit.
    event: Option<(EventKey, GroupEventId)>,
}

/// Everything one `commit_log` will write in one cache session.
pub(crate) struct Commit {
    pending: Vec<Pending>,
    shards: Vec<(LocGroupId, u64, u64)>,
    next: [u64; 4],
}

impl Commit {
    /// The rows to stage in the cache session.
    pub(crate) fn rows(&self) -> impl Iterator<Item = &Row> {
        self.pending.iter().flat_map(|pending| &pending.rows)
    }

    pub(crate) fn shards(&self) -> &[(LocGroupId, u64, u64)] {
        &self.shards
    }

    pub(crate) fn next(&self) -> [u64; 4] {
        self.next
    }
}

/// The allocators, the overlay, and the commit batches.
pub(crate) struct Sequencer {
    next: Cell<[u64; 4]>,
    delta: RefCell<Delta>,
    pending: RefCell<VecDeque<Pending>>,
    shards: RefCell<HashMap<LocGroupId, Shard>>,
    /// Bumped when a commit drops the shard cache, so a read that started
    /// before the commit cannot insert a stale counter afterward.
    shard_epoch: Cell<u64>,
    events: RefCell<HashMap<EventKey, (GroupEventId, Vec<u8>)>>,

    pub users: Flights<UserId, LocUserId>,
    pub senders: Flights<crate::types::SenderPk, LocSenderId>,
    pub groups: Flights<(crate::types::LocMsgTypeId, Box<[u8]>), LocGroupId>,
    pub data: Flights<DataId, Result<LocDataId, crate::types::DataVerifyError>>,
    pub event_flights: Flights<EventKey, EventFlight>,
}

impl Sequencer {
    pub(crate) fn new(next: [u64; 4]) -> Self {
        Self {
            next: Cell::new(next),
            delta: RefCell::new(HashMap::new()),
            pending: RefCell::new(VecDeque::new()),
            shards: RefCell::new(HashMap::new()),
            shard_epoch: Cell::new(0),
            events: RefCell::new(HashMap::new()),
            users: Rc::new(RefCell::new(HashMap::new())),
            senders: Rc::new(RefCell::new(HashMap::new())),
            groups: Rc::new(RefCell::new(HashMap::new())),
            data: Rc::new(RefCell::new(HashMap::new())),
            event_flights: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub(crate) fn next_values(&self) -> [u64; 4] {
        self.next.get()
    }

    /// Reserve the next id. Journal order and reservation order are the same
    /// because this runs under the journal lock.
    pub(crate) fn take_next(&self, which: Counter) -> u64 {
        let mut next = self.next.get();
        let id = next[which.index()];
        next[which.index()] += 1;
        self.next.set(next);
        id
    }

    /// Replay's counter: `id` was seen, so the next id is past it.
    pub(crate) fn observe(&self, which: Counter, id: u64) {
        let mut next = self.next.get();
        next[which.index()] = next[which.index()].max(id + 1);
        self.next.set(next);
    }

    /// The overlay read: unpropagated localization rows first, the cache
    /// second. Event rows are never in the overlay.
    pub(crate) async fn get<D: IsFilesystem>(
        &self,
        cache: &Storage<D>,
        table: TableId,
        key: &[u8],
    ) -> Option<Vec<u8>> {
        if let Some(value) = self
            .delta
            .borrow()
            .get(&table)
            .and_then(|rows| rows.get(key))
        {
            return Some(value.clone());
        }
        self.get_committed(cache, table, key).await
    }

    /// The committed read: cache only. Used for event bodies, event dedup
    /// during replay, and group diffs.
    pub(crate) async fn get_committed<D: IsFilesystem>(
        &self,
        cache: &Storage<D>,
        table: TableId,
        key: &[u8],
    ) -> Option<Vec<u8>> {
        cache.get(table, key).await.expect("read the cache")
    }

    /// Stage localization rows: visible immediately through the delta and
    /// written to the cache by the next commit.
    pub(crate) fn stage(&self, rows: Vec<Row>, cursor: Option<Cursor>) {
        if rows.is_empty() {
            return;
        }
        {
            let mut delta = self.delta.borrow_mut();
            for row in &rows {
                delta
                    .entry(row.table)
                    .or_default()
                    .insert(row.key.clone(), row.value.clone());
            }
        }
        self.pending.borrow_mut().push_back(Pending {
            cursor,
            rows,
            event: None,
        });
    }

    /// Stage an event: it is deliberately not inserted into the delta. Readers
    /// only see it after the commit writes it to the cache.
    pub(crate) fn stage_event(
        &self,
        rows: Vec<Row>,
        cursor: Option<Cursor>,
        key: EventKey,
        slot: GroupEventId,
        value: Vec<u8>,
    ) {
        self.events.borrow_mut().insert(key, (slot, value));
        self.pending.borrow_mut().push_back(Pending {
            cursor,
            rows,
            event: Some((key, slot)),
        });
    }

    /// What live dedup sees: an event waiting for commit, or the committed one.
    pub(crate) async fn event_at<D: IsFilesystem>(
        &self,
        cache: &Storage<D>,
        tables: &super::tables::Tables,
        key: EventKey,
    ) -> Option<(GroupEventId, Vec<u8>)> {
        if let Some((slot, value)) = self.events.borrow().get(&key) {
            return Some((*slot, value.clone()));
        }
        let slot = self
            .get_committed(
                cache,
                tables.event_keys,
                &codec::event_key(key.0, key.1, key.2),
            )
            .await?;
        let slot = GroupEventId(codec::parse_loc(&slot)?);
        let value = self
            .get_committed(cache, tables.events, &codec::event(key.0, slot))
            .await?;
        Some((slot, value))
    }

    /// Ensure the group's slot counter is in RAM. Two concurrent first events
    /// may both read the cache; the first insert wins. A commit that drops the
    /// cached shard mid-read bumps the epoch, so the stale read retries.
    pub(crate) async fn ensure_shard<D: IsFilesystem>(
        &self,
        cache: &Storage<D>,
        tables: &super::tables::Tables,
        group: LocGroupId,
    ) {
        loop {
            if self.shards.borrow().contains_key(&group) {
                return;
            }
            let epoch = self.shard_epoch.get();
            let (next, removed) = self
                .get_committed(cache, tables.shards, &codec::shard(group))
                .await
                .and_then(|bytes| codec::parse_shard(&bytes))
                .unwrap_or((0, 0));
            if self.shard_epoch.get() != epoch {
                continue;
            }
            self.shards
                .borrow_mut()
                .entry(group)
                .or_insert(Shard { next, removed });
            return;
        }
    }

    pub(crate) fn reserve_slot(&self, group: LocGroupId) -> GroupEventId {
        let mut shards = self.shards.borrow_mut();
        let shard = shards
            .get_mut(&group)
            .expect("ensure_shard runs before reserve_slot");
        let slot = GroupEventId(shard.next);
        shard.next += 1;
        slot
    }

    /// Take everything a commit through `head` covers, plus the shard and
    /// counter high-water marks. Runs under the journal lock.
    pub(crate) fn begin_commit(&self, head: Option<Cursor>) -> Commit {
        let mut pending = self.pending.borrow_mut();
        let mut taken = Vec::new();
        while let Some(front) = pending.front() {
            let covered = match (head, front.cursor) {
                (Some(head), Some(cursor)) => cursor <= head,
                (None, _) | (Some(_), None) => true,
            };
            if !covered {
                break;
            }
            taken.push(pending.pop_front().expect("the front exists"));
        }
        drop(pending);

        let shards = self
            .shards
            .borrow()
            .iter()
            .map(|(&group, shard)| (group, shard.next, shard.removed))
            .collect();
        Commit {
            pending: taken,
            shards,
            next: self.next.get(),
        }
    }

    /// The cache refused the batch: put its rows back at the front so the next
    /// commit retries them. The delta and the pending event entries were never
    /// released, so readers keep seeing the unchanged state.
    pub(crate) fn abort_commit(&self, commit: Commit) {
        let mut pending = self.pending.borrow_mut();
        for item in commit.pending.into_iter().rev() {
            pending.push_front(item);
        }
    }

    /// The commit landed: release the overlay rows it superseded and the
    /// per-key event entries it wrote. A newer write to the same row keeps its
    /// delta entry; a newer event keeps its shard state.
    pub(crate) fn finish_commit(&self, commit: &Commit) {
        {
            let mut delta = self.delta.borrow_mut();
            for pending in &commit.pending {
                for row in &pending.rows {
                    let Some(rows) = delta.get_mut(&row.table) else {
                        continue;
                    };
                    if rows.get(&row.key) == Some(&row.value) {
                        rows.remove(&row.key);
                    }
                }
            }
        }
        {
            let mut events = self.events.borrow_mut();
            for pending in &commit.pending {
                if let Some((key, slot)) = pending.event
                    && events.get(&key).is_some_and(|(at, _)| *at == slot)
                {
                    events.remove(&key);
                }
            }
        }
        {
            let mut shards = self.shards.borrow_mut();
            let mut dropped = false;
            for &(group, next, _) in &commit.shards {
                if shards.get(&group).is_some_and(|shard| shard.next == next) {
                    shards.remove(&group);
                    dropped = true;
                }
            }
            if dropped {
                self.shard_epoch.set(self.shard_epoch.get() + 1);
            }
        }
    }

    /// Read a group's committed watermark and diff.
    pub(crate) async fn diff_group<D: IsFilesystem>(
        &self,
        cache: &Storage<D>,
        tables: &super::tables::Tables,
        group: LocGroupId,
        since: Watermark,
    ) -> GroupDiff {
        let (added, removed) = self
            .get_committed(cache, tables.shards, &codec::shard(group))
            .await
            .and_then(|bytes| codec::parse_shard(&bytes))
            .unwrap_or((0, 0));
        GroupDiff {
            added: (since.added.min(added)..added).map(GroupEventId).collect(),
            removed: Vec::new(),
            watermark: Watermark { added, removed },
        }
    }
}

/// Run `work` once per `key`; concurrent callers share the result.
///
/// Returns the flight and whether this call created it. The owner flag is how
/// a write tells its own effect from a joiner's: the shared result was
/// produced for the owner, so only the owner may report it.
///
/// The work is driven from a detached task, not from the callers. A caller can
/// be cancelled at any await (evicting a gear aborts its task), and a
/// [`Shared`] future is polled only while someone awaits it. Driving it here
/// means a cancelled caller neither strands the operation nor pins its
/// captured values in the in-flight map.
pub(crate) fn coalesce<K, T>(
    map: &Flights<K, T>,
    key: K,
    work: impl Future<Output = T> + 'static,
) -> (Flight<T>, bool)
where
    K: Eq + std::hash::Hash + Clone + 'static,
    T: Clone + 'static,
{
    if let Some(flight) = map.borrow().get(&key) {
        return (flight.clone(), false);
    }
    let flight = work.boxed_local().shared();
    map.borrow_mut().insert(key.clone(), flight.clone());
    compio::runtime::spawn({
        let map = Rc::clone(map);
        let flight = flight.clone();
        async move {
            let _ = flight.await;
            map.borrow_mut().remove(&key);
        }
    })
    .detach();
    (flight, true)
}
