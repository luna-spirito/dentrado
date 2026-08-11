use imbl::OrdMap;
use std::borrow::Borrow;
use std::{collections::BTreeSet, hash::Hash};

use crate::core::gear::IsRuntime;
use crate::core::storage::{GroupStore, Storage};
use crate::types::{GroupEventId, Localizable, Remapper};
use crate::utils::sg_ord_map::{SgOrdMap, SgOrdSet};

pub use crate::utils::sg_ord_map::{SGBucketId, SGEventId, Timestamp};

pub struct DeltaList<Id> {
    pub removed: Vec<Id>,
    pub added: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timeline<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord> {
    pub(crate) writes: OrdMap<DepK, SgOrdMap<DepV>>,
}

impl<DepK, DepV> Timeline<DepK, DepV>
where
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
{
    #[must_use]
    pub fn new() -> Self {
        Self {
            writes: OrdMap::new(),
        }
    }

    pub(crate) async fn query_at<R: IsRuntime, S: Storage<R>>(
        &self,
        key: &DepK,
        at: SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Option<DepV> {
        let Some(timeline) = self.writes.get(key) else {
            return None;
        };
        let Some((_, v)) = timeline.latest_at(&at, store).await else {
            return None;
        };
        Some(v.clone())
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DepK, &SgOrdMap<DepV>)> {
        self.writes.iter()
    }

    #[must_use]
    pub(crate) fn diff_from(&self, old: &Self) -> TimelineDelta<DepK, DepV> {
        use imbl::ordmap::DiffItem;

        let mut added_keys: OrdMap<DepK, SgOrdMap<DepV>> = OrdMap::new();
        let mut removed_keys: imbl::OrdSet<DepK> = imbl::OrdSet::new();
        let mut changed_keys: OrdMap<DepK, SgOrdMap<DepV>> = OrdMap::new();

        for item in old.writes.diff(&self.writes) {
            match item {
                DiffItem::Add(k, timeline) => {
                    added_keys.insert(k.clone(), timeline.clone());
                }
                DiffItem::Remove(k, _timeline) => {
                    removed_keys.insert(k.clone());
                }
                DiffItem::Update {
                    old: (k, _old_tl),
                    new: (_, new_tl),
                } => {
                    changed_keys.insert(k.clone(), new_tl.clone());
                }
            }
        }

        TimelineDelta {
            added_keys,
            removed_keys,
            changed_keys,
        }
    }

    #[must_use]
    pub(crate) fn apply_delta(&self, delta: &TimelineDelta<DepK, DepV>) -> Self {
        let mut writes = self.writes.clone();

        for k in &delta.removed_keys {
            writes.remove(k);
        }

        for (k, timeline) in &delta.added_keys {
            writes.insert(k.clone(), timeline.clone());
        }
        for (k, timeline) in &delta.changed_keys {
            writes.insert(k.clone(), timeline.clone());
        }

        Self { writes }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TimelineDelta<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord> {
    pub(crate) added_keys: OrdMap<DepK, SgOrdMap<DepV>>,
    pub(crate) removed_keys: imbl::OrdSet<DepK>,
    pub(crate) changed_keys: OrdMap<DepK, SgOrdMap<DepV>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtDep<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord> {
    pub(crate) cached: Timeline<DepK, DepV>,
    pub(crate) reads: OrdMap<DepK, SgOrdSet>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct EventEffects<
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    K: Ord + Clone + Hash,
    V: Clone + PartialEq + Hash + Ord,
> {
    pub(crate) reads: imbl::OrdSet<K>,
    pub(crate) writes: OrdMap<K, V>,
    pub(crate) dep_reads: OrdMap<Dep, imbl::OrdSet<DepK>>,
}

pub struct HandlerCtx<
    'a,
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    R: IsRuntime,
    S: Storage<R>,
    K: Ord + Clone + Hash,
    V: Clone + Hash,
    D: ?Sized,
> {
    pub event_id: SGEventId,
    reads: &'a mut imbl::OrdSet<K>,
    writes: &'a mut OrdMap<K, V>,
    pub(crate) self_writes: &'a OrdMap<K, SgOrdMap<V>>,
    ext: &'a mut OrdMap<Dep, ExtDep<DepK, DepV>>,
    dep_resolver: &'a mut D,
    store: &'a GroupStore<'a, R, S>,
}

impl<Dep, DepK, DepV, R: IsRuntime, S: Storage<R>, K, V, D>
    HandlerCtx<'_, Dep, DepK, DepV, R, S, K, V, D>
where
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    K: Ord + Clone + Hash,
    V: Clone + Hash,
    D: async FnMut(&Dep) -> Timeline<DepK, DepV> + ?Sized,
{
    pub async fn query(&mut self, k: &K) -> Option<V> {
        self.reads.insert(k.clone());
        let Some(timeline) = self.self_writes.get(k) else {
            return None;
        };
        let Some((_, v)) = timeline.latest_before(&self.event_id, self.store).await else {
            return None;
        };
        Some(v.clone())
    }

    pub fn update(&mut self, k: K, v: V) {
        self.writes.insert(k, v);
    }

    /// The group-bound store this handler runs against.
    #[must_use]
    pub fn store(&self) -> &GroupStore<'_, R, S> {
        self.store
    }

    pub async fn dep_query(&mut self, dep: &Dep, dep_key: &DepK) -> Option<DepV> {
        let writes = (self.dep_resolver)(dep).await;

        match self.ext.entry(dep.clone()) {
            imbl::ordmap::Entry::Vacant(entry) => {
                entry.insert(ExtDep {
                    cached: writes.clone(),
                    reads: OrdMap::new(),
                });
            }
            imbl::ordmap::Entry::Occupied(_) => {}
        }

        let store = self.store;
        if let Some(ext_dep) = self.ext.get_mut(dep) {
            match ext_dep.reads.entry(dep_key.clone()) {
                imbl::ordmap::Entry::Occupied(mut entry) => {
                    entry.get_mut().insert(self.event_id, store).await;
                }
                imbl::ordmap::Entry::Vacant(entry) => {
                    entry.insert(SgOrdSet::unit(self.event_id));
                }
            }
        }

        writes.query_at(dep_key, self.event_id, store).await
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateGraph<
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    K: Ord + Clone + Hash,
    V: Clone + PartialEq + Hash + Ord,
> {
    pub(crate) writes: OrdMap<K, SgOrdMap<V>>,
    pub(crate) reads: OrdMap<K, SgOrdSet>,
    pub(crate) effects: OrdMap<SGEventId, EventEffects<Dep, DepK, K, V>>,
    pub(crate) ext: OrdMap<Dep, ExtDep<DepK, DepV>>,
}

impl<Dep, DepK, DepV, K, V> StateGraph<Dep, DepK, DepV, K, V>
where
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    K: Ord + Clone + Hash,
    V: Clone + PartialEq + Hash + Ord,
{
    #[must_use]
    pub fn new() -> Self {
        Self {
            writes: OrdMap::new(),
            reads: OrdMap::new(),
            effects: OrdMap::new(),
            ext: OrdMap::new(),
        }
    }

    #[must_use]
    pub fn as_writes(&self) -> Timeline<K, V> {
        Timeline {
            writes: self.writes.clone(),
        }
    }

    pub async fn apply<R: IsRuntime, S: Storage<R>, E, H, D>(
        &mut self,
        handler: &mut H,
        event_resolver: &impl async Fn(GroupEventId) -> (SGEventId, E),
        dep_resolver: &mut D,
        store: &GroupStore<'_, R, S>,
        delta: &DeltaList<GroupEventId>,
    ) where
        E: Clone,
        H: async FnMut(&E, &mut HandlerCtx<'_, Dep, DepK, DepV, R, S, K, V, D>),
        D: async FnMut(&Dep) -> Timeline<DepK, DepV>,
    {
        let mut queue: BTreeSet<SGEventId> = BTreeSet::new();

        for &local_id in &delta.removed {
            let (event_id, _) = event_resolver(local_id).await;
            if let Some(old_effects) = self.effects.remove(&event_id) {
                for k in &old_effects.reads {
                    Self::remove_from_reads(&mut self.reads, k, &event_id);
                }
                for (dep, dep_keys) in &old_effects.dep_reads {
                    for dep_key in dep_keys {
                        Self::remove_from_ext_reads(&mut self.ext, dep, dep_key, &event_id);
                    }
                }
                for k in old_effects.writes.keys().cloned().collect::<Vec<_>>() {
                    Self::remove_from_timeline(&mut self.writes, &k, &event_id);
                    Self::propagate_key_change(
                        &self.reads,
                        &self.writes,
                        &k,
                        event_id,
                        &mut queue,
                        store,
                    )
                    .await;
                }
            }
        }

        let dep_queue = self.detect_dep_changes(dep_resolver, store).await;
        for event_id in dep_queue {
            queue.insert(event_id);
        }

        for &local_id in &delta.added {
            let (event_id, _) = event_resolver(local_id).await;
            queue.insert(event_id);
        }

        self.process_queue(handler, event_resolver, dep_resolver, store, &mut queue)
            .await;
    }

    pub(crate) fn query(&self, k: &K) -> Option<&V> {
        self.writes
            .get(k)
            .and_then(|timeline| timeline.last())
            .map(|(_, v)| v)
    }

    pub(crate) async fn query_at<'a, R: IsRuntime, S: Storage<R>>(
        &'a self,
        k: &K,
        event_id: SGEventId,
        store: &GroupStore<'_, R, S>,
    ) -> Option<&'a V> {
        let Some(timeline) = self.writes.get(k) else {
            return None;
        };
        let Some((_, v)) = timeline.latest_at(&event_id, store).await else {
            return None;
        };
        Some(v)
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.writes.keys()
    }

    pub(crate) fn timeline_for(&self, k: &K) -> impl Iterator<Item = (SGEventId, &V)> {
        self.writes.get(k).into_iter().flat_map(SgOrdMap::iter)
    }

    async fn detect_dep_changes<R: IsRuntime, S: Storage<R>, D>(
        &mut self,
        dep_resolver: &mut D,
        store: &GroupStore<'_, R, S>,
    ) -> BTreeSet<SGEventId>
    where
        D: async FnMut(&Dep) -> Timeline<DepK, DepV>,
    {
        use imbl::ordmap::DiffItem;

        let mut affected = BTreeSet::new();
        let dep_ids: Vec<Dep> = self.ext.keys().cloned().collect();

        for dep in dep_ids {
            let current = dep_resolver(&dep).await;

            {
                let Some(ext_dep) = self.ext.get(&dep) else {
                    continue;
                };

                for outer_item in ext_dep.cached.writes.diff(&current.writes) {
                    match outer_item {
                        DiffItem::Add(dep_key, new_timeline) => {
                            if let Some(readers) = ext_dep.reads.get(dep_key)
                                && let Some((first_write, _)) = new_timeline.first()
                            {
                                for reader in readers.range_after(&first_write, store).await {
                                    affected.insert(reader);
                                }
                            }
                        }
                        DiffItem::Remove(dep_key, old_timeline) => {
                            if let Some(readers) = ext_dep.reads.get(dep_key)
                                && let Some((first_write, _)) = old_timeline.first()
                            {
                                for reader in readers.range_after(&first_write, store).await {
                                    affected.insert(reader);
                                }
                            }
                        }
                        DiffItem::Update {
                            old: (dep_key, old_timeline),
                            new: (_, new_timeline),
                        } => {
                            if let Some(readers) = ext_dep.reads.get(dep_key) {
                                for inner_item in
                                    old_timeline.diff_cloned(new_timeline, store).await
                                {
                                    let changed_at = *inner_item.key();
                                    Self::add_affected_readers(
                                        new_timeline,
                                        changed_at,
                                        readers,
                                        &mut affected,
                                        store,
                                    )
                                    .await;
                                }
                            }
                        }
                    }
                }
            }

            if let Some(ext_dep) = self.ext.get_mut(&dep) {
                ext_dep.cached = current;
            }
        }

        affected
    }

    async fn add_affected_readers<R: IsRuntime, S: Storage<R>>(
        new_timeline: &SgOrdMap<DepV>,
        changed_at: SGEventId,
        readers: &SgOrdSet,
        affected: &mut BTreeSet<SGEventId>,
        store: &GroupStore<'_, R, S>,
    ) {
        match new_timeline.next_after(&changed_at, store).await {
            Some(next_write) => {
                for reader in readers.range_between(&changed_at, &next_write, store).await {
                    affected.insert(reader);
                }
            }
            None => {
                for reader in readers.range_after(&changed_at, store).await {
                    affected.insert(reader);
                }
            }
        }
    }

    fn remove_from_reads(reads: &mut OrdMap<K, SgOrdSet>, k: &K, event_id: &SGEventId) {
        match reads.entry(k.clone()) {
            imbl::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().remove(event_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            imbl::ordmap::Entry::Vacant(_) => {}
        }
    }

    async fn add_to_reads<R: IsRuntime, S: Storage<R>>(
        reads: &mut OrdMap<K, SgOrdSet>,
        k: K,
        event_id: SGEventId,
        store: &GroupStore<'_, R, S>,
    ) {
        match reads.entry(k) {
            imbl::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().insert(event_id, store).await;
            }
            imbl::ordmap::Entry::Vacant(entry) => {
                entry.insert(SgOrdSet::unit(event_id));
            }
        }
    }

    fn remove_from_timeline(writes: &mut OrdMap<K, SgOrdMap<V>>, k: &K, event_id: &SGEventId) {
        match writes.entry(k.clone()) {
            imbl::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().remove(event_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            imbl::ordmap::Entry::Vacant(_) => {}
        }
    }

    fn remove_from_ext_reads(
        ext: &mut OrdMap<Dep, ExtDep<DepK, DepV>>,
        dep: &Dep,
        dep_key: &DepK,
        event_id: &SGEventId,
    ) {
        if let Some(ext_dep) = ext.get_mut(dep) {
            match ext_dep.reads.entry(dep_key.clone()) {
                imbl::ordmap::Entry::Occupied(mut entry) => {
                    entry.get_mut().remove(event_id);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
                imbl::ordmap::Entry::Vacant(_) => {}
            }
        }
    }

    async fn propagate_key_change<R: IsRuntime, S: Storage<R>>(
        reads: &OrdMap<K, SgOrdSet>,
        writes: &OrdMap<K, SgOrdMap<V>>,
        k: &K,
        event_id: SGEventId,
        queue: &mut BTreeSet<SGEventId>,
        store: &GroupStore<'_, R, S>,
    ) {
        let Some(read_set) = reads.get(k) else {
            return;
        };
        let upper = if let Some(timeline) = writes.get(k) {
            timeline.next_after(&event_id, store).await
        } else {
            None
        };

        match upper {
            Some(next_write) => {
                for reader in read_set.range_between(&event_id, &next_write, store).await {
                    queue.insert(reader);
                }
            }
            None => {
                for reader in read_set.range_after(&event_id, store).await {
                    queue.insert(reader);
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn process_queue<R: IsRuntime, S: Storage<R>, E, H, D>(
        &mut self,
        handler: &mut H,
        event_resolver: &impl async Fn(GroupEventId) -> (SGEventId, E),
        dep_resolver: &mut D,
        store: &GroupStore<'_, R, S>,
        queue: &mut BTreeSet<SGEventId>,
    ) where
        E: Clone,
        H: async FnMut(&E, &mut HandlerCtx<'_, Dep, DepK, DepV, R, S, K, V, D>),
        D: async FnMut(&Dep) -> Timeline<DepK, DepV>,
    {
        while let Some(&event_id) = queue.first() {
            queue.remove(&event_id);

            let local_id = event_id.1;
            let (_, event_data) = event_resolver(local_id).await;

            let old_effects = self.effects.remove(&event_id);
            let (old_reads, old_writes, old_dep_reads) = match old_effects {
                Some(oe) => (oe.reads, oe.writes, oe.dep_reads),
                None => (imbl::OrdSet::new(), OrdMap::new(), OrdMap::new()),
            };

            let mut reads = imbl::OrdSet::new();
            let mut writes = OrdMap::new();
            {
                let mut hctx = HandlerCtx {
                    event_id,
                    reads: &mut reads,
                    writes: &mut writes,
                    self_writes: &self.writes,
                    ext: &mut self.ext,
                    dep_resolver,
                    store,
                };
                handler(&event_data, &mut hctx).await;
            }

            for k in old_reads.iter().filter(|k| !reads.contains(*k)) {
                Self::remove_from_reads(&mut self.reads, k, &event_id);
            }
            for k in reads.iter().filter(|k| !old_reads.contains(*k)) {
                Self::add_to_reads(&mut self.reads, k.clone(), event_id, store).await;
            }

            for (dep, dep_keys) in &old_dep_reads {
                for dep_key in dep_keys {
                    let still_present = self
                        .ext
                        .get(dep)
                        .and_then(|ed| ed.reads.get(dep_key))
                        .is_some_and(|s| s.contains(&event_id));
                    if !still_present {
                        Self::remove_from_ext_reads(&mut self.ext, dep, dep_key, &event_id);
                    }
                }
            }

            for k in old_writes
                .keys()
                .filter(|k| !writes.contains_key(*k))
                .cloned()
                .collect::<Vec<_>>()
            {
                Self::remove_from_timeline(&mut self.writes, &k, &event_id);
                Self::propagate_key_change(&self.reads, &self.writes, &k, event_id, queue, store)
                    .await;
            }

            for (k, new_val) in &writes {
                let changed = match old_writes.get(k) {
                    Some(old_val) => old_val != new_val,
                    None => true,
                };

                match self.writes.entry(k.clone()) {
                    imbl::ordmap::Entry::Occupied(mut entry) => {
                        entry
                            .get_mut()
                            .insert(event_id, new_val.clone(), store)
                            .await;
                    }
                    imbl::ordmap::Entry::Vacant(entry) => {
                        entry.insert(SgOrdMap::unit(event_id, new_val.clone()));
                    }
                }

                if changed {
                    Self::propagate_key_change(
                        &self.reads,
                        &self.writes,
                        k,
                        event_id,
                        queue,
                        store,
                    )
                    .await;
                }
            }

            let mut new_dep_reads: OrdMap<Dep, imbl::OrdSet<DepK>> = OrdMap::new();
            for (dep, ext_dep) in &self.ext {
                for (dep_key, readers) in &ext_dep.reads {
                    if readers.contains(&event_id) {
                        match new_dep_reads.entry(dep.clone()) {
                            imbl::ordmap::Entry::Occupied(mut entry) => {
                                entry.get_mut().insert(dep_key.clone());
                            }
                            imbl::ordmap::Entry::Vacant(entry) => {
                                entry.insert(imbl::OrdSet::unit(dep_key.clone()));
                            }
                        }
                    }
                }
            }

            for (dep, dep_keys) in &old_dep_reads {
                for dep_key in dep_keys {
                    let still_in_new = new_dep_reads.get(dep).is_some_and(|s| s.contains(dep_key));
                    if !still_in_new {
                        Self::remove_from_ext_reads(&mut self.ext, dep, dep_key, &event_id);
                    }
                }
            }

            self.effects.insert(
                event_id,
                EventEffects {
                    reads,
                    writes,
                    dep_reads: new_dep_reads,
                },
            );
        }
    }
}

impl<Dep, DepK, DepV, K, V> Default for StateGraph<Dep, DepK, DepV, K, V>
where
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    K: Ord + Clone + Hash,
    V: Clone + PartialEq + Hash + Ord,
{
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[path = "state_graph_basic.rs"]
mod basic;
#[cfg(test)]
#[path = "state_graph_deps.rs"]
mod deps;
#[cfg(test)]
#[path = "state_graph_poc.rs"]
mod poc;

impl<DepK, DepV> Localizable for Timeline<DepK, DepV>
where
    DepK: Ord + Clone + Hash + Localizable,
    DepV: Clone + PartialEq + Hash + Ord + Localizable,
{
    async fn localize<R: Remapper>(self, remapper: &mut R) -> Result<Self, R::Err> {
        let mut writes = OrdMap::new();
        for (k, mut sg_map) in self.writes {
            let new_k = k.localize(remapper).await?;
            sg_map.try_remap_values(remapper).await?;
            writes.insert(new_k, sg_map);
        }
        Ok(Timeline { writes })
    }
}
