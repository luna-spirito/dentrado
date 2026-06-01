use std::{fmt, hash::Hash, sync::Arc};

use crate::{
    fadeno::bridge::FadenoRuntime,
    types::{AnyLocEventId, LocDataId, LocMsgTypeId, LocSenderId, LocUserId, Localizable},
    utils::{
        self,
        state_graph::{HandlerCtx, StateGraph, StateGraphOut},
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Bits {
    Bits8,
    Bits16,
    Bits32,
    Bits64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NumDesc {
    Fin { nonneg: bool, bits: Bits },
    Inf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BuiltinT {
    Any,
    Bool,
    Eq,
    Loop,
    If,
    IntEq,
    IntGte0,
    List,
    ListIndexL,
    ListLength,
    ListViewL,
    Never,
    OpaqueVal(u64),
    RecordDropFields,
    RecordGet,
    RecordKeepFields,
    Refl,
    RowPlus,
    Tag,
    TagEq,
    TypePlus,
    W,
    WUnwrap,
    WWrap,
    Int(NumDesc),
    IntAdd(NumDesc),
    IntMul(NumDesc),
    IntNeg(NumDesc),
    PropListViewlDec,
    PropLteTrans,

    KolDataId,          // 30
    KolGear,            // 31
    KolId,              // 32
    KolLocEventId,      // 33
    KolMkEventType,     // 34
    KolMkGear,          // 35
    KolQuery,           // 36
    KolUserId,          // 37
    KolEventTypeId,     // 38
    KolMkStateGraph,    // 39
    KolQueryDelta,      // 40
    KolSenderToUser,    // 41
    KolSgCtxDepQuery,   // 42
    KolSgCtxQuery,      // 43
    KolSgCtxUpdate,     // 44
    KolStateGraphApply, // 45
    KolStateGraphOut,   // 46
    KolStateGraphOutT,  // 47
    KolStateGraphT,     // 48
    KolTimestamp,       // 49
    KolResolveData,     // 50

    KolMkQuery,
    KolResolveEvent,
    KolUserEq,
    KolMkAnchorAgg,
    KolAnchorAggApply,
    KolMkTextAgg,
    KolTextAggApply,
    KolTextAggMerge,
    KolSecondaryGet,
    KolLoopIter,
    KolIterList,
    KolSenderId,
    KolLocalUserId,
    KolTextUpdT,
    KolAnchorAggT,
    KolTextAggT,
    KolUnEventType,
    KolPrimaryT,
    KolSecondaryT,
    KolPropQueryEvents,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct InstrRange {
    pub start: u32,
    pub len: u32,
}

impl InstrRange {
    #[inline]
    #[must_use]
    pub fn slice<'a>(&self, pool: &'a [Instr]) -> &'a [Instr] {
        let s = self.start as usize;
        let e = s + self.len as usize;
        &pool[s..e]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct KolGear {
    pub(crate) primary_msg_type: LocMsgTypeId,
    pub(crate) primary_group: LocValue,

    pub(crate) initial_cache: LocValue,
    pub(crate) step: LocValue,
}

impl KolGear {
    #[must_use]
    pub fn group(&self) -> &LocValue {
        &self.primary_group
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Closure {
    pub(crate) captures: Arc<Vec<LocValue>>,
    pub(crate) args: u8,
    pub(crate) body: InstrRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LocValue {
    Num(i64),
    Tag(u64),
    Bool(bool),
    List(Arc<Vec<LocValue>>),
    Record {
        tag_set: Arc<Vec<u64>>,
        fields: Arc<Vec<LocValue>>,
    },
    BuiltinsVar,
    Builtin(BuiltinT),
    Panic,
    Import(u64),

    Closure(Closure),
    Partial {
        func: Arc<LocValue>,
        applied: Arc<Vec<LocValue>>,
    },
    LoopCont {
        step: Arc<LocValue>,
    },

    KolEventId(AnyLocEventId),
    KolDataId(LocDataId),
    KolEventTypeId(LocMsgTypeId),
    KolUserId(LocUserId),
    KolQuery(u64, u64),
    KolPrimary,
    KolGear(Box<KolGear>),
    KolStateGraph(Box<StateGraph<LocValue, LocValue, LocValue, LocValue, LocValue>>),
    KolStateGraphOut(Box<StateGraphOut<LocValue, LocValue>>),
    KolAnchorAgg(utils::text::AnchorAgg),
    KolTextAgg(utils::text::TextAgg),
    KolTextUpd(utils::text::TextUpd),
    KolSecondary,
    KolSenderId(LocSenderId),
}

impl Localizable for LocValue {
    fn localize<U, S, D, E>(
        &self,
        remap_user: &mut U,
        remap_sender: &mut S,
        remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        match self {
            LocValue::KolUserId(lid) => {
                let new_lid = remap_user(*lid)?;
                Ok(if new_lid == *lid {
                    None
                } else {
                    Some(LocValue::KolUserId(new_lid))
                })
            }
            LocValue::KolSenderId(sid) => {
                let new_sid = remap_sender(*sid)?;
                Ok(if new_sid == *sid {
                    None
                } else {
                    Some(LocValue::KolSenderId(new_sid))
                })
            }
            LocValue::KolDataId(did) => {
                let new_did = remap_data(*did)?;
                Ok(if new_did == *did {
                    None
                } else {
                    Some(LocValue::KolDataId(new_did))
                })
            }
            LocValue::KolEventId(_) => {
                panic!("KolEventId must not cross context boundaries — events are mutable")
            }

            LocValue::Num(_)
            | LocValue::Tag(_)
            | LocValue::Bool(_)
            | LocValue::BuiltinsVar
            | LocValue::Builtin(_)
            | LocValue::Panic
            | LocValue::Import(_)
            | LocValue::KolQuery(_, _)
            | LocValue::KolEventTypeId(_) => Ok(None),

            LocValue::KolPrimary | LocValue::KolSecondary => {
                todo!("Don't localize those")
            }

            LocValue::List(vs) => {
                for (i, x) in vs.iter().enumerate() {
                    if let Some(remapped) = x.localize(remap_user, remap_sender, remap_data)? {
                        let mut new = Vec::with_capacity(vs.len());
                        new.extend(vs[..i].iter().cloned());
                        new.push(remapped);
                        for y in &vs[i + 1..] {
                            new.push(
                                y.localize(remap_user, remap_sender, remap_data)?
                                    .unwrap_or_else(|| y.clone()),
                            );
                        }
                        return Ok(Some(LocValue::List(Arc::new(new))));
                    }
                }
                Ok(None)
            }
            LocValue::Record { tag_set, fields } => {
                for (i, x) in fields.iter().enumerate() {
                    if let Some(remapped) = x.localize(remap_user, remap_sender, remap_data)? {
                        let mut new = Vec::with_capacity(fields.len());
                        new.extend(fields[..i].iter().cloned());
                        new.push(remapped);
                        for y in &fields[i + 1..] {
                            new.push(
                                y.localize(remap_user, remap_sender, remap_data)?
                                    .unwrap_or_else(|| y.clone()),
                            );
                        }
                        return Ok(Some(LocValue::Record {
                            tag_set: tag_set.clone(),
                            fields: Arc::new(new),
                        }));
                    }
                }
                Ok(None)
            }
            LocValue::Closure(Closure {
                captures,
                args,
                body,
            }) => {
                for (i, x) in captures.iter().enumerate() {
                    if let Some(remapped) = x.localize(remap_user, remap_sender, remap_data)? {
                        let mut new = Vec::with_capacity(captures.len());
                        new.extend(captures[..i].iter().cloned());
                        new.push(remapped);
                        for y in &captures[i + 1..] {
                            new.push(
                                y.localize(remap_user, remap_sender, remap_data)?
                                    .unwrap_or_else(|| y.clone()),
                            );
                        }
                        return Ok(Some(LocValue::Closure(Closure {
                            captures: Arc::new(new),
                            args: *args,
                            body: *body,
                        })));
                    }
                }
                Ok(None)
            }
            LocValue::Partial { func, applied } => {
                if let Some(remapped_func) = func.localize(remap_user, remap_sender, remap_data)? {
                    let mut new_applied: Vec<LocValue> = Vec::with_capacity(applied.len());
                    for y in applied.iter() {
                        new_applied.push(
                            y.localize(remap_user, remap_sender, remap_data)?
                                .unwrap_or_else(|| y.clone()),
                        );
                    }
                    return Ok(Some(LocValue::Partial {
                        func: Arc::new(remapped_func),
                        applied: Arc::new(new_applied),
                    }));
                }
                for (i, x) in applied.iter().enumerate() {
                    if let Some(remapped) = x.localize(remap_user, remap_sender, remap_data)? {
                        let mut new = Vec::with_capacity(applied.len());
                        new.extend(applied[..i].iter().cloned());
                        new.push(remapped);
                        for y in &applied[i + 1..] {
                            new.push(
                                y.localize(remap_user, remap_sender, remap_data)?
                                    .unwrap_or_else(|| y.clone()),
                            );
                        }
                        return Ok(Some(LocValue::Partial {
                            func: Arc::clone(func),
                            applied: Arc::new(new),
                        }));
                    }
                }
                Ok(None)
            }
            LocValue::LoopCont { step } => Ok(step
                .localize(remap_user, remap_sender, remap_data)?
                .map(|s| LocValue::LoopCont { step: Arc::new(s) })),

            LocValue::KolGear(g) => Ok((**g)
                .localize(remap_user, remap_sender, remap_data)?
                .map(|gear| LocValue::KolGear(Box::new(gear)))),
            LocValue::KolStateGraph(_) => todo!(),
            LocValue::KolStateGraphOut(sg) => {
                let mut any_changed = false;
                let mut new_writes = im::OrdMap::new();
                for (k, timeline) in &sg.writes {
                    let new_k = k
                        .localize(remap_user, remap_sender, remap_data)?
                        .unwrap_or_else(|| k.clone());
                    if new_k != *k {
                        any_changed = true;
                    }

                    let mut new_timeline = timeline.clone();

                    new_timeline.try_remap_values(&mut |v: LocValue| {
                        v.localize(remap_user, remap_sender, remap_data).map(|opt| {
                            if opt.is_some() {
                                any_changed = true;
                            }
                            opt.unwrap_or(v)
                        })
                    })?;

                    new_writes.insert(new_k, new_timeline);
                }
                if any_changed {
                    Ok(Some(LocValue::KolStateGraphOut(Box::new(StateGraphOut {
                        writes: new_writes,
                    }))))
                } else {
                    Ok(None)
                }
            }
            LocValue::KolAnchorAgg(_) | LocValue::KolTextAgg(_) | LocValue::KolTextUpd(_) => {
                Ok(None)
            }
        }
    }
}

impl Localizable for KolGear {
    fn localize<U, S, D, E>(
        &self,
        remap_user: &mut U,
        remap_sender: &mut S,
        remap_data: &mut D,
    ) -> Result<Option<Self>, E>
    where
        U: FnMut(LocUserId) -> Result<LocUserId, E>,
        S: FnMut(LocSenderId) -> Result<LocSenderId, E>,
        D: FnMut(LocDataId) -> Result<LocDataId, E>,
    {
        let group = self
            .primary_group
            .localize(remap_user, remap_sender, remap_data)?;
        let cache = self
            .initial_cache
            .localize(remap_user, remap_sender, remap_data)?;
        let step = self.step.localize(remap_user, remap_sender, remap_data)?;

        if group.is_none() && cache.is_none() && step.is_none() {
            return Ok(None);
        }

        Ok(Some(KolGear {
            primary_msg_type: self.primary_msg_type,
            primary_group: group.unwrap_or_else(|| self.primary_group.clone()),
            initial_cache: cache.unwrap_or_else(|| self.initial_cache.clone()),
            step: step.unwrap_or_else(|| self.step.clone()),
        }))
    }
}

pub(crate) type SgHandlerCtx<'a> =
    HandlerCtx<'a, LocValue, LocValue, LocValue, FadenoRuntime, LocValue, LocValue>;

pub(crate) struct SgHandlerRef<'r, 'h> {
    pub(crate) ctx: &'r SgHandlerCtx<'h>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Instr {
    PushConst(u32),
    PushVar,
    Copy(u8),
    PopVar,
    App(u8),
    Closure {
        captures: u8,
        args: u8,
        body: InstrRange,
    },
    IfElse {
        then_: InstrRange,
        else_: InstrRange,
    },
    MkList(u8),
    MkRecord(u8),
    MkQRecord {
        tag_set: u64,
        n: u8,
    },
    RecordCat,
}

#[derive(Debug, Clone)]
pub struct TagRegistry {
    tag_to_name: Vec<Vec<u8>>,
    name_to_tag: std::collections::HashMap<Vec<u8>, u64>,
    tag_sets: Vec<(Vec<(u64, u8)>, u64)>,
    id_to_ts_idx: std::collections::HashMap<u64, usize>,
}

impl TagRegistry {
    pub(crate) fn tag_set_entries(&self, tag_set_id: u64) -> Option<&[(u64, u8)]> {
        let idx = self.id_to_ts_idx.get(&tag_set_id)?;
        let (entries, _) = &self.tag_sets.get(*idx)?;
        Some(entries)
    }

    #[must_use]
    pub(crate) fn new(tag_to_name: Vec<Vec<u8>>, tag_sets: Vec<(Vec<(u64, u8)>, u64)>) -> Self {
        let name_to_tag = tag_to_name
            .iter()
            .enumerate()
            .map(|(id, n)| (n.clone(), id as u64))
            .collect();
        let id_to_ts_idx = tag_sets
            .iter()
            .enumerate()
            .map(|(i, (_, id))| (*id, i))
            .collect();
        Self {
            tag_to_name,
            name_to_tag,
            tag_sets,
            id_to_ts_idx,
        }
    }

    #[must_use]
    pub(crate) fn name_to_tag(&self, name: &[u8]) -> Option<u64> {
        self.name_to_tag.get(name).copied()
    }

    #[must_use]
    pub(crate) fn tag_to_name(&self, id: u64) -> Option<&[u8]> {
        self.tag_to_name.get(id as usize).map(|x| &**x)
    }

    #[must_use]
    pub fn debug_dump_tags(&self) -> Vec<(u64, String)> {
        self.tag_to_name
            .iter()
            .enumerate()
            .map(|(id, name)| (id as u64, String::from_utf8_lossy(name).to_string()))
            .collect()
    }

    #[must_use]
    pub fn debug_dump_tag_sets(&self) -> Vec<(u64, Vec<(u64, String)>)> {
        self.tag_sets
            .iter()
            .map(|(entries, id)| {
                let fields: Vec<(u64, String)> = entries
                    .iter()
                    .map(|(tid, _)| {
                        (
                            *tid,
                            self.tag_to_name(*tid)
                                .map(|n| String::from_utf8_lossy(n).to_string())
                                .unwrap_or_default(),
                        )
                    })
                    .collect();
                (*id, fields)
            })
            .collect()
    }

    #[must_use]
    pub(crate) fn field_index(&self, tag_set_id: u64, tag_id: u64) -> Option<usize> {
        let idx = self.id_to_ts_idx.get(&tag_set_id)?;
        let (entries, _) = &self.tag_sets.get(*idx)?;
        for (i, (tid, _width)) in entries.iter().enumerate() {
            if *tid == tag_id {
                return Some(i);
            }
        }
        None
    }

    #[must_use]
    pub fn record_get(&self, record: &LocValue, field_name: &[u8]) -> Option<LocValue> {
        let tag_id = self.name_to_tag(field_name)?;
        self.record_get_by_tag(record, tag_id)
    }

    #[must_use]
    pub fn record_get_by_tag(&self, record: &LocValue, tag_id: u64) -> Option<LocValue> {
        match record {
            LocValue::Record { tag_set, fields } => {
                let mut field_offset = 0usize;
                for &ts_id in tag_set.iter() {
                    let idx = self.id_to_ts_idx.get(&ts_id)?;
                    let (entries, _) = &self.tag_sets.get(*idx)?;
                    for (i, (tid, _width)) in entries.iter().enumerate() {
                        if *tid == tag_id {
                            return fields.get(field_offset + i).cloned();
                        }
                    }
                    field_offset += entries.len();
                }
                None
            }
            _ => None,
        }
    }

    #[must_use]
    pub fn make_record(&self, pairs: &[(&[u8], LocValue)]) -> LocValue {
        let tag_ids: Vec<u64> = pairs
            .iter()
            .filter_map(|(name, _)| self.name_to_tag(name))
            .collect();

        if tag_ids.len() == pairs.len() {
            if let Some(ts_id) = self.find_tag_set_for_tags(&tag_ids) {
                let idx = self.id_to_ts_idx.get(&ts_id).expect("tag_set idx");
                let (entries, _) = &self.tag_sets[*idx];
                let max_idx = entries.len();
                let mut fields = vec![LocValue::Panic; max_idx];
                for (i, (tid, _width)) in entries.iter().enumerate() {
                    for (name, val) in pairs {
                        if let Some(tid2) = self.name_to_tag(name) {
                            if tid2 == *tid {
                                fields[i] = val.clone();
                                break;
                            }
                        }
                    }
                }
                return LocValue::Record {
                    tag_set: Arc::new(vec![ts_id]),
                    fields: Arc::new(fields),
                };
            }
        }

        panic!(
            "make_record: no tag_set found for fields: {:?}",
            pairs
                .iter()
                .map(|(n, _)| String::from_utf8_lossy(n))
                .collect::<Vec<_>>()
        )
    }

    #[must_use]
    pub(crate) fn find_tag_set_for_tags(&self, tag_ids: &[u64]) -> Option<u64> {
        for (entries, ts_id) in &self.tag_sets {
            if tag_ids
                .iter()
                .all(|t| entries.iter().any(|(tid, _)| tid == t))
            {
                return Some(*ts_id);
            }
        }
        None
    }

    #[must_use]
    pub fn find_exact_tag_set(&self, tag_ids: &[u64]) -> Option<u64> {
        for (entries, ts_id) in &self.tag_sets {
            if entries.len() == tag_ids.len()
                && entries
                    .iter()
                    .zip(tag_ids)
                    .all(|((tid, _), wanted)| *tid == *wanted)
            {
                return Some(*ts_id);
            }
        }
        None
    }

    fn push_tag_set(&mut self, entries: Vec<(u64, u8)>) -> u64 {
        let id = self.tag_sets.len() as u64;
        let idx = self.tag_sets.len();
        self.tag_sets.push((entries, id));
        self.id_to_ts_idx.insert(id, idx);
        id
    }

    pub(crate) fn ensure_tag_id(&mut self, name: &[u8]) -> u64 {
        if let Some(id) = self.name_to_tag(name) {
            return id;
        }
        let new_id = self.tag_to_name.len() as u64;
        self.tag_to_name.push(name.to_vec());
        self.name_to_tag.insert(name.to_vec(), new_id);
        new_id
    }

    pub(crate) fn ensure_tag_set(&mut self, tag_ids: &[u64]) -> u64 {
        if let Some(ts) = self.find_exact_tag_set(tag_ids) {
            return ts;
        }
        self.push_tag_set(tag_ids.iter().map(|&t| (t, 1u8)).collect())
    }
}

#[derive(Debug, Clone)]
pub struct Compiled {
    pub(crate) tags: TagRegistry,
    pub(crate) constants: Vec<LocValue>,
    pub(crate) pool: Vec<Instr>,
    pub(crate) module_ranges: Vec<InstrRange>,
}

impl Compiled {
    pub fn tags_mut(&mut self) -> &mut TagRegistry {
        &mut self.tags
    }
}

impl fmt::Display for LocValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocValue::Num(n) => write!(f, "{n}"),
            LocValue::Tag(t) => write!(f, "tag#{t}"),
            LocValue::Bool(b) => write!(f, "{b}"),
            LocValue::List(vs) => {
                write!(f, "[")?;
                for (i, v) in vs.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            LocValue::Record { tag_set, fields } => {
                write!(f, "record#{{")?;
                for (i, ts) in tag_set.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{ts}")?;
                }
                write!(f, "}}{{")?;
                for (i, v) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "}}")
            }
            LocValue::BuiltinsVar => write!(f, "fadeno"),
            LocValue::Builtin(b) => write!(f, "builtin({b:?})"),
            LocValue::Panic => write!(f, "PANIC"),
            LocValue::Import(n) => write!(f, "import#{n}"),
            LocValue::Closure(Closure { captures, args, .. }) => {
                write!(f, "<closure captures={} args={}>", captures.len(), args)
            }
            LocValue::Partial { func, applied } => {
                write!(f, "(partial {func}")?;
                for a in applied.iter() {
                    write!(f, " {a}")?;
                }
                write!(f, ")")
            }
            LocValue::KolEventId(id) => write!(f, "EventId({})", id.0),
            LocValue::KolDataId(id) => write!(f, "DataId({})", id.0),
            LocValue::KolEventTypeId(id) => write!(f, "EventTypeId({})", id.0),
            LocValue::KolUserId(id) => write!(f, "UserId({})", id.0),
            LocValue::KolQuery(n, m) => write!(f, "Query({n}, {m})"),
            LocValue::KolGear(g) => write!(f, "Gear({:?})", g.primary_group),
            LocValue::KolStateGraph(_) => write!(f, "StateGraph"),
            LocValue::KolStateGraphOut(_) => write!(f, "StateGraphOut"),
            LocValue::KolAnchorAgg(_) => write!(f, "AnchorAgg"),
            LocValue::KolTextAgg(_) => write!(f, "TextAgg"),
            LocValue::KolTextUpd(_) => write!(f, "TextUpd"),
            LocValue::KolPrimary => write!(f, "PrimaryCtx"),
            LocValue::KolSecondary => write!(f, "SecondaryCtx"),
            LocValue::LoopCont { .. } => write!(f, "<loop-cont>"),
            LocValue::KolSenderId(loc_sender_event_id) => {
                write!(f, "KolSenderId({})", loc_sender_event_id.0)
            }
        }
    }
}

impl fmt::Display for Instr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Instr::PushConst(i) => write!(f, "push({i})"),
            Instr::PushVar => write!(f, "pushvar"),
            Instr::Copy(n) => write!(f, "copy({n})"),
            Instr::PopVar => write!(f, "popvar"),
            Instr::App(n) => write!(f, "app({n})"),
            Instr::Closure {
                captures,
                args,
                body,
            } => {
                write!(
                    f,
                    "closure(captures={captures},args={args},[{start}..{end}))",
                    start = body.start,
                    end = body.start + body.len
                )
            }
            Instr::IfElse { then_, else_ } => {
                write!(
                    f,
                    "ifelse(then=[{ts}..{te}],else=[{es}..{ee}))",
                    ts = then_.start,
                    te = then_.start + then_.len,
                    es = else_.start,
                    ee = else_.start + else_.len
                )
            }
            Instr::MkList(n) => write!(f, "mklist({n})"),
            Instr::MkRecord(n) => write!(f, "mkrecord({n})"),
            Instr::MkQRecord { tag_set, n } => write!(f, "mkqrecord(ts={tag_set},n={n})"),
            Instr::RecordCat => write!(f, "recordcat"),
        }
    }
}
