use im::OrdMap;
use std::cell::RefCell;
use std::{collections::BTreeSet, hash::Hash};

use crate::core::gear::Runtime;
use crate::core::loc_ctx::LocCtx;
use crate::types::AnyLocEventId;
use crate::utils::sg_ord_map::{SgOrdMap, SgOrdSet};

pub use crate::utils::sg_ord_map::{SGBucketId, SGEventId, Timestamp};

pub struct DeltaList<Id> {
    pub removed: Vec<Id>,
    pub added: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StateGraphOut<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord> {
    pub(crate) writes: OrdMap<DepK, SgOrdMap<DepV>>,
}

impl<DepK, DepV> StateGraphOut<DepK, DepV>
where
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
{
    pub(crate) fn query_at<R: Runtime>(
        &self,
        key: &DepK,
        at: SGEventId,
        ctx: &LocCtx<R>,
    ) -> Option<DepV> {
        self.writes
            .get(key)
            .and_then(|timeline| timeline.latest_at(&at, ctx).map(|(_, v)| v.clone()))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&DepK, &SgOrdMap<DepV>)> {
        self.writes.iter()
    }

    #[must_use]
    pub(crate) fn diff_from(&self, old: &Self) -> StateGraphOutDelta<DepK, DepV> {
        use im::ordmap::DiffItem;

        let mut added_keys: OrdMap<DepK, SgOrdMap<DepV>> = OrdMap::new();
        let mut removed_keys: im::OrdSet<DepK> = im::OrdSet::new();
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

        StateGraphOutDelta {
            added_keys,
            removed_keys,
            changed_keys,
        }
    }

    #[must_use]
    pub(crate) fn apply_delta(&self, delta: &StateGraphOutDelta<DepK, DepV>) -> Self {
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
pub(crate) struct StateGraphOutDelta<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord>
{
    pub(crate) added_keys: OrdMap<DepK, SgOrdMap<DepV>>,
    pub(crate) removed_keys: im::OrdSet<DepK>,
    pub(crate) changed_keys: OrdMap<DepK, SgOrdMap<DepV>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ExtDep<DepK: Ord + Clone + Hash, DepV: Clone + PartialEq + Hash + Ord> {
    pub(crate) cached: StateGraphOut<DepK, DepV>,
    pub(crate) reads: OrdMap<DepK, SgOrdSet>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct EventEffects<
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    K: Ord + Clone + Hash,
    V: Clone + PartialEq + Hash + Ord,
> {
    pub(crate) reads: im::OrdSet<K>,
    pub(crate) writes: OrdMap<K, V>,
    pub(crate) dep_reads: OrdMap<Dep, im::OrdSet<DepK>>,
}

pub struct HandlerCtx<
    'a,
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    R: Runtime,
    K: Ord + Clone + Hash,
    V: Clone + Hash,
> {
    pub(crate) event_id: SGEventId,
    reads: RefCell<&'a mut im::OrdSet<K>>,
    writes: RefCell<&'a mut OrdMap<K, V>>,
    pub(crate) self_writes: &'a OrdMap<K, SgOrdMap<V>>,
    ext: RefCell<&'a mut OrdMap<Dep, ExtDep<DepK, DepV>>>,
    dep_resolver: &'a dyn Fn(Dep) -> StateGraphOut<DepK, DepV>,
    ctx: &'a LocCtx<R>,
}

impl<Dep, DepK, DepV, R: Runtime, K, V> HandlerCtx<'_, Dep, DepK, DepV, R, K, V>
where
    Dep: Ord + Clone + Hash,
    DepK: Ord + Clone + Hash,
    DepV: Clone + PartialEq + Hash + Ord,
    K: Ord + Clone + Hash,
    V: Clone + Hash,
{
    pub(crate) fn query(&self, k: &K) -> Option<V> {
        self.reads.borrow_mut().insert(k.clone());
        self.self_writes.get(k).and_then(|timeline| {
            timeline
                .latest_before(&self.event_id, self.ctx)
                .map(|(_, v)| v.clone())
        })
    }

    pub(crate) fn update(&self, k: K, v: V) {
        self.writes.borrow_mut().insert(k, v);
    }

    pub(crate) fn dep_query(&self, dep: &Dep, dep_key: &DepK) -> Option<DepV> {
        let writes = (self.dep_resolver)(dep.clone());

        match self.ext.borrow_mut().entry(dep.clone()) {
            im::ordmap::Entry::Vacant(entry) => {
                entry.insert(ExtDep {
                    cached: writes.clone(),
                    reads: OrdMap::new(),
                });
            }
            im::ordmap::Entry::Occupied(_) => {}
        }

        if let Some(ext_dep) = self.ext.borrow_mut().get_mut(dep) {
            match ext_dep.reads.entry(dep_key.clone()) {
                im::ordmap::Entry::Occupied(mut entry) => {
                    entry.get_mut().insert(self.event_id, self.ctx);
                }
                im::ordmap::Entry::Vacant(entry) => {
                    entry.insert(SgOrdSet::unit(self.event_id));
                }
            }
        }

        writes.query_at(dep_key, self.event_id, self.ctx)
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
    pub(crate) fn new() -> Self {
        Self {
            writes: OrdMap::new(),
            reads: OrdMap::new(),
            effects: OrdMap::new(),
            ext: OrdMap::new(),
        }
    }

    #[must_use]
    pub(crate) fn as_writes(&self) -> StateGraphOut<K, V> {
        StateGraphOut {
            writes: self.writes.clone(),
        }
    }

    pub(crate) fn apply<R: Runtime, E, F>(
        &mut self,
        handler: &F,
        event_resolver: &impl Fn(AnyLocEventId) -> (SGEventId, E),
        dep_resolver: &dyn Fn(Dep) -> StateGraphOut<DepK, DepV>,
        ctx: &LocCtx<R>,
        delta: &DeltaList<AnyLocEventId>,
    ) where
        E: Clone,
        F: Fn(&E, &HandlerCtx<Dep, DepK, DepV, R, K, V>),
    {
        let mut queue: BTreeSet<SGEventId> = BTreeSet::new();

        for &local_id in &delta.removed {
            let (event_id, _) = event_resolver(local_id);
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
                        ctx,
                    );
                }
            }
        }

        let dep_queue = self.detect_dep_changes(dep_resolver, ctx);
        for event_id in dep_queue {
            queue.insert(event_id);
        }

        for &local_id in &delta.added {
            let (event_id, _) = event_resolver(local_id);
            queue.insert(event_id);
        }

        self.process_queue(handler, event_resolver, dep_resolver, ctx, &mut queue);
    }

    pub(crate) fn query(&self, k: &K) -> Option<&V> {
        self.writes
            .get(k)
            .and_then(|timeline| timeline.last())
            .map(|(_, v)| v)
    }

    pub(crate) fn query_at<R: Runtime>(
        &self,
        k: &K,
        event_id: SGEventId,
        ctx: &LocCtx<R>,
    ) -> Option<&V> {
        self.writes
            .get(k)
            .and_then(|timeline| timeline.latest_at(&event_id, ctx).map(|(_, v)| v))
    }

    pub(crate) fn keys(&self) -> impl Iterator<Item = &K> {
        self.writes.keys()
    }

    pub(crate) fn timeline_for(&self, k: &K) -> impl Iterator<Item = (SGEventId, &V)> {
        self.writes.get(k).into_iter().flat_map(SgOrdMap::iter)
    }

    fn detect_dep_changes<R: Runtime>(
        &mut self,
        dep_resolver: &dyn Fn(Dep) -> StateGraphOut<DepK, DepV>,
        ctx: &LocCtx<R>,
    ) -> BTreeSet<SGEventId> {
        use im::ordmap::DiffItem;

        let mut affected = BTreeSet::new();
        let dep_ids: Vec<Dep> = self.ext.keys().cloned().collect();

        for dep in dep_ids {
            let current = dep_resolver(dep.clone());

            {
                let Some(ext_dep) = self.ext.get(&dep) else {
                    continue;
                };

                for outer_item in ext_dep.cached.writes.diff(&current.writes) {
                    match outer_item {
                        DiffItem::Add(dep_key, new_timeline) => {
                            if let Some(readers) = ext_dep.reads.get(dep_key) {
                                if let Some((first_write, _)) = new_timeline.first() {
                                    for reader in readers.range_after(&first_write, ctx) {
                                        affected.insert(reader);
                                    }
                                }
                            }
                        }
                        DiffItem::Remove(dep_key, old_timeline) => {
                            if let Some(readers) = ext_dep.reads.get(dep_key) {
                                if let Some((first_write, _)) = old_timeline.first() {
                                    for reader in readers.range_after(&first_write, ctx) {
                                        affected.insert(reader);
                                    }
                                }
                            }
                        }
                        DiffItem::Update {
                            old: (dep_key, old_timeline),
                            new: (_, new_timeline),
                        } => {
                            if let Some(readers) = ext_dep.reads.get(dep_key) {
                                for inner_item in old_timeline.diff_cloned(new_timeline, ctx) {
                                    let changed_at = *inner_item.key();
                                    Self::add_affected_readers(
                                        new_timeline,
                                        changed_at,
                                        readers,
                                        &mut affected,
                                        ctx,
                                    );
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

    fn add_affected_readers<R: Runtime>(
        new_timeline: &SgOrdMap<DepV>,
        changed_at: SGEventId,
        readers: &SgOrdSet,
        affected: &mut BTreeSet<SGEventId>,
        ctx: &LocCtx<R>,
    ) {
        match new_timeline.next_after(&changed_at, ctx) {
            Some(next_write) => {
                for reader in readers.range_between(&changed_at, &next_write, ctx) {
                    affected.insert(reader);
                }
            }
            None => {
                for reader in readers.range_after(&changed_at, ctx) {
                    affected.insert(reader);
                }
            }
        }
    }

    fn remove_from_reads(reads: &mut OrdMap<K, SgOrdSet>, k: &K, event_id: &SGEventId) {
        match reads.entry(k.clone()) {
            im::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().remove(event_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            im::ordmap::Entry::Vacant(_) => {}
        }
    }

    fn add_to_reads<R: Runtime>(
        reads: &mut OrdMap<K, SgOrdSet>,
        k: K,
        event_id: SGEventId,
        ctx: &LocCtx<R>,
    ) {
        match reads.entry(k) {
            im::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().insert(event_id, ctx);
            }
            im::ordmap::Entry::Vacant(entry) => {
                entry.insert(SgOrdSet::unit(event_id));
            }
        }
    }

    fn remove_from_timeline(writes: &mut OrdMap<K, SgOrdMap<V>>, k: &K, event_id: &SGEventId) {
        match writes.entry(k.clone()) {
            im::ordmap::Entry::Occupied(mut entry) => {
                entry.get_mut().remove(event_id);
                if entry.get().is_empty() {
                    entry.remove();
                }
            }
            im::ordmap::Entry::Vacant(_) => {}
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
                im::ordmap::Entry::Occupied(mut entry) => {
                    entry.get_mut().remove(event_id);
                    if entry.get().is_empty() {
                        entry.remove();
                    }
                }
                im::ordmap::Entry::Vacant(_) => {}
            }
        }
    }

    fn propagate_key_change<R: Runtime>(
        reads: &OrdMap<K, SgOrdSet>,
        writes: &OrdMap<K, SgOrdMap<V>>,
        k: &K,
        event_id: SGEventId,
        queue: &mut BTreeSet<SGEventId>,
        ctx: &LocCtx<R>,
    ) {
        if let Some(read_set) = reads.get(k) {
            let upper = writes
                .get(k)
                .and_then(|timeline| timeline.next_after(&event_id, ctx));

            match upper {
                Some(next_write) => {
                    for reader in read_set.range_between(&event_id, &next_write, ctx) {
                        queue.insert(reader);
                    }
                }
                None => {
                    for reader in read_set.range_after(&event_id, ctx) {
                        queue.insert(reader);
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn process_queue<R: Runtime, E, F>(
        &mut self,
        handler: &F,
        event_resolver: &impl Fn(AnyLocEventId) -> (SGEventId, E),
        dep_resolver: &dyn Fn(Dep) -> StateGraphOut<DepK, DepV>,
        ctx: &LocCtx<R>,
        queue: &mut BTreeSet<SGEventId>,
    ) where
        E: Clone,
        F: Fn(&E, &HandlerCtx<Dep, DepK, DepV, R, K, V>),
    {
        while let Some(&event_id) = queue.first() {
            queue.remove(&event_id);

            let local_id = event_id.1;
            let (_, event_data) = event_resolver(local_id);

            let old_effects = self.effects.remove(&event_id);
            let (old_reads, old_writes, old_dep_reads) = match old_effects {
                Some(oe) => (oe.reads, oe.writes, oe.dep_reads),
                None => (im::OrdSet::new(), OrdMap::new(), OrdMap::new()),
            };

            let mut reads = im::OrdSet::new();
            let mut writes = OrdMap::new();
            {
                let hctx = HandlerCtx {
                    event_id,
                    reads: RefCell::new(&mut reads),
                    writes: RefCell::new(&mut writes),
                    self_writes: &self.writes,
                    ext: RefCell::new(&mut self.ext),
                    dep_resolver,
                    ctx,
                };
                handler(&event_data, &hctx);
            }

            for k in old_reads.iter().filter(|k| !reads.contains(k)) {
                Self::remove_from_reads(&mut self.reads, k, &event_id);
            }
            for k in reads.iter().filter(|k| !old_reads.contains(k)) {
                Self::add_to_reads(&mut self.reads, k.clone(), event_id, ctx);
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
                .filter(|k| !writes.contains_key(k))
                .cloned()
                .collect::<Vec<_>>()
            {
                Self::remove_from_timeline(&mut self.writes, &k, &event_id);
                Self::propagate_key_change(&self.reads, &self.writes, &k, event_id, queue, ctx);
            }

            for (k, new_val) in &writes {
                let changed = match old_writes.get(k) {
                    Some(old_val) => old_val != new_val,
                    None => true,
                };

                match self.writes.entry(k.clone()) {
                    im::ordmap::Entry::Occupied(mut entry) => {
                        entry.get_mut().insert(event_id, new_val.clone(), ctx);
                    }
                    im::ordmap::Entry::Vacant(entry) => {
                        entry.insert(SgOrdMap::unit(event_id, new_val.clone()));
                    }
                }

                if changed {
                    Self::propagate_key_change(&self.reads, &self.writes, k, event_id, queue, ctx);
                }
            }

            let mut new_dep_reads: OrdMap<Dep, im::OrdSet<DepK>> = OrdMap::new();
            for (dep, ext_dep) in &self.ext {
                for (dep_key, readers) in &ext_dep.reads {
                    if readers.contains(&event_id) {
                        match new_dep_reads.entry(dep.clone()) {
                            im::ordmap::Entry::Occupied(mut entry) => {
                                entry.get_mut().insert(dep_key.clone());
                            }
                            im::ordmap::Entry::Vacant(entry) => {
                                entry.insert(im::OrdSet::unit(dep_key.clone()));
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
