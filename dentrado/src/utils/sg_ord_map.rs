use imbl::OrdMap;
use std::ops::Bound;

use crate::core::gear::IsRuntime;
use crate::core::storage::{GroupStore, Storage};
use crate::types::{GroupEventId, Localizable, Remapper};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SGBucketId {
    pub timestamp: Timestamp,
}

pub type Timestamp = u32;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SGEventId(pub SGBucketId, pub GroupEventId);

impl SGEventId {
    #[must_use]
    pub fn new(bucket: SGBucketId, local_id: GroupEventId) -> Self {
        Self(bucket, local_id)
    }

    #[must_use]
    pub fn bucket(&self) -> &SGBucketId {
        &self.0
    }

    #[must_use]
    pub fn local_id(&self) -> GroupEventId {
        self.1
    }
}

impl PartialOrd for SGEventId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SGEventId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0).then_with(|| self.1.cmp(&other.1))
    }
}

#[derive(Clone, Debug)]
pub struct SgEntry<X> {
    pub local_id: GroupEventId,
    pub value: X,
}

impl<X> SgEntry<X> {
    fn to_event_id(&self, bucket: &SGBucketId) -> SGEventId {
        SGEventId(*bucket, self.local_id)
    }
}

impl<X: PartialEq> PartialEq for SgEntry<X> {
    fn eq(&self, other: &Self) -> bool {
        self.local_id == other.local_id && self.value == other.value
    }
}

#[derive(Clone, Debug)]
pub enum SgDiffItem<X> {
    Add(SGEventId, X),
    Remove(SGEventId, X),
    Update(SGEventId, X, X),
}

impl<X> SgDiffItem<X> {
    #[must_use]
    pub fn key(&self) -> &SGEventId {
        match self {
            SgDiffItem::Add(k, _) => k,
            SgDiffItem::Remove(k, _) => k,
            SgDiffItem::Update(k, _, _) => k,
        }
    }
}

async fn cmp_tx_sender<R: IsRuntime, S: Storage<R>>(
    store: &GroupStore<'_, R, S>,
    a: GroupEventId,
    b: GroupEventId,
) -> std::cmp::Ordering {
    let a_ev = store
        .stored_event(a)
        .await
        .expect("cmp_tx_sender: event not found");
    let b_ev = store
        .stored_event(b)
        .await
        .expect("cmp_tx_sender: event not found");
    let (a_tx, a_sender) = (a_ev.tx_id, a_ev.sender);
    let (b_tx, b_sender) = (b_ev.tx_id, b_ev.sender);
    match a_tx.cmp(&b_tx) {
        std::cmp::Ordering::Equal => {
            let a_pk = store
                .sender_pk(a_sender)
                .await
                .expect("cmp_tx_sender: sender_pk not found");
            let b_pk = store
                .sender_pk(b_sender)
                .await
                .expect("cmp_tx_sender: sender_pk not found");
            a_pk.cmp(&b_pk)
        }
        other => other,
    }
}

async fn bucket_binary_search<E, R: IsRuntime, S: Storage<R>>(
    entries: &[E],
    target_lid: GroupEventId,
    store: &GroupStore<'_, R, S>,
    get_local_id: impl Fn(&E) -> GroupEventId,
) -> Result<usize, usize> {
    let mut left = 0;
    let mut right = entries.len();
    while left < right {
        let mid = left + (right - left) / 2;
        match cmp_tx_sender(store, get_local_id(&entries[mid]), target_lid).await {
            std::cmp::Ordering::Less => left = mid + 1,
            std::cmp::Ordering::Equal => return Ok(mid),
            std::cmp::Ordering::Greater => right = mid,
        }
    }
    Err(left)
}

async fn bucket_upper_bound<E, R: IsRuntime, S: Storage<R>>(
    entries: &[E],
    target_lid: GroupEventId,
    store: &GroupStore<'_, R, S>,
    get_local_id: impl Fn(&E) -> GroupEventId,
) -> usize {
    match bucket_binary_search(entries, target_lid, store, get_local_id).await {
        Ok(idx) => idx + 1,
        Err(idx) => idx,
    }
}

async fn bucket_lower_bound_inclusive<E, R: IsRuntime, S: Storage<R>>(
    entries: &[E],
    target_lid: GroupEventId,
    store: &GroupStore<'_, R, S>,
    get_local_id: impl Fn(&E) -> GroupEventId,
) -> Option<usize> {
    match bucket_binary_search(entries, target_lid, store, get_local_id).await {
        Ok(idx) => Some(idx),
        Err(idx) => idx.checked_sub(1),
    }
}

async fn bucket_lower_bound_exclusive<E, R: IsRuntime, S: Storage<R>>(
    entries: &[E],
    target_lid: GroupEventId,
    store: &GroupStore<'_, R, S>,
    get_local_id: impl Fn(&E) -> GroupEventId,
) -> Option<usize> {
    match bucket_binary_search(entries, target_lid, store, get_local_id).await {
        Ok(idx) => idx.checked_sub(1),
        Err(idx) => idx.checked_sub(1),
    }
}

fn find_by_local_id<X>(entries: &[SgEntry<X>], local_id: GroupEventId) -> Option<usize> {
    entries.iter().position(|e| e.local_id == local_id)
}

#[derive(Clone, Debug)]
pub struct SgOrdMap<X: Clone> {
    buckets: OrdMap<SGBucketId, Vec<SgEntry<X>>>,
}

impl<X: Clone> Default for SgOrdMap<X> {
    fn default() -> Self {
        Self {
            buckets: OrdMap::new(),
        }
    }
}

impl<X: Clone> SgOrdMap<X> {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn unit(key: SGEventId, value: X) -> Self {
        let entry = SgEntry {
            local_id: key.1,
            value,
        };
        let bucket = key.0;
        Self {
            buckets: OrdMap::from(vec![(bucket, vec![entry])]),
        }
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.buckets.iter().map(|(_, v)| v.len()).sum()
    }

    pub async fn insert<R: IsRuntime, S: Storage<R>>(
        &mut self,
        key: SGEventId,
        value: X,
        store: &GroupStore<'_, R, S>,
    ) -> Option<X> {
        let bucket = key.0;
        let new_entry = SgEntry {
            local_id: key.1,
            value,
        };

        match self.buckets.entry(bucket) {
            imbl::ordmap::Entry::Occupied(mut entries) => {
                let entries = entries.get_mut();
                match bucket_binary_search(entries, key.1, store, |e| e.local_id).await {
                    Ok(idx) => {
                        if entries[idx].local_id == key.1 {
                            let old = std::mem::replace(&mut entries[idx].value, new_entry.value);
                            Some(old)
                        } else {
                            entries.insert(idx, new_entry);
                            None
                        }
                    }
                    Err(idx) => {
                        entries.insert(idx, new_entry);
                        None
                    }
                }
            }
            imbl::ordmap::Entry::Vacant(entry) => {
                entry.insert(vec![new_entry]);
                None
            }
        }
    }

    pub fn remove(&mut self, key: &SGEventId) -> Option<X> {
        let bucket = key.0;
        let entries = self.buckets.get_mut(&bucket)?;
        let idx = find_by_local_id(entries, key.1)?;
        let removed = entries.remove(idx);
        if entries.is_empty() {
            self.buckets.remove(&bucket);
        }
        Some(removed.value)
    }

    #[must_use]
    pub fn get(&self, key: &SGEventId) -> Option<&X> {
        let entries = self.buckets.get(&key.0)?;
        let idx = find_by_local_id(entries, key.1)?;
        Some(&entries[idx].value)
    }

    #[must_use]
    pub fn contains(&self, key: &SGEventId) -> bool {
        self.get(key).is_some()
    }

    #[must_use]
    pub async fn latest_at<'a, R: IsRuntime, S: Storage<R>>(
        &'a self,
        at: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Option<(SGEventId, &'a X)> {
        let bucket = at.0;

        if let Some(entries) = self.buckets.get(&bucket) {
            let idx = bucket_lower_bound_inclusive(entries, at.1, store, |e| e.local_id).await;
            if let Some(idx) = idx
                && let Some(entry) = entries.get(idx)
            {
                return Some((entry.to_event_id(&bucket), &entry.value));
            }
        }

        self.buckets
            .range((Bound::Unbounded, Bound::Excluded(bucket)))
            .next_back()
            .map(|(bk, entries)| {
                let last = entries.last().expect("empty bucket should not exist");
                (last.to_event_id(bk), &last.value)
            })
    }

    #[must_use]
    pub async fn latest_before<'a, R: IsRuntime, S: Storage<R>>(
        &'a self,
        at: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Option<(SGEventId, &'a X)> {
        let bucket = at.0;

        if let Some(entries) = self.buckets.get(&bucket) {
            let idx = bucket_lower_bound_exclusive(entries, at.1, store, |e| e.local_id).await;
            if let Some(idx) = idx
                && let Some(entry) = entries.get(idx)
            {
                return Some((entry.to_event_id(&bucket), &entry.value));
            }
        }

        self.buckets
            .range((Bound::Unbounded, Bound::Excluded(bucket)))
            .next_back()
            .map(|(bk, entries)| {
                let last = entries.last().expect("empty bucket should not exist");
                (last.to_event_id(bk), &last.value)
            })
    }

    #[must_use]
    pub async fn next_after<R: IsRuntime, S: Storage<R>>(
        &self,
        at: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Option<SGEventId> {
        let bucket = at.0;

        if let Some(entries) = self.buckets.get(&bucket) {
            let idx = bucket_upper_bound(entries, at.1, store, |e| e.local_id).await;
            if let Some(entry) = entries.get(idx) {
                return Some(entry.to_event_id(&bucket));
            }
        }

        self.buckets
            .range((Bound::Excluded(bucket), Bound::Unbounded))
            .next()
            .map(|(bk, entries)| {
                let first = entries.first().expect("empty bucket should not exist");
                first.to_event_id(bk)
            })
    }

    #[must_use]
    pub fn first(&self) -> Option<(SGEventId, &X)> {
        self.buckets.iter().next().map(|(bk, entries)| {
            let first = entries.first().expect("empty bucket should not exist");
            (first.to_event_id(bk), &first.value)
        })
    }

    #[must_use]
    pub fn last(&self) -> Option<(SGEventId, &X)> {
        self.buckets.iter().next_back().map(|(bk, entries)| {
            let last = entries.last().expect("empty bucket should not exist");
            (last.to_event_id(bk), &last.value)
        })
    }

    pub fn iter(&self) -> impl Iterator<Item = (SGEventId, &X)> {
        self.buckets
            .iter()
            .flat_map(|(bk, entries)| entries.iter().map(move |e| (e.to_event_id(bk), &e.value)))
    }

    #[must_use]
    pub async fn range_between<R: IsRuntime, S: Storage<R>>(
        &self,
        at: &SGEventId,
        upper: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Vec<SGEventId> {
        let start_bucket = at.0;
        let end_bucket = upper.0;

        if start_bucket == end_bucket {
            let mut result = Vec::new();
            if let Some(entries) = self.buckets.get(&start_bucket) {
                let idx_start = bucket_upper_bound(entries, at.1, store, |e| e.local_id).await;
                let idx_end = bucket_lower_bound_inclusive(entries, upper.1, store, |e| e.local_id)
                    .await
                    .map_or(0, |i| i + 1);
                for e in &entries[idx_start..idx_end.max(idx_start)] {
                    result.push(e.to_event_id(&start_bucket));
                }
            }
            return result;
        }

        let mut result = Vec::new();

        if let Some(entries) = self.buckets.get(&start_bucket) {
            let idx = bucket_upper_bound(entries, at.1, store, |e| e.local_id).await;
            for e in &entries[idx..] {
                result.push(e.to_event_id(&start_bucket));
            }
        }

        for (bk, entries) in self
            .buckets
            .range((Bound::Excluded(start_bucket), Bound::Excluded(end_bucket)))
        {
            for e in entries {
                result.push(e.to_event_id(bk));
            }
        }

        if let Some(entries) = self.buckets.get(&end_bucket) {
            let idx = bucket_lower_bound_inclusive(entries, upper.1, store, |e| e.local_id)
                .await
                .map_or(0, |i| i + 1);
            for e in &entries[..idx] {
                result.push(e.to_event_id(&end_bucket));
            }
        }

        result
    }

    #[must_use]
    pub async fn range_after<R: IsRuntime, S: Storage<R>>(
        &self,
        at: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Vec<SGEventId> {
        let bucket = at.0;
        let mut result = Vec::new();

        if let Some(entries) = self.buckets.get(&bucket) {
            let idx = bucket_upper_bound(entries, at.1, store, |e| e.local_id).await;
            for e in &entries[idx..] {
                result.push(e.to_event_id(&bucket));
            }
        }

        for (bk, entries) in self
            .buckets
            .range((Bound::Excluded(bucket), Bound::Unbounded))
        {
            for e in entries {
                result.push(e.to_event_id(bk));
            }
        }

        result
    }

    pub fn try_remap_local_ids<E>(
        &mut self,
        f: &mut dyn FnMut(GroupEventId) -> Result<GroupEventId, E>,
    ) -> Result<(), E> {
        let old = std::mem::take(&mut self.buckets);
        for (bk, entries) in old {
            let mut new_entries = Vec::with_capacity(entries.len());
            for mut e in entries {
                e.local_id = f(e.local_id)?;
                new_entries.push(e);
            }
            self.buckets.insert(bk, new_entries);
        }
        Ok(())
    }

    pub async fn try_remap_values<E>(
        &mut self,
        remapper: &mut impl Remapper<Err = E>,
    ) -> Result<(), E>
    where
        X: Localizable,
    {
        let old = std::mem::take(&mut self.buckets);
        for (bk, entries) in old {
            let mut new_entries = Vec::with_capacity(entries.len());
            for mut e in entries {
                e.value = e.value.localize(remapper).await?;
                new_entries.push(e);
            }
            self.buckets.insert(bk, new_entries);
        }
        Ok(())
    }
}

impl<X: Clone + PartialEq> PartialEq for SgOrdMap<X> {
    fn eq(&self, other: &Self) -> bool {
        if self.buckets.len() != other.buckets.len() {
            return false;
        }
        for (lk, lv) in &self.buckets {
            match other.buckets.get(lk) {
                Some(rv) if lv.len() == rv.len() => {
                    for (le, re) in lv.iter().zip(rv.iter()) {
                        if le.local_id != re.local_id || le.value != re.value {
                            return false;
                        }
                    }
                }
                _ => return false,
            }
        }
        true
    }
}

impl<X: Clone + PartialEq + Eq> Eq for SgOrdMap<X> {}

impl<X: Clone + std::hash::Hash> std::hash::Hash for SgOrdMap<X> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (k, entries) in &self.buckets {
            k.hash(state);
            entries.len().hash(state);
            for e in entries {
                e.local_id.hash(state);
                e.value.hash(state);
            }
        }
    }
}

impl<X: Clone + Ord> PartialOrd for SgOrdMap<X> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<X: Clone + Ord> Ord for SgOrdMap<X> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let mut left = self.iter();
        let mut right = other.iter();
        loop {
            match (left.next(), right.next()) {
                (None, None) => return std::cmp::Ordering::Equal,
                (None, Some(_)) => return std::cmp::Ordering::Less,
                (Some(_), None) => return std::cmp::Ordering::Greater,
                (Some((lk, lv)), Some((rk, rv))) => match lk.cmp(&rk) {
                    std::cmp::Ordering::Equal => match lv.cmp(rv) {
                        std::cmp::Ordering::Equal => continue,
                        other => return other,
                    },
                    other => return other,
                },
            }
        }
    }
}

impl<X: Clone + PartialEq> SgOrdMap<X> {
    #[must_use]
    pub async fn diff_cloned<R: IsRuntime, S: Storage<R>>(
        &self,
        other: &Self,
        store: &GroupStore<'_, R, S>,
    ) -> Vec<SgDiffItem<X>> {
        use imbl::ordmap::DiffItem;

        let mut result = Vec::new();

        for item in self.buckets.diff(&other.buckets) {
            match item {
                DiffItem::Add(bk, entries) => {
                    for e in entries {
                        result.push(SgDiffItem::Add(e.to_event_id(bk), e.value.clone()));
                    }
                }
                DiffItem::Remove(bk, entries) => {
                    for e in entries {
                        result.push(SgDiffItem::Remove(e.to_event_id(bk), e.value.clone()));
                    }
                }
                DiffItem::Update {
                    old: (bk, old_entries),
                    new: (_, new_entries),
                } => {
                    let mut oi = 0;
                    let mut ni = 0;
                    while oi < old_entries.len() && ni < new_entries.len() {
                        match cmp_tx_sender(
                            store,
                            old_entries[oi].local_id,
                            new_entries[ni].local_id,
                        )
                        .await
                        {
                            std::cmp::Ordering::Less => {
                                result.push(SgDiffItem::Remove(
                                    old_entries[oi].to_event_id(bk),
                                    old_entries[oi].value.clone(),
                                ));
                                oi += 1;
                            }
                            std::cmp::Ordering::Greater => {
                                result.push(SgDiffItem::Add(
                                    new_entries[ni].to_event_id(bk),
                                    new_entries[ni].value.clone(),
                                ));
                                ni += 1;
                            }
                            std::cmp::Ordering::Equal => {
                                if old_entries[oi].local_id == new_entries[ni].local_id {
                                    if old_entries[oi].value != new_entries[ni].value {
                                        result.push(SgDiffItem::Update(
                                            old_entries[oi].to_event_id(bk),
                                            old_entries[oi].value.clone(),
                                            new_entries[ni].value.clone(),
                                        ));
                                    }
                                } else {
                                    result.push(SgDiffItem::Remove(
                                        old_entries[oi].to_event_id(bk),
                                        old_entries[oi].value.clone(),
                                    ));
                                    result.push(SgDiffItem::Add(
                                        new_entries[ni].to_event_id(bk),
                                        new_entries[ni].value.clone(),
                                    ));
                                }
                                oi += 1;
                                ni += 1;
                            }
                        }
                    }
                    while oi < old_entries.len() {
                        result.push(SgDiffItem::Remove(
                            old_entries[oi].to_event_id(bk),
                            old_entries[oi].value.clone(),
                        ));
                        oi += 1;
                    }
                    while ni < new_entries.len() {
                        result.push(SgDiffItem::Add(
                            new_entries[ni].to_event_id(bk),
                            new_entries[ni].value.clone(),
                        ));
                        ni += 1;
                    }
                }
            }
        }

        result
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, std::hash::Hash, PartialOrd, Ord)]
pub struct SgOrdSet(SgOrdMap<()>);

impl SgOrdSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn unit(key: SGEventId) -> Self {
        Self(SgOrdMap::unit(key, ()))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub async fn insert<R: IsRuntime, S: Storage<R>>(
        &mut self,
        key: SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> bool {
        self.0.insert(key, (), store).await.is_none()
    }

    pub fn remove(&mut self, key: &SGEventId) -> bool {
        self.0.remove(key).is_some()
    }

    #[must_use]
    pub fn contains(&self, key: &SGEventId) -> bool {
        self.0.contains(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = SGEventId> + '_ {
        self.0.iter().map(|(eid, ())| eid)
    }

    #[must_use]
    pub async fn range_between<R: IsRuntime, S: Storage<R>>(
        &self,
        at: &SGEventId,
        upper: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Vec<SGEventId> {
        self.0.range_between(at, upper, store).await
    }

    #[must_use]
    pub async fn range_after<R: IsRuntime, S: Storage<R>>(
        &self,
        at: &SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Vec<SGEventId> {
        self.0.range_after(at, store).await
    }

    pub fn try_remap_local_ids<E>(
        &mut self,
        f: &mut dyn FnMut(GroupEventId) -> Result<GroupEventId, E>,
    ) -> Result<(), E> {
        self.0.try_remap_local_ids(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gear::EmptyRuntime;
    use crate::core::loc_ctx::StoredEvent;
    use crate::core::storage::{GroupStore, InMemoryStorage, Storage};
    use crate::types::{LocGroupId, SenderPk};

    const PK_A: SenderPk = SenderPk([0u8; 32]);
    const PK_B: SenderPk = SenderPk([1u8; 32]);
    const PK_C: SenderPk = SenderPk([2u8; 32]);

    /// Drive a future on a throwaway compio runtime (sg_ord_map is async but
    /// I/O-free; `InMemoryStorage` completes eagerly).
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        compio::runtime::RuntimeBuilder::new()
            .build()
            .expect("compio runtime build failed")
            .block_on(fut)
    }

    async fn make_test_storage() -> InMemoryStorage<EmptyRuntime> {
        let storage = InMemoryStorage::<EmptyRuntime>::new();
        let sid_a = storage.mk_loc_sender(PK_A, None).await;
        let sid_b = storage.mk_loc_sender(PK_B, None).await;
        let sid_c = storage.mk_loc_sender(PK_C, None).await;
        for i in 0u64..10 {
            let sid = match i % 3 {
                0 => sid_a,
                1 => sid_b,
                _ => sid_c,
            };
            storage
                .store_event(
                    LocGroupId(0),
                    StoredEvent {
                        sender: sid,
                        tx_id: i as u32,
                        timestamp: 0,
                        source_node: crate::types::NodeId(0),
                        body: (),
                    },
                )
                .await;
        }
        storage
    }

    fn gs(
        storage: &InMemoryStorage<EmptyRuntime>,
    ) -> GroupStore<'_, EmptyRuntime, InMemoryStorage<EmptyRuntime>> {
        GroupStore::new(storage, LocGroupId(0))
    }

    fn eid(ts: u32, lid: u64) -> SGEventId {
        SGEventId::new(SGBucketId { timestamp: ts }, GroupEventId(lid))
    }

    #[test]
    fn map_insert_get_remove() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k = eid(1, 0);
            assert!(m.insert(k, "hello", &ctx).await.is_none());
            assert_eq!(m.get(&k), Some(&"hello"));
            assert_eq!(m.remove(&k), Some("hello"));
            assert!(m.get(&k).is_none());
        })
    }

    #[test]
    fn map_insert_overwrite() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k = eid(1, 0);
            assert!(m.insert(k, "old", &ctx).await.is_none());
            assert_eq!(m.insert(k, "new", &ctx).await, Some("old"));
            assert_eq!(m.get(&k), Some(&"new"));
        })
    }

    #[test]
    fn map_concurrent_events_in_bucket() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let ka = eid(1, 0);
            let kb = eid(1, 1);
            m.insert(ka, "A", &ctx).await;
            m.insert(kb, "B", &ctx).await;

            assert_eq!(m.get(&ka), Some(&"A"));
            assert_eq!(m.get(&kb), Some(&"B"));
            assert_eq!(m.len(), 2);
        })
    }

    #[test]
    fn map_latest_at() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(3, 0);
            m.insert(k1, "first", &ctx).await;
            m.insert(k2, "second", &ctx).await;

            assert_eq!(m.latest_at(&k1, &ctx).await, Some((k1, &"first")));
            assert_eq!(m.latest_at(&k2, &ctx).await, Some((k2, &"second")));

            let between = eid(2, 0);
            assert_eq!(m.latest_at(&between, &ctx).await, Some((k1, &"first")));

            let before = eid(0, 0);
            assert!(m.latest_at(&before, &ctx).await.is_none());
        })
    }

    #[test]
    fn map_latest_before() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            m.insert(k1, "first", &ctx).await;
            m.insert(k2, "second", &ctx).await;

            assert_eq!(m.latest_before(&k2, &ctx).await, Some((k1, &"first")));
            assert!(m.latest_before(&k1, &ctx).await.is_none());
        })
    }

    #[test]
    fn map_latest_at_same_bucket() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let ka = eid(1, 0);
            let kb = eid(1, 1);
            m.insert(ka, "A", &ctx).await;
            m.insert(kb, "B", &ctx).await;

            assert_eq!(m.latest_at(&kb, &ctx).await, Some((kb, &"B")));
            assert_eq!(m.latest_at(&ka, &ctx).await, Some((ka, &"A")));
        })
    }

    #[test]
    fn map_next_after() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            let k3 = eid(2, 3);
            m.insert(k1, "first", &ctx).await;
            m.insert(k2, "second", &ctx).await;
            m.insert(k3, "third", &ctx).await;

            assert_eq!(m.next_after(&k1, &ctx).await, Some(k2));
            assert_eq!(m.next_after(&k2, &ctx).await, Some(k3));
            assert!(m.next_after(&k3, &ctx).await.is_none());
        })
    }

    #[test]
    fn map_iter_order() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k2 = eid(2, 0);
            let k1 = eid(1, 0);
            let k3 = eid(2, 3);
            m.insert(k2, "second", &ctx).await;
            m.insert(k1, "first", &ctx).await;
            m.insert(k3, "third", &ctx).await;

            let items: Vec<_> = m.iter().collect();
            assert_eq!(items[0], (k1, &"first"));
            assert_eq!(items[1], (k2, &"second"));
            assert_eq!(items[2], (k3, &"third"));
        })
    }

    #[test]
    fn map_first_last() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            m.insert(k1, "first", &ctx).await;
            m.insert(k2, "second", &ctx).await;

            assert_eq!(m.first(), Some((k1, &"first")));
            assert_eq!(m.last(), Some((k2, &"second")));
        })
    }

    #[test]
    fn map_diff_cloned() {
        block_on(async {
            let mut old = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            old.insert(k1, "keep", &ctx).await;
            old.insert(k2, "remove", &ctx).await;

            let mut new = SgOrdMap::new();
            let k3 = eid(3, 0);
            new.insert(k1, "keep", &ctx).await;
            new.insert(k3, "add", &ctx).await;

            let diff = old.diff_cloned(&new, &ctx).await;
            assert_eq!(diff.len(), 2);
            assert!(matches!(&diff[0], SgDiffItem::Remove(id, v) if *id == k2 && *v == "remove"));
            assert!(matches!(&diff[1], SgDiffItem::Add(id, v) if *id == k3 && *v == "add"));
        })
    }

    #[test]
    fn map_remap_local_ids() {
        block_on(async {
            let mut m = SgOrdMap::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 10);
            let k2 = eid(2, 20);
            m.insert(k1, "a", &ctx).await;
            m.insert(k2, "b", &ctx).await;

            m.try_remap_local_ids::<std::convert::Infallible>(&mut |lid| {
                Ok(GroupEventId(lid.0 + 1000))
            })
            .unwrap();

            assert!(m.get(&k1).is_none());
            assert!(m.get(&k2).is_none());

            let new_k1 = eid(1, 1010);
            let new_k2 = eid(2, 1020);
            assert_eq!(m.get(&new_k1), Some(&"a"));
            assert_eq!(m.get(&new_k2), Some(&"b"));
        })
    }

    #[test]
    fn set_insert_remove_contains() {
        block_on(async {
            let mut s = SgOrdSet::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k = eid(1, 0);
            assert!(s.insert(k, &ctx).await);
            assert!(s.contains(&k));
            assert!(!s.insert(k, &ctx).await);
            assert!(s.remove(&k));
            assert!(!s.contains(&k));
            assert!(!s.remove(&k));
        })
    }

    #[test]
    fn set_range_between() {
        block_on(async {
            let mut s = SgOrdSet::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            let k3 = eid(3, 0);
            let k4 = eid(4, 0);
            s.insert(k1, &ctx).await;
            s.insert(k2, &ctx).await;
            s.insert(k3, &ctx).await;
            s.insert(k4, &ctx).await;

            let result = s.range_between(&k2, &k4, &ctx).await;
            assert_eq!(result, vec![k3, k4]);
        })
    }

    #[test]
    fn set_range_after() {
        block_on(async {
            let mut s = SgOrdSet::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let k1 = eid(1, 0);
            let k2 = eid(2, 0);
            let k3 = eid(3, 0);
            s.insert(k1, &ctx).await;
            s.insert(k2, &ctx).await;
            s.insert(k3, &ctx).await;

            let result = s.range_after(&k2, &ctx).await;
            assert_eq!(result, vec![k3]);
        })
    }

    #[test]
    fn set_range_between_same_bucket() {
        block_on(async {
            let mut s = SgOrdSet::new();
            let storage = make_test_storage().await;
            let ctx = gs(&storage);
            let ka = eid(1, 0);
            let kb = eid(1, 1);
            let kc = eid(1, 2);
            s.insert(ka, &ctx).await;
            s.insert(kb, &ctx).await;
            s.insert(kc, &ctx).await;

            let result = s.range_between(&ka, &kc, &ctx).await;
            assert_eq!(result, vec![kb, kc]);
        })
    }
}
