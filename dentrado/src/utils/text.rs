#![allow(clippy::pedantic)]

use im::HashMap as ImHashMap;
use similar::{Algorithm, DiffOp, capture_diff_slices};
use std::collections::BTreeSet;

use crate::{
    core::{gear::IsRuntime, loc_ctx::LocCtx},
    types::{GlobalCoreId, LocSenderEventId, LocSenderId},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnchorId(pub LocSenderEventId, pub u32);

pub const ROOT_ANCHOR: AnchorId = AnchorId(
    LocSenderEventId(LocSenderId(u64::MAX), GlobalCoreId(u32::MAX), u32::MAX),
    u32::MAX,
);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AnchorPos {
    pub(crate) parent: AnchorId,
    pub(crate) offset: u32,
}

impl AnchorPos {
    #[must_use]
    pub fn new(parent: AnchorId, offset: u32) -> Self {
        Self { parent, offset }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Anchor {
    pub(crate) id: AnchorId,
    pub(crate) pos: AnchorPos,
}

impl Anchor {
    #[must_use]
    pub(crate) fn new(id: AnchorId, pos: AnchorPos) -> Self {
        Self { id, pos }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextUpd {
    pub(crate) new_anchors: Vec<AnchorPos>,
    pub(crate) new_strings: Vec<String>,
    pub(crate) deletions: Vec<AnchorId>,
}

impl TextUpd {
    #[must_use]
    pub fn new(new_anchors: Vec<AnchorPos>, new_strings: Vec<String>) -> Self {
        assert_eq!(
            new_anchors.len(),
            new_strings.len(),
            "new_anchors and new_strings must be parallel"
        );
        Self {
            new_anchors,
            new_strings,
            deletions: Vec::new(),
        }
    }

    #[must_use]
    pub(crate) fn with_deletions(mut self, deletions: Vec<AnchorId>) -> Self {
        self.deletions = deletions;
        self
    }

    #[must_use]
    pub(crate) fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.new_anchors.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.new_anchors.is_empty() && self.deletions.is_empty()
    }

    #[must_use]
    pub(crate) fn tx_count(&self) -> u64 {
        self.new_strings.len() as u64 + self.deletions.len() as u64
    }
}

pub(crate) type DocSegment = ((AnchorId, u32), String);

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct ChildEntry {
    pub(crate) child_id: AnchorId,
    pub(crate) offset: u32,
}

impl ChildEntry {
    #[must_use]
    pub(crate) fn new(child_id: AnchorId, offset: u32) -> Self {
        Self { child_id, offset }
    }

    fn cmp_rga<R: IsRuntime>(&self, other: &Self, ctx: &LocCtx<R>) -> std::cmp::Ordering {
        let LocSenderEventId(s_sender, s_core, s_tx) = self.child_id.0;
        let LocSenderEventId(o_sender, o_core, o_tx) = other.child_id.0;
        self.offset
            .cmp(&other.offset)
            .then_with(|| o_tx.cmp(&s_tx))
            .then_with(|| o_core.cmp(&s_core))
            .then_with(|| other.child_id.1.cmp(&self.child_id.1))
            .then_with(|| {
                let pk_a = ctx.sender_pk(s_sender).expect("sender_pk: unknown");
                let pk_b = ctx.sender_pk(o_sender).expect("sender_pk: unknown");
                pk_a.cmp(&pk_b)
            })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AnchorAgg {
    children: ImHashMap<AnchorId, Vec<ChildEntry>>,
}

impl std::hash::Hash for AnchorAgg {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (k, v) in self.children.iter() {
            k.hash(state);
            v.hash(state);
        }
    }
}

impl PartialOrd for AnchorAgg {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for AnchorAgg {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.children.iter().cmp(other.children.iter())
    }
}

impl AnchorAgg {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub(crate) fn apply<R: IsRuntime>(
        mut self,
        event_id: LocSenderEventId,
        upd: &TextUpd,
        ctx: &LocCtx<R>,
    ) -> Self {
        for (i, pos) in upd.new_anchors.iter().enumerate() {
            let id = AnchorId(event_id, i as u32);
            let parent_children = self.children.entry(pos.parent).or_default();
            let entry = ChildEntry::new(id, pos.offset);
            if parent_children.iter().any(|e| e.child_id == id) {
                continue;
            }
            let idx = parent_children
                .binary_search_by(|e| e.cmp_rga(&entry, ctx))
                .unwrap_or_else(|x| x);
            parent_children.insert(idx, entry);
        }
        self
    }

    #[must_use]
    pub(crate) fn children(&self, parent: AnchorId) -> &[ChildEntry] {
        match self.children.get(&parent) {
            Some(v) => v,
            None => &[],
        }
    }

    #[must_use]
    pub(crate) fn contains(&self, id: AnchorId) -> bool {
        self.children.contains_key(&id)
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.children.len()
    }

    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.children.is_empty()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextAgg {
    content: ImHashMap<AnchorId, String>,
}

impl std::hash::Hash for TextAgg {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for (k, v) in self.content.iter() {
            k.hash(state);
            v.hash(state);
        }
    }
}

impl PartialOrd for TextAgg {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TextAgg {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.content.iter().cmp(other.content.iter())
    }
}

impl std::hash::Hash for TextUpd {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        for a in &self.new_anchors {
            a.hash(state);
        }
        for s in &self.new_strings {
            s.hash(state);
        }
        for d in &self.deletions {
            d.hash(state);
        }
    }
}

impl PartialOrd for TextUpd {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TextUpd {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.new_anchors
            .cmp(&other.new_anchors)
            .then_with(|| self.new_strings.cmp(&other.new_strings))
            .then_with(|| self.deletions.cmp(&other.deletions))
    }
}

impl TextAgg {
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub(crate) fn fork(&self) -> Self {
        self.clone()
    }

    #[must_use]
    pub(crate) fn apply(mut self, event_id: LocSenderEventId, upd: &TextUpd) -> Self {
        for (i, text) in upd.new_strings.iter().enumerate() {
            let id = AnchorId(event_id, i as u32);
            if !text.is_empty() {
                self.content.insert(id, text.clone());
            }
        }
        for id in &upd.deletions {
            self.content.remove(id);
        }
        self
    }

    #[must_use]
    pub(crate) fn is_deleted(&self, anchor: AnchorId) -> bool {
        !self.content.contains_key(&anchor)
    }

    #[must_use]
    pub(crate) fn merge(self, rhs: &Self) -> Self {
        Self {
            content: rhs.content.clone().union(self.content),
        }
    }

    #[must_use]
    pub(crate) fn get_content(&self, anchor: AnchorId) -> Option<&str> {
        self.content.get(&anchor).map(std::string::String::as_str)
    }

    #[must_use]
    pub fn get_text(&self, agg: &AnchorAgg) -> String {
        let mut result = String::new();
        self.build_text(agg, ROOT_ANCHOR, &mut result);
        result
    }

    #[must_use]
    pub(crate) fn get_document(&self, agg: &AnchorAgg) -> Vec<DocSegment> {
        let mut result = Vec::new();
        self.build_doc(agg, ROOT_ANCHOR, &mut result);
        result
    }

    fn build_text(&self, agg: &AnchorAgg, id: AnchorId, out: &mut String) {
        let children = agg.children(id);

        match self.content.get(&id) {
            None => {
                for entry in children {
                    self.build_text(agg, entry.child_id, out);
                }
            }
            Some(text) if children.is_empty() => {
                out.push_str(text);
            }
            Some(text) => {
                let n_chars = text.chars().count();
                let mut last_end = 0usize;

                for entry in children {
                    let pos = (entry.offset as usize).min(n_chars);
                    if pos > last_end {
                        out.push_str(char_slice(text, last_end, pos));
                    }
                    self.build_text(agg, entry.child_id, out);
                    last_end = pos;
                }
                if last_end < n_chars {
                    out.push_str(char_slice(text, last_end, n_chars));
                }
            }
        }
    }

    fn build_doc(&self, agg: &AnchorAgg, id: AnchorId, out: &mut Vec<DocSegment>) {
        let children = agg.children(id);

        match self.content.get(&id) {
            None => {
                for entry in children {
                    self.build_doc(agg, entry.child_id, out);
                }
            }
            Some(text) if children.is_empty() => {
                out.push(((id, 0), text.clone()));
            }
            Some(text) => {
                let n_chars = text.chars().count();
                let mut last_end = 0usize;
                for entry in children {
                    let pos = (entry.offset as usize).min(n_chars);
                    if pos > last_end {
                        let seg = char_slice(text, last_end, pos).to_string();
                        out.push(((id, last_end as u32), seg));
                    }
                    self.build_doc(agg, entry.child_id, out);
                    last_end = pos;
                }
                if last_end < n_chars {
                    let seg = char_slice(text, last_end, n_chars).to_string();
                    out.push(((id, last_end as u32), seg));
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Document<'a> {
    pub(crate) anchors: &'a AnchorAgg,
    pub(crate) text: &'a TextAgg,
}

impl<'a> Document<'a> {
    #[must_use]
    pub(crate) fn new(anchors: &'a AnchorAgg, text: &'a TextAgg) -> Self {
        Self { anchors, text }
    }

    #[must_use]
    pub(crate) fn get(&self) -> String {
        self.text.get_text(self.anchors)
    }

    #[must_use]
    pub(crate) fn diff(&self, new_text: &str) -> TextUpd {
        let doc_segs = self.text.get_document(self.anchors);
        let mut old_chars: Vec<char> = Vec::new();
        let mut char_src: Vec<(AnchorId, u32)> = Vec::new(); // (anchor, offset_in_anchor)

        for ((anchor, seg_off), text) in &doc_segs {
            for (i, ch) in text.chars().enumerate() {
                old_chars.push(ch);
                char_src.push((*anchor, *seg_off + i as u32));
            }
        }

        let new_chars: Vec<char> = new_text.chars().collect();

        if old_chars == new_chars {
            return TextUpd::empty();
        }

        if old_chars.is_empty() {
            return TextUpd::new(
                vec![AnchorPos::new(ROOT_ANCHOR, 0)],
                vec![new_text.to_string()],
            );
        }

        let ops = capture_diff_slices(Algorithm::Myers, &old_chars, &new_chars);

        let mut dirty: BTreeSet<AnchorId> = BTreeSet::new();
        for op in &ops {
            match op {
                DiffOp::Delete {
                    old_index, old_len, ..
                }
                | DiffOp::Replace {
                    old_index, old_len, ..
                } => {
                    for i in 0..*old_len {
                        dirty.insert(char_src[*old_index + i].0);
                    }
                }
                _ => {}
            }
        }

        if dirty.is_empty() {
            let mut new_anchors = Vec::new();
            let mut new_strings = Vec::new();
            let mut pending = String::new();
            let mut last_pos = AnchorPos::new(ROOT_ANCHOR, 0);

            for op in &ops {
                match op {
                    DiffOp::Equal { old_index, len, .. } => {
                        if !pending.is_empty() {
                            new_anchors.push(last_pos);
                            new_strings.push(std::mem::take(&mut pending));
                        }
                        let src = &char_src[*old_index + len - 1];
                        last_pos = AnchorPos::new(src.0, src.1 + 1);
                    }
                    DiffOp::Insert {
                        new_index, new_len, ..
                    } => {
                        for i in 0..*new_len {
                            pending.push(new_chars[*new_index + i]);
                        }
                    }
                    _ => unreachable!("dirty is empty but got {:?}", op),
                }
            }
            if !pending.is_empty() {
                new_anchors.push(last_pos);
                new_strings.push(pending);
            }
            return TextUpd::new(new_anchors, new_strings);
        }

        let mut new_anchors = Vec::new();
        let mut new_strings = Vec::new();
        let mut pending = String::new();
        let mut pending_pos: Option<AnchorPos> = None;
        let mut last_clean_end: Option<AnchorPos> = None;

        for op in &ops {
            match op {
                DiffOp::Equal { old_index, len, .. } => {
                    for i in 0..*len {
                        let (anchor, off) = char_src[*old_index + i];
                        if dirty.contains(&anchor) {
                            if pending.is_empty() && pending_pos.is_none() {
                                pending_pos =
                                    last_clean_end.or(Some(AnchorPos::new(ROOT_ANCHOR, 0)));
                            }
                            pending.push(old_chars[*old_index + i]);
                        } else {
                            if !pending.is_empty() {
                                new_anchors
                                    .push(pending_pos.unwrap_or(AnchorPos::new(ROOT_ANCHOR, 0)));
                                new_strings.push(std::mem::take(&mut pending));
                                pending_pos = None;
                            }
                            last_clean_end = Some(AnchorPos::new(anchor, off + 1));
                        }
                    }
                }
                DiffOp::Delete { .. } => {
                    if pending.is_empty() && pending_pos.is_none() {
                        pending_pos = last_clean_end.or(Some(AnchorPos::new(ROOT_ANCHOR, 0)));
                    }
                }
                DiffOp::Insert {
                    new_index, new_len, ..
                } => {
                    if pending.is_empty() && pending_pos.is_none() {
                        pending_pos = last_clean_end.or(Some(AnchorPos::new(ROOT_ANCHOR, 0)));
                    }
                    for i in 0..*new_len {
                        pending.push(new_chars[*new_index + i]);
                    }
                }
                DiffOp::Replace {
                    new_index, new_len, ..
                } => {
                    if pending.is_empty() && pending_pos.is_none() {
                        pending_pos = last_clean_end.or(Some(AnchorPos::new(ROOT_ANCHOR, 0)));
                    }
                    for i in 0..*new_len {
                        pending.push(new_chars[*new_index + i]);
                    }
                }
            }
        }

        if !pending.is_empty() {
            new_anchors.push(pending_pos.unwrap_or(AnchorPos::new(ROOT_ANCHOR, 0)));
            new_strings.push(pending);
        }

        let deletions: Vec<AnchorId> = dirty.into_iter().collect();

        TextUpd {
            new_anchors,
            new_strings,
            deletions,
        }
    }
}

fn char_pos_to_byte(s: &str, char_pos: usize) -> usize {
    if char_pos == 0 {
        return 0;
    }
    s.char_indices().nth(char_pos).map_or(s.len(), |(i, _)| i)
}

fn char_slice(s: &str, start: usize, end: usize) -> &str {
    if start >= end {
        return "";
    }
    let start_b = char_pos_to_byte(s, start);
    let end_b = char_pos_to_byte(s, end);
    &s[start_b..end_b]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::gear::EmptyRuntime;

    const S1: LocSenderId = LocSenderId(1);
    const _S2: LocSenderId = LocSenderId(2);
    const CID: GlobalCoreId = GlobalCoreId(0);

    const fn eid(sender: LocSenderId, tx: u32) -> LocSenderEventId {
        LocSenderEventId(sender, CID, tx)
    }

    #[test]
    fn empty_doc() {
        let agg = AnchorAgg::new();
        let text = TextAgg::new();
        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "");
        assert!(text.get_document(&agg).is_empty());
    }

    #[test]
    fn single_anchor() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "hello");

        let segs = text.get_document(&agg);
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0], ((AnchorId(eid(S1, 1), 0), 0), "hello".to_string()));
    }

    #[test]
    fn two_siblings_ordering() {
        let upd = TextUpd::new(
            vec![
                AnchorPos::new(ROOT_ANCHOR, 0), // anchor 0
                AnchorPos::new(ROOT_ANCHOR, 0), // anchor 1
            ],
            vec!["first".to_string(), "second".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "secondfirst");

        let segs = text.get_document(&agg);
        assert_eq!(segs[0].1, "second");
        assert_eq!(segs[1].1, "first");
    }

    #[test]
    fn child_at_offset() {
        let upd1 = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["base".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd1, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd1);

        let parent = AnchorId(eid(S1, 1), 0);
        let upd2 = TextUpd::new(
            vec![AnchorPos::new(parent, 2)],
            vec![" inserted ".to_string()],
        );
        let agg = agg.apply(eid(S1, 2), &upd2, &LocCtx::<EmptyRuntime>::new());
        let text = text.apply(eid(S1, 2), &upd2);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "ba inserted se");
    }

    #[test]
    fn children_ordered_by_offset() {
        let upd1 = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["abcde".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd1, &LocCtx::<EmptyRuntime>::new());

        let parent = AnchorId(eid(S1, 1), 0);
        let upd2 = TextUpd::new(
            vec![AnchorPos::new(parent, 3), AnchorPos::new(parent, 1)],
            vec!["[C1]".to_string(), "[C2]".to_string()],
        );
        let agg = agg.apply(eid(S1, 2), &upd2, &LocCtx::<EmptyRuntime>::new());

        let text = TextAgg::new()
            .apply(eid(S1, 1), &upd1)
            .apply(eid(S1, 2), &upd2);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "a[C2]bc[C1]de");
    }

    #[test]
    fn delete_single() {
        let upd1 = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd1, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd1);

        let upd2 = TextUpd::empty().with_deletions(vec![AnchorId(eid(S1, 1), 0)]);
        let text = text.apply(eid(S1, 2), &upd2);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "");
    }

    #[test]
    fn deleted_parent_keeps_children() {
        let upd1 = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["parent ".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd1, &LocCtx::<EmptyRuntime>::new());
        let mut text = TextAgg::new().apply(eid(S1, 1), &upd1);

        let parent = AnchorId(eid(S1, 1), 0);
        let upd2 = TextUpd::new(vec![AnchorPos::new(parent, 0)], vec!["child".to_string()]);
        let agg = agg.apply(eid(S1, 2), &upd2, &LocCtx::<EmptyRuntime>::new());
        text = text.apply(eid(S1, 2), &upd2);

        let upd3 = TextUpd::empty().with_deletions(vec![parent]);
        text = text.apply(eid(S1, 3), &upd3);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "child");
    }

    #[test]
    fn fork_independence() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let branch = text.fork().apply(
            eid(S1, 2),
            &TextUpd::empty().with_deletions(vec![AnchorId(eid(S1, 1), 0)]),
        );

        assert_eq!(Document::new(&agg, &text).get(), "hello");
        assert_eq!(Document::new(&agg, &branch).get(), "");
    }

    #[test]
    fn reapply_is_idempotent() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["test".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let agg2 = agg.apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text2 = text.apply(eid(S1, 1), &upd);

        assert_eq!(Document::new(&agg2, &text2).get(), "test");
    }

    #[test]
    fn utf8_content() {
        let upd = TextUpd::new(
            vec![
                AnchorPos::new(ROOT_ANCHOR, 0),
                AnchorPos::new(ROOT_ANCHOR, 0),
            ],
            vec!["мир ".to_string(), "Привет ".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "Привет мир ");
    }

    #[test]
    fn char_slice_multibyte() {
        let s = "Привет мир"; // 10 chars
        assert_eq!(char_slice(s, 0, 6), "Привет");
        assert_eq!(char_slice(s, 7, 10), "мир");
        assert_eq!(char_slice(s, 3, 5), "ве");
        assert_eq!(char_slice(s, 0, 0), "");
        assert_eq!(char_slice(s, 5, 5), "");
    }

    #[test]
    fn concurrent_non_overlapping_inserts() {
        let base_upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello world".to_string()],
        );
        let base_agg =
            AnchorAgg::new().apply(eid(S1, 1), &base_upd, &LocCtx::<EmptyRuntime>::new());
        let base_text = TextAgg::new().apply(eid(S1, 1), &base_upd);

        let alice_upd = Document::new(&base_agg, &base_text).diff("hello, world");

        let bob_upd = Document::new(&base_agg, &base_text).diff("hello world!");

        const ALICE: LocSenderId = LocSenderId(2);
        const BOB: LocSenderId = LocSenderId(3);

        let merged_agg = base_agg
            .clone()
            .apply(eid(ALICE, 2), &alice_upd, &LocCtx::<EmptyRuntime>::new())
            .apply(eid(BOB, 3), &bob_upd, &LocCtx::<EmptyRuntime>::new());
        let merged_text = base_text
            .clone()
            .apply(eid(ALICE, 2), &alice_upd)
            .apply(eid(BOB, 3), &bob_upd);

        assert_eq!(
            Document::new(&merged_agg, &merged_text).get(),
            "hello, world!",
            "concurrent non-overlapping inserts should merge cleanly"
        );
    }

    fn assert_diff_roundtrip(
        agg: &AnchorAgg,
        text: &TextAgg,
        new_text: &str,
        event_id: LocSenderEventId,
    ) {
        let doc = Document::new(agg, text);
        let upd = doc.diff(new_text);
        if upd.is_empty() {
            assert_eq!(doc.get(), new_text);
            return;
        }
        let agg2 = agg
            .clone()
            .apply(event_id, &upd, &LocCtx::<EmptyRuntime>::new());
        let text2 = text.clone().apply(event_id, &upd);
        let doc2 = Document::new(&agg2, &text2);
        assert_eq!(
            doc2.get(),
            new_text,
            "diff→apply roundtrip failed:\n  old = {:?}\n  expected = {:?}\n  upd = {:?}",
            doc.get(),
            new_text,
            upd
        );
    }

    #[test]
    fn diff_no_change() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let doc = Document::new(&agg, &text);
        let result = doc.diff("hello");
        assert!(result.is_empty());
    }

    #[test]
    fn diff_insert_at_end() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "hello world", eid(S1, 2));
    }

    #[test]
    fn diff_insert_at_beginning() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "well hello", eid(S1, 2));
    }

    #[test]
    fn diff_insert_in_middle() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello world".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "hello beautiful world", eid(S1, 2));
    }

    #[test]
    fn diff_delete_suffix() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello world".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "hello", eid(S1, 2));
    }

    #[test]
    fn diff_delete_prefix() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello world".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "world", eid(S1, 2));
    }

    #[test]
    fn diff_replace_middle() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello world".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "hallo world", eid(S1, 2));
    }

    #[test]
    fn diff_delete_all() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["hello".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "", eid(S1, 2));
    }

    #[test]
    fn diff_insert_into_empty() {
        let agg = AnchorAgg::new();
        let text = TextAgg::new();

        assert_diff_roundtrip(&agg, &text, "new text", eid(S1, 1));
    }

    #[test]
    fn diff_multi_anchor_preserves_clean() {
        let upd = TextUpd::new(
            vec![
                AnchorPos::new(ROOT_ANCHOR, 0),
                AnchorPos::new(ROOT_ANCHOR, 0),
            ],
            vec!["first".to_string(), "second".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        let doc = Document::new(&agg, &text);
        assert_eq!(doc.get(), "secondfirst");

        assert_diff_roundtrip(&agg, &text, "secondFIRST", eid(S1, 3));
    }

    #[test]
    fn diff_utf8() {
        let upd = TextUpd::new(
            vec![AnchorPos::new(ROOT_ANCHOR, 0)],
            vec!["Привет мир".to_string()],
        );
        let agg = AnchorAgg::new().apply(eid(S1, 1), &upd, &LocCtx::<EmptyRuntime>::new());
        let text = TextAgg::new().apply(eid(S1, 1), &upd);

        assert_diff_roundtrip(&agg, &text, "Привет прекрасный мир", eid(S1, 2));
    }
}
