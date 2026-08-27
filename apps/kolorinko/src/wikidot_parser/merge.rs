//! The merge pass: a single iterative walk over the [`Token`] slice produced
//! by [`lex`], pairing openers with closers into [`Node`]s. No
//! backtracking, no cloning of parsed output — each token is consumed
//! exactly once and nodes are built in place.
//!
//! ## Interval pairing
//!
//! Every wrapping construct (`[[div]]`, `[[span]]`, `**`, `##`) is an
//! interval over the token stream, and Wikidot's tree is what you get by
//! cutting *overlapping* intervals apart: a `[[span]]` crossed by `[[/div]]`
//! closes at the div, then **re-opens after it** —
//!
//! ```text
//! [[div]] hi1 [[span]] hi2 [[/div]] hi3 [[/span]]
//!   → <div>hi1 <span>hi2</span></div><span>hi3</span>
//! ```
//!
//! Open intervals live on an explicit stack of [`Frame`]s in the [`Merger`]
//! — never on the Rust call stack, so nesting depth is bounded by the heap.
//! A closer pairs with the topmost frame carrying its key: every frame it
//! crosses is *cut* (its node is built with the body so far) and re-opened
//! as a fresh frame above the owner, so the split half wraps the content
//! that follows the owner. A closer pairing with nothing is the stray it
//! is: raw text, and parsing continues.
//!
//! Line-scoped inline frames (marks, colors) do not survive a newline: the
//! newline seals them where they stand — a strikethrough whose closer never
//! came degrades to em-dashes, everything else builds with the body so far.
//!
//! The one construct that is not an interval: `[[collapsible]]` plants a
//! header leaf where it opens, and its closer pairs that leaf wherever it
//! sits (see [`close_collapsible`]); containers crossed on the way close
//! normally, without re-opening.
//!
//! The rest mirrors the old chumsky grammar's central ideas:
//! * [`Merger::loop_until_closer_at`] is the old `content_loop`: it stops at
//!   (and consumes) the first closing tag — or comment close — reporting why.
//! * [`Merger::body_until`] is the old `content_before`: elements until a
//!   caller-supplied sigil (a mark, a newline). Closers stop it too, but are
//!   left for the enclosing loop.
//! * Degradation: any token that appears where its construct is not
//!   recognized (`||` outside a table, `[[head]]` outside a listpages body, a
//!   stray `--]`) becomes the text the old parser's character fallback
//!   produced.

use super::lexer::{OpenTag, Params, SectionSlot, Tok, Token, lex};
use super::*;

/// A closing event: a token that pairs with an open [`Frame`]. Bracket
/// closers are consumed by the loops; inline closers (`**`, `##`) by
/// [`Merger::pre_pair`], which claims them the moment their owner is open.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Closer {
    Tag(ClosedTag),
    Mark(TextStyle),
    /// Any `##`. A `##spec|` opener landing in closer position closes the
    /// color too, its `spec|` kept as trailing text.
    Color,
}

/// What a [`Frame`] rebuilds when it closes.
#[derive(Clone, Debug)]
enum FrameKind<'src> {
    Tag(OpenTag<'src>),
    Mark(TextStyle),
    Color(&'src str),
}

/// One open wrapping construct on the interval stack.
#[derive(Clone, Debug)]
struct Frame<'src> {
    /// The pairing key closers match against.
    key: Closer,
    /// Source span of the opener, for raw degradation.
    opener: (usize, usize),
    kind: FrameKind<'src>,
    /// The body accumulated so far; sealed into the node on close.
    children: Content,
    /// A split half re-opened by a foreign closer: its opener is already
    /// confirmed, so an unclosed body builds instead of degrading.
    restarted: bool,
}

impl<'src> Frame<'src> {
    fn line_scoped(&self) -> bool {
        !matches!(self.kind, FrameKind::Tag(_))
    }

    fn restart(&self) -> Frame<'src> {
        Frame {
            key: self.key.clone(),
            opener: self.opener,
            kind: self.kind.clone(),
            children: Content::new(),
            restarted: true,
        }
    }

    fn node(self) -> Node {
        match self.kind {
            FrameKind::Tag(open) => build_tag_node(open, self.children),
            FrameKind::Mark(style) => Node::Container {
                kind: ContainerKind::Style(style),
                content: self.children,
            },
            FrameKind::Color(spec) => Node::Container {
                kind: ContainerKind::Color(normalize_color(spec.to_string())),
                content: self.children,
            },
        }
    }

    /// The claim of a matching closer. A strikethrough with whitespace rims
    /// never matched the Text_Wiki regex: both marks degrade to em-dashes
    /// around the free body.
    fn seal_claim(self) -> Content {
        if matches!(self.kind, FrameKind::Mark(TextStyle::Strikethrough)) && rim_ws(&self.children)
        {
            return self.degraded("—", "—");
        }
        vec![self.node()]
    }

    /// Cut or crossed by a foreign closer: the construct is committed, the
    /// node builds with the body so far.
    fn seal_cut(self) -> Content {
        vec![self.node()]
    }

    /// The region ended (a newline for a line-scoped frame, EOF or an
    /// enclosing stop for any frame) with the closer still missing: a
    /// confirmed construct builds — a split half owns its opener, and an
    /// unclosed mark/color body was always built — while an unconfirmed tag
    /// degrades to its raw opener with the body spliced after. A
    /// strikethrough nobody claimed is just em-dashes.
    fn drain_out(self, src: &str) -> Content {
        match self.kind {
            FrameKind::Tag(_) if !self.restarted => {
                let mut out = Vec::with_capacity(self.children.len() + 1);
                out.push(Node::Raw(src[self.opener.0..self.opener.1].to_string()));
                out.extend(self.children);
                out
            }
            FrameKind::Mark(TextStyle::Strikethrough) if !self.restarted => self.degraded("—", ""),
            _ => vec![self.node()],
        }
    }

    fn degraded(self, lead: &str, tail: &str) -> Content {
        let mut out = Vec::with_capacity(self.children.len() + 2);
        out.push(Node::Text(TextObj::Plain(lead.to_string())));
        out.extend(self.children);
        if !tail.is_empty() {
            out.push(Node::Text(TextObj::Plain(tail.to_string())));
        }
        out
    }
}

/// Why a content loop stopped. The merge counterpart of the old
/// `ContentExitReason`.
#[derive(Clone, Debug)]
enum Stop<'src> {
    Eof,
    Comment,
    /// A consumed closer whose owner sits below the loop that observed it;
    /// the frames it cut ride along, to re-open when the owner seals.
    Closer {
        closer: Closer,
        restarts: Vec<Frame<'src>>,
    },
    /// A consumed closer no frame pairs with: the observing region decides
    /// (the document absorbs it as raw text, nested regions flatten).
    Stray(Closer),
}

/// Parse a token slice as a standalone document, absorbing stray closers as
/// raw text (the old `parse()`). Pending collapsible openers whose closer
/// never came degrade to their verbatim text.
pub(crate) fn parse_toks(src: &str, toks: &[Token]) -> Content {
    let mut m = Merger {
        src,
        toks,
        pos: 0,
        frames: Vec::new(),
    };
    let mut content = m.content();
    degrade_unclosed_collapsibles(&mut content);
    merge_text(content)
}

/// Recursively parse a raw markup fragment (include values, variable
/// defaults, link/tab text): lex it and merge it as a document.
pub(crate) fn parse_sub(src: &str) -> Content {
    let toks = lex(src);
    parse_toks(src, &toks)
}

pub(crate) struct Merger<'src> {
    src: &'src str,
    toks: &'src [Token<'src>],
    pos: usize,
    /// Open wrapping constructs, outermost first. Every parse loop shares
    /// this one stack; a loop only ever resolves frames above the depth it
    /// entered at, so a region (a table cell, a heading line) ends with its
    /// own frames drained and the enclosing ones untouched.
    frames: Vec<Frame<'src>>,
}

/// Uniform `[[…]]`-opener handler signature for the constructs parsed as
/// whole regions (frames are pushed directly, without an arm).
type TagArm<'src> =
    fn(&mut Merger<'src>, (usize, usize), OpenTag<'src>) -> (Content, Option<Stop<'src>>);

/// Uniform token-handler signature for [`Merger::element`].
type TokArm<'src> = fn(&mut Merger<'src>, &Tok<'src>) -> (Content, Option<Stop<'src>>);

/// The closer a plain wrapping tag pairs with, or `None` for constructs
/// parsed as whole regions.
fn closed_tag_of(tag: &OpenTag) -> Option<ClosedTag> {
    match tag {
        OpenTag::Div { .. } => Some(ClosedTag::Div),
        OpenTag::Span { .. } => Some(ClosedTag::Span),
        OpenTag::Anchor { .. } => Some(ClosedTag::Anchor),
        OpenTag::Footnote => Some(ClosedTag::Footnote),
        OpenTag::ModuleBlock { .. } => Some(ClosedTag::Module),
        OpenTag::Size(_) => Some(ClosedTag::Size),
        OpenTag::IfTags(_) => Some(ClosedTag::IfTags),
        OpenTag::Align { floating, side } => Some(ClosedTag::Align {
            floating: *floating,
            side: *side,
        }),
        OpenTag::Cell { .. } => Some(ClosedTag::Cell),
        _ => None,
    }
}

/// The node a closed tag frame builds from its opener and body — the old
/// per-arm `build` closures in one place.
fn build_tag_node(open: OpenTag, children: Content) -> Node {
    match open {
        OpenTag::Div { underscore, params } => Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: !underscore,
                params,
            },
            content: children,
        },
        OpenTag::Span { params } => Node::Container {
            kind: ContainerKind::Div {
                inline: true,
                block: false,
                params,
            },
            content: children,
        },
        // `[[a]]` is just a link that also carries a class: classify the
        // href like any other target (so it gets auto-rewritten), and thread
        // the class through to the renderer.
        OpenTag::Anchor { params } => {
            let class = attr_value(&params, "class").filter(|s| !s.is_empty());
            let target = params
                .get("href")
                .map(|v| parse_link_target_objs(v))
                .unwrap_or(LinkTarget::Url("#".to_string()));
            Node::Link {
                target,
                text: children,
                class,
            }
        }
        OpenTag::Footnote => Node::Footnote(children),
        OpenTag::ModuleBlock { name, params } => Node::ModuleBlock {
            name,
            params,
            body: children,
        },
        OpenTag::Size(arg) => Node::Container {
            kind: ContainerKind::Size(arg.to_string()),
            content: children,
        },
        OpenTag::IfTags(filter) => {
            let (has_all, has_none) = parse_tag_filter(filter);
            Node::Container {
                kind: ContainerKind::IfTags { has_all, has_none },
                content: children,
            }
        }
        OpenTag::Align { floating, side } => Node::Container {
            kind: ContainerKind::Align(Align { floating, side }),
            content: children,
        },
        OpenTag::Cell { header, params } => Node::BlockCell(BlockCell {
            header,
            params,
            content: children,
        }),
        _ => unreachable!("frame tags only"),
    }
}

/// Whether a body's first or last text child has a whitespace edge — the
/// Text_Wiki strikethrough rim rule.
fn rim_ws(children: &Content) -> bool {
    matches!(children.first(), Some(Node::Text(TextObj::Plain(s))) if s.starts_with(char::is_whitespace))
        || matches!(children.last(), Some(Node::Text(TextObj::Plain(s))) if s.ends_with(char::is_whitespace))
}

/// The raw text a stray closer renders as when no frame owns it.
fn stray_raw(closer: Closer) -> Node {
    match closer {
        Closer::Tag(tag) => Node::Raw(format!("[[/{}]]", tag.opener_str())),
        Closer::Mark(style) => Node::Raw(match style {
            TextStyle::Italic => "//".to_string(),
            TextStyle::Bold => "**".to_string(),
            TextStyle::Underline => "__".to_string(),
            TextStyle::Strikethrough => "--".to_string(),
        }),
        Closer::Color => Node::Raw("##".to_string()),
    }
}

enum PairOutcome<'src> {
    /// Sealed, emitted, restarts pushed — the loop carries on.
    Handled,
    /// The owner sits below this loop's depth: stop and propagate.
    Below(Vec<Frame<'src>>),
    /// No frame pairs with the closer.
    Ownerless,
}

impl<'src> Merger<'src> {
    fn peek(&self) -> Option<&Tok<'src>> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek_is(&self, f: impl FnOnce(&Tok) -> bool) -> bool {
        self.peek().is_some_and(f)
    }

    fn text_node(&self, s: &str) -> Node {
        Node::Text(TextObj::Plain(s.to_string()))
    }

    /// Degrade a token to text, restoring any spaces the lexer's construct
    /// grammar ate (a `||` cell prefix).
    fn degrade(&mut self) -> Node {
        let t_start = self.toks[self.pos].start;
        self.pos += 1;
        let end = self.toks.get(self.pos).map_or(self.src.len(), |n| n.start);
        self.text_node(&self.src[t_start..end])
    }

    fn at_line_start(&self) -> bool {
        self.pos == 0 || matches!(self.toks[self.pos - 1].tok, Tok::Newline)
    }

    // ── frame plumbing ────────────────────────────────────────────────────

    /// Where element output goes: the innermost open frame if one belongs to
    /// the current region, else the loop's own sink.
    fn emit(&mut self, depth: usize, sink: &mut Content, nodes: Content) {
        if nodes.is_empty() {
            return;
        }
        if self.frames.len() > depth {
            self.frames
                .last_mut()
                .expect("len > depth")
                .children
                .extend(nodes);
        } else {
            sink.extend(nodes);
        }
    }

    /// Run `f` on the current emission target (see [`Merger::emit`]).
    fn with_target<R>(
        &mut self,
        depth: usize,
        sink: &mut Content,
        f: impl FnOnce(&mut Content) -> R,
    ) -> R {
        let target = self
            .frames
            .len()
            .checked_sub(1)
            .filter(|_| self.frames.len() > depth);
        match target {
            Some(i) => f(&mut self.frames[i].children),
            None => f(sink),
        }
    }

    /// Resolve every frame this region opened and never closed: each drains
    /// into the frame (or sink) below it, innermost first.
    fn drain(&mut self, depth: usize, sink: &mut Content) {
        while self.frames.len() > depth {
            let frame = self.frames.pop().expect("len > depth");
            let out = frame.drain_out(self.src);
            self.emit(depth, sink, out);
        }
    }

    /// The closing event an inline token carries, if any.
    fn closer_event(tok: &Tok) -> Option<Closer> {
        match tok {
            Tok::Mark(style) => Some(Closer::Mark(*style)),
            Tok::ColorClose => Some(Closer::Color),
            _ => None,
        }
    }
    /// The token-level closer routing every loop shares, run before element
    /// dispatch. An inline closer whose owner is open pairs right here; a
    /// `##spec|` in closer position closes the color keeping its `spec|` as
    /// trailing text; a newline seals the line-scoped frames it reaches.
    /// `Some(stop)` = the loop must stop and propagate.
    fn pre_pair(&mut self, depth: usize, sink: &mut Content) -> Option<Stop<'src>> {
        if self.peek_is(|t| matches!(t, Tok::Newline))
            && self.frames.len() > depth
            && self.frames.last().is_some_and(|f| f.line_scoped())
        {
            while self.frames.len() > depth && self.frames.last().is_some_and(|f| f.line_scoped()) {
                let frame = self.frames.pop().expect("checked len");
                let out = frame.drain_out(self.src);
                self.emit(depth, sink, out);
            }
            return None;
        }
        if let Some(Tok::ColorOpen(spec)) = self.peek().cloned()
            && self.frames.len() > depth
            && matches!(self.frames.last().map(|f| &f.key), Some(Closer::Color))
        {
            let frame = self.frames.pop().expect("checked len");
            self.pos += 1;
            let mut out = frame.seal_claim();
            out.push(self.text_node(&format!("{spec}|")));
            self.emit(depth, sink, out);
            return None;
        }
        if let Some(closer) = self.peek().cloned().and_then(|t| Self::closer_event(&t))
            && self.frames.iter().any(|f| f.key == closer)
        {
            self.pos += 1;
            return match self.pair(&closer, depth, sink, Vec::new()) {
                PairOutcome::Handled => None,
                PairOutcome::Below(restarts) => Some(Stop::Closer { closer, restarts }),
                // Checked just above; fall through to element dispatch.
                PairOutcome::Ownerless => None,
            };
        }
        None
    }

    /// Pair a consumed closer with the topmost frame carrying its key. The
    /// frames it crosses are cut: sealed into their parent (the construct is
    /// committed) and re-opened above the owner — the split half of the
    /// interval. The owner seals into wherever the current region emits.
    fn pair(
        &mut self,
        closer: &Closer,
        depth: usize,
        sink: &mut Content,
        mut restarts: Vec<Frame<'src>>,
    ) -> PairOutcome<'src> {
        let Some(owner) = self.frames.iter().rposition(|f| &f.key == closer) else {
            return PairOutcome::Ownerless;
        };
        let mut cuts = Vec::new();
        while self.frames.len() > owner + 1 {
            let frame = self.frames.pop().expect("len > owner + 1");
            cuts.push(frame.restart());
            let sealed = frame.seal_cut();
            self.frames
                .last_mut()
                .expect("owner still open")
                .children
                .extend(sealed);
        }
        cuts.reverse();
        restarts.splice(0..0, cuts);
        if owner < depth {
            return PairOutcome::Below(restarts);
        }
        let frame = self.frames.pop().expect("owner exists");
        let sealed = frame.seal_claim();
        self.emit(depth, sink, sealed);
        for frame in restarts {
            self.frames.push(frame);
        }
        PairOutcome::Handled
    }

    /// Route a consumed bracket closer through [`Merger::pair`].
    fn closer_step(
        &mut self,
        closer: Closer,
        depth: usize,
        sink: &mut Content,
    ) -> Option<Stop<'src>> {
        match self.pair(&closer, depth, sink, Vec::new()) {
            PairOutcome::Handled => None,
            PairOutcome::Below(restarts) => Some(Stop::Closer { closer, restarts }),
            PairOutcome::Ownerless => Some(Stop::Stray(closer)),
        }
    }

    /// One element through the shared plumbing: token-level closer routing
    /// first, then dispatch, the output landing in the open frame or the
    /// sink, and a closer that escaped a nested region re-paired at this
    /// depth. `Some(stop)` = the loop must stop and propagate.
    fn element_step(&mut self, depth: usize, sink: &mut Content) -> Option<Stop<'src>> {
        if let Some(stop) = self.pre_pair(depth, sink) {
            return Some(stop);
        }
        let (sub, reason) = self.element();
        self.emit(depth, sink, sub);
        match reason {
            Some(Stop::Closer { closer, restarts }) => {
                match self.pair(&closer, depth, sink, restarts) {
                    PairOutcome::Handled => None,
                    PairOutcome::Below(restarts) => Some(Stop::Closer { closer, restarts }),
                    PairOutcome::Ownerless => Some(Stop::Stray(closer)),
                }
            }
            stop => stop,
        }
    }

    /// Route a consumed `[[/collapsible]]`: pair it with the latest unpaired
    /// header leaf reachable from here — sealing crossed frames on the way
    /// down (a collapsible never re-opens: the leaf pairs, the containers
    /// survive). `false` when nothing pairs below this region either.
    fn collapsible_step(&mut self, depth: usize, sink: &mut Content) -> bool {
        if self.with_target(depth, sink, close_collapsible) {
            return true;
        }
        while self.frames.len() > depth
            && !self.with_target(depth, sink, |c| c.iter().any(node_has_pending))
        {
            let frame = self.frames.pop().expect("len > depth");
            let sealed = frame.seal_cut();
            self.emit(depth, sink, sealed);
        }
        self.with_target(depth, sink, close_collapsible)
    }

    // ── content loops ─────────────────────────────────────────────────────

    /// The old `content`: a full document, folding any stray closer back in
    /// as raw text and continuing.
    pub(crate) fn content(&mut self) -> Content {
        let mut nodes = Content::new();
        loop {
            let (sub, _, stop) = self.loop_until_closer_at(false);
            nodes.extend(sub);
            match stop {
                Stop::Eof => return nodes,
                Stop::Comment => nodes.push(Node::Raw("--]".to_string())),
                // A closer escaping a nested region pairs with a frame here
                // or is the stray it is; either way the document continues.
                Stop::Closer { closer, .. } | Stop::Stray(closer) => nodes.push(stray_raw(closer)),
            }
        }
    }

    /// Elements until the first closer — the old `content_loop`. Returns the
    /// nodes, the byte offset where the body ended (the closer's start — raw
    /// bodies slice `src[opener.end..end]`), and why it stopped. A closer
    /// pairing with a frame of this region is claimed here and the loop
    /// carries on; anything else stops it.
    fn loop_until_closer_at(&mut self, comment: bool) -> (Content, usize, Stop<'src>) {
        let depth = self.frames.len();
        let mut nodes = Content::new();
        loop {
            match self.peek().cloned() {
                None => {
                    self.drain(depth, &mut nodes);
                    return (nodes, self.src.len(), Stop::Eof);
                }
                Some(Tok::Close(tag)) => {
                    let end = self.toks[self.pos].start;
                    self.pos += 1;
                    // A `[[/collapsible]]` closer pairs with the header leaf
                    // planted in whatever is accumulating (see
                    // [`Merger::collapsible_arm`]); only when nothing pairs
                    // here does it travel as a stray.
                    if matches!(tag, ClosedTag::Collapsible)
                        && self.collapsible_step(depth, &mut nodes)
                    {
                        continue;
                    }
                    if let Some(stop) = self.closer_step(Closer::Tag(tag), depth, &mut nodes) {
                        self.drain(depth, &mut nodes);
                        return (nodes, end, stop);
                    }
                }
                Some(Tok::CommentClose) if comment => {
                    let end = self.toks[self.pos].start;
                    self.pos += 1;
                    self.drain(depth, &mut nodes);
                    return (nodes, end, Stop::Comment);
                }
                _ => {
                    let start = self.toks[self.pos].start;
                    if let Some(stop) = self.element_step(depth, &mut nodes) {
                        self.drain(depth, &mut nodes);
                        return (nodes, start, stop);
                    }
                }
            }
        }
    }

    /// Elements until a stop-sigil token (peeked, the caller consumes it) —
    /// the old `content_before`. Sigils and unconsumed closers stop it only
    /// at its own depth: a frame opened inside shields its tokens, exactly
    /// as the old recursive body grammars did. A stop returned by a nested
    /// element rides along.
    fn body_until(
        &mut self,
        stop_is: impl Fn(&Tok) -> bool + Copy,
    ) -> (Content, Option<Stop<'src>>) {
        let depth = self.frames.len();
        let mut nodes = Content::new();
        loop {
            if self.frames.len() > depth {
                match self.peek().cloned() {
                    None => {
                        self.drain(depth, &mut nodes);
                        return (nodes, None);
                    }
                    // A closer of a frame opened inside this body: claim it
                    // here, just as the old recursive container loop did.
                    Some(Tok::Close(tag)) => {
                        self.pos += 1;
                        if let Some(stop) = self.closer_step(Closer::Tag(tag), depth, &mut nodes) {
                            self.drain(depth, &mut nodes);
                            return (nodes, Some(stop));
                        }
                        continue;
                    }
                    _ => {}
                }
            } else {
                match self.peek() {
                    None => return (nodes, None),
                    // Unconsumed: the enclosing loop's grammar owns it.
                    Some(Tok::Close(_)) => return (nodes, None),
                    Some(tok) if stop_is(tok) => return (nodes, None),
                    _ => {}
                }
            }
            if let Some(stop) = self.element_step(depth, &mut nodes) {
                self.drain(depth, &mut nodes);
                return (nodes, Some(stop));
            }
        }
    }

    /// The whitespace the old `container_balanced` loops skipped between
    /// children (' ' and '\n').
    fn skip_container_ws(&mut self) {
        while self.peek_is(|t| match t {
            Tok::Newline => true,
            Tok::Text(s) => s.bytes().all(|b| b == b' '),
            _ => false,
        }) {
            self.pos += 1;
        }
    }

    // ── element dispatch ──────────────────────────────────────────────────

    /// One element: `(nodes, None)` normally, `(nodes, Some(stop))` when a
    /// nested region ended against a closer it could not pair. Wrapping
    /// constructs push [`Frame`]s instead of recursing — the element itself
    /// is just the opener, the body accumulates in the frame. Deliberately a
    /// fn-pointer dispatcher: it sits on every parse cycle, where per-arm
    /// stack slots would multiply per construct.
    fn element(&mut self) -> (Content, Option<Stop<'src>>) {
        let Some(tok) = self.peek().cloned() else {
            return (Content::new(), None);
        };
        let arm: TokArm<'src> = match &tok {
            Tok::Text(_) => Self::text,
            Tok::Newline => Self::newline,
            Tok::Escape(_) => Self::escape,
            Tok::Url(_) => Self::url_link,
            Tok::Mark(_) => Self::mark,
            Tok::SupMark => Self::sup,
            Tok::SubMark => Self::sub,
            Tok::ColorOpen(_) => Self::color_span,
            Tok::ColorClose => Self::degrade_tok,
            Tok::Tt(_) => Self::tt,
            Tok::Clearfloat(_) => Self::clearfloat,
            Tok::AnchorTarget(_) => Self::anchor_target,
            Tok::IfExpr { .. } => Self::if_expr,
            Tok::ModuleVar { .. } => Self::module_var,
            Tok::IncludeVar { .. } => Self::include_var,
            Tok::CommentClose => Self::stray_comment_close,
            Tok::QuoteMark => Self::blockquote,
            Tok::Heading(_) => Self::heading,
            Tok::Rule => Self::rule,
            Tok::ListMark { .. } => Self::list_line,
            Tok::CenterEq => Self::center_eq,
            Tok::Pipe2 if self.at_line_start() => Self::table_lines,
            Tok::Pipe2 | Tok::Tilde | Tok::CellAlign(_) => Self::degrade_tok,
            Tok::Link3 { .. } => Self::link3,
            Tok::Link1 { .. } => Self::link1,
            Tok::CommentOpen => Self::comment,
            // Unreachable at loop level (the loops pair closers first);
            // defensive degrade like the old char fallback.
            Tok::Close(_) => Self::degrade_tok,
            // The lexer plants the toggle-header leaf beside the opener;
            // this pass grows its own from the opener token, so the leaf
            // is noise.
            Tok::CollapsibleHdr(_) => Self::skip_tok,
            Tok::Open(_) => Self::open_bracket,
        };
        arm(self, &tok)
    }

    fn skip_tok(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.pos += 1;
        (Content::new(), None)
    }

    fn text(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Text(s) = tok else { unreachable!() };
        self.pos += 1;
        // Typography (the Text_Wiki rule): `...` / `. . .` → ellipsis.
        (vec![self.text_node(&typography(s))], None)
    }

    fn tt(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Tt(body) = tok else { unreachable!() };
        self.pos += 1;
        (
            vec![Node::Container {
                kind: ContainerKind::Tt,
                content: parse_sub(body),
            }],
            None,
        )
    }

    fn clearfloat(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Clearfloat(side) = *tok else {
            unreachable!()
        };
        self.pos += 1;
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        (vec![Node::Clearfloat(side)], None)
    }

    fn anchor_target(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::AnchorTarget(name) = tok else {
            unreachable!()
        };
        self.pos += 1;
        (vec![Node::AnchorTarget(name.to_string())], None)
    }

    fn if_expr(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::IfExpr { cond, then, els } = tok else {
            unreachable!()
        };
        self.pos += 1;
        (
            vec![Node::IfExpr {
                cond: text_objs_of(cond),
                then: parse_sub(then),
                els: els.map(parse_sub).unwrap_or_default(),
            }],
            None,
        )
    }

    fn newline(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.pos += 1;
        (vec![self.text_node("\n")], None)
    }

    fn escape(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Escape(body) = tok else {
            unreachable!()
        };
        self.pos += 1;
        (vec![self.text_node(body)], None)
    }

    fn url_link(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Url(u) = tok else { unreachable!() };
        self.pos += 1;
        (
            vec![Node::Link {
                target: LinkTarget::Url(u.to_string()),
                text: vec![self.text_node(u)],
                class: None,
            }],
            None,
        )
    }

    fn module_var(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::ModuleVar { name, default } = tok else {
            unreachable!()
        };
        self.pos += 1;
        (
            vec![Node::Text(TextObj::ModuleVar {
                name: name.to_string(),
                default: default.map(str::to_string),
            })],
            None,
        )
    }

    fn include_var(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::IncludeVar { name, default } = tok else {
            unreachable!()
        };
        self.pos += 1;
        (
            vec![Node::Text(TextObj::IncludeVar {
                name: name.to_string(),
                default: default.map(parse_sub),
            })],
            None,
        )
    }

    fn sup(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.sup_sub(true)
    }

    fn sub(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.sup_sub(false)
    }

    fn sup_sub(&mut self, sup: bool) -> (Content, Option<Stop<'src>>) {
        let is_mark: fn(&Tok) -> bool = if sup {
            |t| matches!(t, Tok::SupMark)
        } else {
            |t| matches!(t, Tok::SubMark)
        };
        self.pos += 1;
        let (body, reason) = self.body_until(|t| is_mark(t) || matches!(t, Tok::Newline));
        if self.peek_is(is_mark) {
            self.pos += 1;
        }
        let node = if sup {
            Node::SupSubscript {
                sup: body,
                sub: Content::new(),
            }
        } else {
            Node::SupSubscript {
                sup: Content::new(),
                sub: body,
            }
        };
        (vec![node], reason)
    }

    fn heading(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Heading(level) = *tok else {
            unreachable!()
        };
        self.pos += 1;
        let (content, reason) = self.line_body();
        (
            vec![Node::Heading {
                level,
                anchor: None,
                content,
            }],
            reason,
        )
    }

    fn rule(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.pos += 1;
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        (vec![Node::HorizontalRule], None)
    }

    fn list_line(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        (vec![self.list_block()], None)
    }

    fn center_eq(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.pos += 1;
        let (content, reason) = self.line_body();
        (
            vec![Node::Container {
                kind: ContainerKind::Align(Align {
                    floating: false,
                    side: AlignSide::Center,
                }),
                content,
            }],
            reason,
        )
    }

    fn table_lines(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        (vec![self.table_block()], None)
    }

    fn degrade_tok(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        (vec![self.degrade()], None)
    }

    fn link3(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Link3 { target, text } = tok else {
            unreachable!()
        };
        self.pos += 1;
        let objs = text_objs_of(target.trim());
        (
            vec![Node::Link {
                target: parse_link_target_objs(&objs),
                text: match text {
                    Some(t) => parse_sub(t),
                    None => objs.into_iter().map(Node::Text).collect(),
                },
                class: None,
            }],
            None,
        )
    }

    fn link1(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Link1 { target, text } = tok else {
            unreachable!()
        };
        self.pos += 1;
        let objs = text_objs_of(target.trim());
        (
            vec![Node::Link {
                target: parse_link_target_objs(&objs),
                text: match text {
                    Some(t) => vec![self.text_node(t.trim())],
                    None => objs.into_iter().map(Node::Text).collect(),
                },
                class: None,
            }],
            None,
        )
    }

    /// `//`, `**`, `__`, `--` — an interval opener (a closer pairing with an
    /// open frame never reaches this arm; see [`Merger::pre_pair`]). An
    /// opener immediately followed by a space is not a span (the old
    /// `just(' ').not()` guard); `-- ` there is an em-dash that swallows the
    /// space, any other mark is plain text.
    fn mark(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::Mark(style) = *tok else {
            unreachable!()
        };
        let t = &self.toks[self.pos];
        let followed_by_space = self.src[t.end..].starts_with(' ');
        self.pos += 1;
        match (style, followed_by_space) {
            (TextStyle::Strikethrough, true) => {
                // The em-dash consumed `-- ` including the space.
                let mut nodes = vec![self.text_node("— ")];
                if let Some(Tok::Text(rest)) = self.peek() {
                    let rest = &rest[1..];
                    self.pos += 1;
                    if !rest.is_empty() {
                        nodes.push(self.text_node(&typography(rest)));
                    }
                }
                (nodes, None)
            }
            (_, true) => {
                // Not an opener; the mark itself is text (the space belongs
                // to the following text token).
                let t_start = self.toks[self.pos - 1].start;
                (
                    vec![self.text_node(&self.src[t_start..self.toks[self.pos - 1].end])],
                    None,
                )
            }
            (_, false) => {
                self.frames.push(Frame {
                    key: Closer::Mark(style),
                    opener: (t.start, t.end),
                    kind: FrameKind::Mark(style),
                    children: Content::new(),
                    restarted: false,
                });
                (Content::new(), None)
            }
        }
    }

    /// `##spec|…##`: the opener of a color interval.
    fn color_span(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Tok::ColorOpen(spec) = tok else {
            unreachable!()
        };
        let t = &self.toks[self.pos];
        self.pos += 1;
        self.frames.push(Frame {
            key: Closer::Color,
            opener: (t.start, t.end),
            kind: FrameKind::Color(spec),
            children: Content::new(),
            restarted: false,
        });
        (Content::new(), None)
    }

    /// A stray `--]` outside a comment: the old parser saw a `--`
    /// strikethrough opener whose body starts with `]`.
    fn stray_comment_close(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        self.pos += 1;
        let mut body = vec![self.text_node("]")];
        let (tail, reason) =
            self.body_until(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough) | Tok::Newline));
        body.extend(tail);
        if self.peek_is(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough))) {
            self.pos += 1;
        }
        (
            vec![Node::Container {
                kind: ContainerKind::Style(TextStyle::Strikethrough),
                content: body,
            }],
            reason,
        )
    }

    fn blockquote(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let mut inner: Vec<Token> = Vec::new();
        while self.peek_is(|t| matches!(t, Tok::QuoteMark)) {
            self.pos += 1; // the stripped marker
            while let Some(t) = self.toks.get(self.pos) {
                if matches!(t.tok, Tok::Newline) {
                    inner.push(t.clone());
                    self.pos += 1;
                    break;
                }
                inner.push(t.clone());
                self.pos += 1;
            }
            if !self.peek_is(|t| matches!(t, Tok::QuoteMark)) {
                break;
            }
        }
        // A fresh merger — and a fresh frame stack — for the stripped lines:
        // a quote is a block boundary, so intervals cut inside it stay there
        // (the token buffer is quote-local and cannot travel out).
        let mut m = Merger {
            src: self.src,
            toks: &inner,
            pos: 0,
            frames: Vec::new(),
        };
        let (content, _, stop) = m.loop_until_closer_at(false);
        let node = Node::Container {
            kind: ContainerKind::Quote,
            content,
        };
        match stop {
            Stop::Eof => (vec![node], None),
            Stop::Comment => (vec![node], Some(Stop::Comment)),
            Stop::Closer { closer, .. } | Stop::Stray(closer) => {
                (vec![node], Some(Stop::Stray(closer)))
            }
        }
    }

    // ── line blocks ───────────────────────────────────────────────────────

    /// The rest of the line and its newline (the old `content_before(line_end)`
    /// plus `line_end()`); a stop from a nested element rides along.
    fn line_body(&mut self) -> (Content, Option<Stop<'src>>) {
        let (content, reason) = self.body_until(|t| matches!(t, Tok::Newline));
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        (content, reason)
    }

    /// One or more `*` / `#` lines folded into a nested [`List`] (the old
    /// `list_block` + `build_list`).
    fn list_block(&mut self) -> Node {
        let mut lines: Vec<(usize, bool, Content)> = Vec::new();
        while let Some(Tok::ListMark { ordered, indent }) = self.peek().cloned() {
            self.pos += 1;
            let (content, _) = self.line_body();
            lines.push((indent, ordered, content));
        }
        Node::List(build_list(&lines))
    }

    /// One or more `||`-prefixed lines (the old `table_block`).
    fn table_block(&mut self) -> Node {
        let mut rows: Vec<Vec<TableCell>> = Vec::new();
        while self.at_line_start() && self.peek_is(|t| matches!(t, Tok::Pipe2)) {
            self.pos += 1; // the row's opening `||`
            let mut row: Vec<TableCell> = Vec::new();
            loop {
                let header = self.peek_is(|t| matches!(t, Tok::Tilde));
                if header {
                    self.pos += 1;
                }
                let align = if let Some(Tok::CellAlign(side)) = self.peek().cloned() {
                    self.pos += 1;
                    Some(Align {
                        floating: false,
                        side,
                    })
                } else {
                    None
                };
                let (content, _) = self.body_until(|t| matches!(t, Tok::Pipe2 | Tok::Newline));
                row.push(TableCell {
                    colspan: 1,
                    header,
                    align,
                    content,
                });
                match self.peek() {
                    Some(Tok::Pipe2) => {
                        self.pos += 1;
                        if self.peek_is(|t| matches!(t, Tok::Newline)) {
                            self.pos += 1;
                            break;
                        }
                        if self.peek().is_none() {
                            break;
                        }
                    }
                    Some(Tok::Newline) => {
                        self.pos += 1;
                        break;
                    }
                    _ => break,
                }
            }
            rows.push(row);
        }
        Node::Table(rows)
    }

    // ── bracket constructs ────────────────────────────────────────────────

    fn raw_flatten(
        &self,
        opener: (usize, usize),
        body: Content,
        stop: Stop<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(Node::Raw(self.src[opener.0..opener.1].to_string()));
        out.extend(body);
        (out, Some(stop))
    }

    /// A `[[` opener: a plain wrapping tag pushes its frame; the rest hand
    /// off to their region arms.
    fn open_bracket(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let Token { tok, start, end } = &self.toks[self.pos];
        let Tok::Open(tag) = tok else { unreachable!() };
        let opener = (*start, *end);
        self.pos += 1;
        if let Some(key) = closed_tag_of(tag) {
            self.frames.push(Frame {
                key: Closer::Tag(key),
                opener,
                kind: FrameKind::Tag(tag.clone()),
                children: Content::new(),
                restarted: false,
            });
            return (Content::new(), None);
        }
        let arm: TagArm<'src> = match tag {
            OpenTag::Collapsible { .. } => Self::collapsible_arm,
            OpenTag::User { .. } => Self::user_arm,
            OpenTag::Footnoteblock => Self::footnoteblock_arm,
            OpenTag::Code { .. } => Self::code_arm,
            OpenTag::Css => Self::css_arm,
            OpenTag::ListPages { .. } => Self::listpages_arm,
            OpenTag::Module { .. } => Self::module_arm,
            OpenTag::Include { .. } => Self::include_arm,
            OpenTag::Image { .. } => Self::image_arm,
            // Only meaningful as a tabview child / listpages marker / table
            // child; stray ones degrade to text like the old char fallback.
            OpenTag::Tab { .. } | OpenTag::Section(_) | OpenTag::Row { .. } => Self::stray_arm,
            OpenTag::Table { .. } => Self::table_arm,
            OpenTag::Tabview => Self::tabview_arm,
            _ => unreachable!("frame tags pushed above"),
        };
        arm(self, opener, tag.clone())
    }

    /// `[[collapsible …]]` is not an interval: the closer may sit arbitrarily
    /// deeper than the opener. The opener plants a [`Node::CollapsibleHeader`]
    /// leaf right here, riding the tree through whatever containers close
    /// around it; the closer (wherever it arrives) pairs it via
    /// [`Merger::collapsible_step`].
    fn collapsible_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::Collapsible { params } = tag else {
            unreachable!()
        };
        let open = attr_value_raw(&params, "show").unwrap_or_else(|| "+ show block".into());
        let close = attr_value_raw(&params, "hide").unwrap_or_else(|| "- hide block".into());
        let folded = !matches!(
            attr_value(&params, "folded").as_deref(),
            Some("no") | Some("false")
        );
        (
            vec![Node::CollapsibleHeader {
                folded,
                open,
                close,
                raw: self.src[opener.0..opener.1].to_string(),
            }],
            None,
        )
    }

    fn user_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::User { avatar, name } = tag else {
            unreachable!()
        };
        (
            vec![Node::User {
                name: name.to_string(),
                avatar,
            }],
            None,
        )
    }

    fn footnoteblock_arm(
        &mut self,
        _opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        (vec![Node::FootnoteBlock(Vec::new())], None)
    }

    /// The old `raw_balanced`: the body is sliced verbatim from the source
    /// between the opener and the stop point; the parsed nodes matter only on
    /// the flatten path.
    fn raw_body(
        &mut self,
        opener: (usize, usize),
        closer: ClosedTag,
        build: impl Fn(&str) -> Node,
    ) -> (Content, Option<Stop<'src>>) {
        let (body, body_end, stop) = self.loop_until_closer_at(false);
        let mine = matches!(&stop, Stop::Stray(Closer::Tag(t)) if *t == closer);
        if mine {
            (vec![build(&self.src[opener.1..body_end])], None)
        } else {
            self.raw_flatten(opener, body, stop)
        }
    }

    fn table_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::Table { params } = tag else {
            unreachable!()
        };
        self.block_table(opener, params)
    }

    fn tabview_arm(
        &mut self,
        opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        self.tabview(opener)
    }

    fn code_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let ty = match tag {
            OpenTag::Code { params } => code_type(&params),
            _ => None,
        };
        self.raw_body(opener, ClosedTag::Code, move |body| Node::Code {
            ty: ty.clone(),
            raw: body.to_string(),
        })
    }

    fn css_arm(
        &mut self,
        opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        self.raw_body(opener, ClosedTag::Module, |body| {
            Node::Stylesheet(wikidot_verbatim(body))
        })
    }

    fn listpages_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::ListPages { params } = tag else {
            unreachable!()
        };
        self.listpages(opener, params)
    }

    fn module_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::Module { name, params } = tag else {
            unreachable!()
        };
        (vec![Node::Module { name, params }], None)
    }

    fn include_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::Include { raw } = tag else {
            unreachable!()
        };
        let (source, vars) = parse_include_args(raw);
        (vec![Node::Include(Include { source, vars })], None)
    }

    fn image_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        let OpenTag::Image {
            align,
            source,
            params,
        } = tag
        else {
            unreachable!()
        };
        (
            vec![Node::Image {
                align,
                source,
                params,
            }],
            None,
        )
    }

    fn stray_arm(
        &mut self,
        _opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop<'src>>) {
        self.pos -= 1;
        (vec![self.degrade()], None)
    }

    /// The old `module_block` ListPages branch: the body's four slots, closed
    /// only by a `[[/module]]`.
    fn listpages(
        &mut self,
        opener: (usize, usize),
        params: Params,
    ) -> (Content, Option<Stop<'src>>) {
        let depth = self.frames.len();
        let mut head = Content::new();
        let mut body = Content::new();
        let mut foot = Content::new();
        let mut main = Content::new();
        let mut slot = None;
        let mut body_section = false;
        let mut stop = Stop::Eof;
        loop {
            match self.peek().cloned() {
                None => break,
                Some(Tok::Close(tag)) => {
                    self.pos += 1;
                    if let Some(s) = self.closer_step(Closer::Tag(tag), depth, &mut main) {
                        stop = s;
                        break;
                    }
                }
                Some(Tok::Open(OpenTag::Section(Some(s)))) => {
                    self.pos += 1;
                    body_section |= s == SectionSlot::Body;
                    slot = Some(s);
                }
                Some(Tok::Open(OpenTag::Section(None))) => {
                    self.pos += 1;
                    slot = None;
                }
                _ => {
                    let sink = match slot {
                        Some(SectionSlot::Head) => &mut head,
                        Some(SectionSlot::Body) => &mut body,
                        Some(SectionSlot::Foot) => &mut foot,
                        None => &mut main,
                    };
                    if let Some(s) = self.element_step(depth, sink) {
                        stop = s;
                        break;
                    }
                }
            }
        }
        {
            let sink = match slot {
                Some(SectionSlot::Head) => &mut head,
                Some(SectionSlot::Body) => &mut body,
                Some(SectionSlot::Foot) => &mut foot,
                None => &mut main,
            };
            self.drain(depth, sink);
        }
        let mut lp = ListPages {
            params: listpages_params(&params),
            prepend: head,
            repeat: if body_section { body } else { main },
            append: foot,
        };
        if matches!(stop, Stop::Stray(Closer::Tag(ClosedTag::Module))) {
            if lp.prepend.is_empty()
                && let Some(line) = attr_value(&params, "prependline")
            {
                lp.prepend = parse_sub(&line);
            }
            if lp.append.is_empty()
                && let Some(line) = attr_value(&params, "appendline")
            {
                lp.append = parse_sub(&line);
            }
            (vec![Node::ListPages(lp)], None)
        } else {
            let mut out =
                Vec::with_capacity(lp.prepend.len() + lp.repeat.len() + lp.append.len() + 1);
            out.push(Node::Raw(self.src[opener.0..opener.1].to_string()));
            out.extend(lp.prepend);
            out.extend(lp.repeat);
            out.extend(lp.append);
            (out, Some(stop))
        }
    }

    /// The old `container_balanced` for `[[table]]`: `[[row]]` children, stray
    /// content between them discarded on a successful match.
    fn block_table(
        &mut self,
        opener: (usize, usize),
        params: Params,
    ) -> (Content, Option<Stop<'src>>) {
        let depth = self.frames.len();
        let mut rows: Vec<BlockRow> = Vec::new();
        let mut stray = Content::new();
        let mut stop = Stop::Eof;
        loop {
            self.skip_container_ws();
            match self.peek().cloned() {
                None => break,
                Some(Tok::Close(tag)) => {
                    self.pos += 1;
                    if let Some(s) = self.closer_step(Closer::Tag(tag), depth, &mut stray) {
                        stop = s;
                        break;
                    }
                }
                Some(Tok::Open(OpenTag::Row { params: row_params })) => {
                    self.pos += 1;
                    // A row ends at any closer, consumed and dropped.
                    let (content, _, _) = self.loop_until_closer_at(false);
                    rows.push(BlockRow {
                        params: row_params,
                        content,
                    });
                }
                _ => {
                    if let Some(s) = self.element_step(depth, &mut stray) {
                        stop = s;
                        break;
                    }
                }
            }
        }
        self.drain(depth, &mut stray);
        if matches!(stop, Stop::Stray(Closer::Tag(ClosedTag::Table))) {
            (vec![Node::BlockTable(BlockTable { params, rows })], None)
        } else {
            let mut out = Vec::with_capacity(rows.len() + stray.len() + 1);
            out.push(Node::Raw(self.src[opener.0..opener.1].to_string()));
            for r in rows {
                out.extend(r.content);
            }
            out.extend(stray);
            (out, Some(stop))
        }
    }

    /// The old `container_balanced` for `[[tabview]]`: `[[tab]]` children.
    fn tabview(&mut self, opener: (usize, usize)) -> (Content, Option<Stop<'src>>) {
        let depth = self.frames.len();
        let mut tabs: Vec<Tab> = Vec::new();
        let mut stray = Content::new();
        let mut stop = Stop::Eof;
        loop {
            self.skip_container_ws();
            match self.peek().cloned() {
                None => break,
                Some(Tok::Close(tag)) => {
                    self.pos += 1;
                    if let Some(s) = self.closer_step(Closer::Tag(tag), depth, &mut stray) {
                        stop = s;
                        break;
                    }
                }
                Some(Tok::Open(OpenTag::Tab { name })) => {
                    self.pos += 1;
                    let name = parse_sub(name);
                    // A tab ends at any closer, consumed and dropped.
                    let (content, _, _) = self.loop_until_closer_at(false);
                    tabs.push(Tab { name, content });
                }
                _ => {
                    if let Some(s) = self.element_step(depth, &mut stray) {
                        stop = s;
                        break;
                    }
                }
            }
        }
        self.drain(depth, &mut stray);
        if matches!(stop, Stop::Stray(Closer::Tag(ClosedTag::Tabview))) {
            (vec![Node::Tabview { id: 0, tabs }], None)
        } else {
            let mut out = Vec::with_capacity(tabs.len() + stray.len() + 1);
            out.push(Node::Raw(self.src[opener.0..opener.1].to_string()));
            for t in tabs {
                out.extend(t.name);
                out.extend(t.content);
            }
            out.extend(stray);
            (out, Some(stop))
        }
    }

    // ── comment ───────────────────────────────────────────────────────────

    /// `[!-- … --]`: the body is parsed so nested markup degrades gracefully;
    /// on a successful close everything is discarded.
    fn comment(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop<'src>>) {
        let opener = (self.toks[self.pos].start, self.toks[self.pos].end);
        self.pos += 1;
        let (body, _, stop) = self.loop_until_closer_at(true);
        if matches!(stop, Stop::Comment) {
            (Content::new(), None)
        } else {
            self.raw_flatten(opener, body, stop)
        }
    }
}

// ── collapsible pairing ─────────────────────────────────────────────────

/// Pair a `[[/collapsible]]` closer with the header leaf planted earlier
/// in `nodes` (the latest unpaired one in document order; see
/// [`Merger::collapsible_arm`]). The node holding the leaf — inline
/// wrappers around it included — becomes the collapsible's `header`
/// wholesale; everything after it becomes the `body`.
///
/// `false` when no opener is pending at this level: the caller seals the
/// frames it crosses (they survive, without re-opening) and lets the closer
/// travel as a stray.
fn close_collapsible(nodes: &mut Content) -> bool {
    let Some(idx) = nodes.iter().rposition(node_has_pending) else {
        return false;
    };
    let header = vec![nodes.remove(idx)];
    let body = nodes.split_off(idx);
    nodes.push(Node::Collapsible { header, body });
    true
}

fn node_has_pending(node: &Node) -> bool {
    match node {
        Node::CollapsibleHeader { .. } => true,
        // Paired: its header leaf is off the market.
        Node::Collapsible { .. } => false,
        node => {
            let mut found = false;
            node.visit_node(&mut |children| found |= children.iter().any(node_has_pending));
            found
        }
    }
}

/// Fold flat indented list lines into a nested [`List`]; lines at the minimum
/// indent are top-level items, each followed by its deeper-indented run (which
/// becomes the item's sublist).
fn build_list(lines: &[(usize, bool, Content)]) -> List {
    let root_indent = lines[0].0;
    let mut items: Vec<ListItem> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (_, _, content) = &lines[i];
        let mut child: Vec<(usize, bool, Content)> = Vec::new();
        let mut j = i + 1;
        while j < lines.len() && lines[j].0 > root_indent {
            child.push(lines[j].clone());
            j += 1;
        }
        let sublist = (!child.is_empty()).then(|| Box::new(build_list(&child)));
        items.push(ListItem {
            content: content.clone(),
            sublist,
        });
        i = j;
    }
    List {
        ordered: lines[0].1,
        items,
    }
}
