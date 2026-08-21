//! The tree builder: a single fold over the token stream guided by the
//! pairer's interval facts. It owns no pairing logic of its own — every
//! "which closer owns which opener" decision comes from [`pair`] — and
//! instead enforces exactly one structural rule, Wikidot's own rendering
//! invariant:
//!
//! **Block frames sit at the bottom of the stack, inline frames on top.**
//!
//! A block opener force-closes the inline frames above it (their sealed
//! halves land before the block, fresh copies re-open inside it) — which is
//! why `[[size]] h1 [[div]] h2 [[/div]] h3 [[/size]]` renders as three
//! spans with the div cutting the middle one, and why a bold crossing `||`
//! splits into per-cell bolds. A closer force-closes everything above its
//! frame the same way, so crossing intervals split instead of nesting.
//!
//! Openers the pairer left unpaired and closers with no owner render as
//! [`Node::Raw`]: the input stays visible, nothing is silently dropped.
//! Line constructs (headings, list items, cells, colours, `^^`) always have
//! a fact — the pairer closes them at their line's end or EOF — so only
//! bracket constructs and marks can degrade this way.

use super::lexer::{OpenTag, SectionSlot, Tok, Token, lex};
use super::pairer::{Event, Pairing, pair};
use super::*;

/// Which verbatim region a frame holds.
#[derive(Clone, Copy)]
enum Verb {
    Code,
    Css,
    Comment,
}

#[derive(Clone)]
enum FrameKind<'src> {
    /// A bracket construct; the node comes from [`build_tag_node`] or one
    /// of the structural closers (tab, row, table, tabview, listpages,
    /// collapsible).
    Tag(OpenTag<'src>),
    Mark(TextStyle),
    Color(&'src str),
    Sup(bool),
    Verbatim(Verb),
    Heading(u32),
    Center,
    Quote,
    ListLine {
        ordered: bool,
        indent: usize,
    },
    Cell {
        header: bool,
        align: Option<Align>,
    },
}

/// Per-frame payload for the constructs that gather more than a body.
enum Extra {
    Tabs(Vec<Tab>),
    Rows(Vec<BlockRow>),
    Lp(Box<LpState>),
}

/// A listpages body's four slots; `[[section]]` markers switch the sink.
struct LpState {
    head: Content,
    body: Content,
    foot: Content,
    main: Content,
    section: Option<SectionSlot>,
    in_body: bool,
}

struct Frame<'src> {
    /// The opener's token index — the identity its pairing fact refers to.
    open: usize,
    /// The event's close boundary; may equal the token count (a virtual
    /// EOF closer for line constructs).
    close: usize,
    kind: FrameKind<'src>,
    content: Content,
    extra: Option<Extra>,
}

/// Inline frames ride on top of block ones; every block boundary — opener
/// or closer — splits them, per the stack invariant above.
fn is_inline(kind: &FrameKind) -> bool {
    match kind {
        FrameKind::Tag(
            OpenTag::Span { .. } | OpenTag::Anchor { .. } | OpenTag::Size(_) | OpenTag::Footnote,
        )
        | FrameKind::Mark(_)
        | FrameKind::Color(_)
        | FrameKind::Sup(_) => true,
        _ => false,
    }
}

/// Which pending group an opener continues (`*` continues a list, `||`
/// continues a table) and must not flush.
#[derive(Clone, Copy, PartialEq)]
enum Keep {
    None,
    List,
    Table,
}

/// The node a closed tag frame builds from its opener and body.
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

/// Fold flat list lines into a nested [`List`].
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

fn is_ws_only(c: &Content) -> bool {
    c.iter().all(|n| match n {
        Node::Text(TextObj::Plain(s)) => s.trim().is_empty(),
        _ => false,
    })
}

/// Whether a body's first or last text child has a whitespace edge — the
/// Text_Wiki strikethrough rim rule.
fn rim_ws(children: &Content) -> bool {
    matches!(children.first(), Some(Node::Text(TextObj::Plain(s))) if s.starts_with(char::is_whitespace))
        || matches!(children.last(), Some(Node::Text(TextObj::Plain(s))) if s.ends_with(char::is_whitespace))
}

fn txt(s: &str) -> Node {
    Node::Text(TextObj::Plain(s.to_string()))
}

fn collapsible_header(params: &Params, raw: String) -> Node {
    let open = attr_value_raw(params, "show").unwrap_or_else(|| "+ show block".into());
    let close = attr_value_raw(params, "hide").unwrap_or_else(|| "- hide block".into());
    let folded = !matches!(
        attr_value(params, "folded").as_deref(),
        Some("no") | Some("false")
    );
    Node::CollapsibleHeader {
        folded,
        open,
        close,
        raw,
    }
}

// ── collapsible pairing ─────────────────────────────────────────────────

/// Pair a `[[/collapsible]]` closer with the header leaf planted earlier
/// in `nodes`: the node holding the leaf — inline wrappers around it
/// included — becomes the collapsible's `header` wholesale; everything
/// after it becomes the `body`. `false` when no leaf is reachable: the
/// caller degrades to a default-header collapsible over an empty body.
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
        Node::Collapsible { .. } => false,
        node => {
            let mut found = false;
            node.visit_node(&mut |children| found |= children.iter().any(node_has_pending));
            found
        }
    }
}

// ── entry points ─────────────────────────────────────────────────────────

/// Parse a token slice as a standalone document: pair, fold, then the
/// shared post-processing. Total: any input parses to something.
pub(crate) fn parse_toks(src: &str, toks: &[Token]) -> Content {
    let pr = pair(src, toks);
    let mut b = Builder {
        src,
        toks,
        pr: &pr,
        frames: Vec::new(),
        out: Vec::new(),
        list: None,
        row: Vec::new(),
        table: None,
        skip: false,
    };
    b.run();
    let mut content = b.out;
    degrade_unclosed_collapsibles(&mut content);
    merge_text(content)
}

/// Recursively parse a raw markup fragment (include values, variable
/// defaults, link/tab text): lex it and build it as a document.
pub(crate) fn parse_sub(src: &str) -> Content {
    let toks = lex(src);
    parse_toks(src, &toks)
}

struct Builder<'src, 'p> {
    src: &'src str,
    toks: &'src [Token<'src>],
    pr: &'p Pairing,
    /// Open frames, outermost first. Invariant: no block frame ever sits
    /// above an inline one.
    frames: Vec<Frame<'src>>,
    out: Content,
    /// Consecutive list lines awaiting their `List` node, with the stack
    /// level they belong to.
    list: Option<(usize, Vec<(usize, bool, Content)>)>,
    /// The `||` row under construction.
    row: Vec<TableCell>,
    /// Finished rows awaiting their `Table` node.
    table: Option<(usize, Vec<Vec<TableCell>>)>,
    /// Swallow the next token (the newline a rule or clearfloat eats).
    skip: bool,
}

impl<'src> Builder<'src, '_> {
    fn run(&mut self) {
        let n = self.toks.len();
        let mut i = 0;
        while i <= n {
            while let Some(j) = self.frames.iter().rposition(|f| f.close <= i) {
                self.close_frame_at(j);
            }
            if i == n {
                break;
            }
            if self.skip {
                self.skip = false;
                i += 1;
                continue;
            }
            if self.pr.open_of[i].is_some() {
                self.open_frame(i);
            } else if self.pr.close_of[i].is_none() {
                self.leaf(i);
            }
            i += 1;
        }
        self.row_flush();
        let due = self.take_due(0, Keep::None);
        self.out.extend(due);
    }

    // ── frames ───────────────────────────────────────────────────────────

    fn kind_of(&self, i: usize) -> FrameKind<'src> {
        match &self.toks[i].tok {
            Tok::Heading(level) => FrameKind::Heading(*level),
            Tok::CenterEq => FrameKind::Center,
            Tok::QuoteMark => FrameKind::Quote,
            Tok::ListMark { ordered, indent } => FrameKind::ListLine {
                ordered: *ordered,
                indent: *indent,
            },
            // `||~ header ||= left` — the decorations follow the pipe.
            Tok::Pipe2 => {
                let mut j = i + 1;
                let header = matches!(self.toks.get(j).map(|t| &t.tok), Some(Tok::Tilde));
                if header {
                    j += 1;
                }
                let align = match self.toks.get(j).map(|t| &t.tok) {
                    Some(Tok::CellAlign(side)) => Some(Align {
                        floating: false,
                        side: *side,
                    }),
                    _ => None,
                };
                FrameKind::Cell { header, align }
            }
            Tok::Mark(style) => FrameKind::Mark(*style),
            Tok::ColorOpen(spec) => FrameKind::Color(spec),
            Tok::SupMark => FrameKind::Sup(true),
            Tok::SubMark => FrameKind::Sup(false),
            Tok::CommentOpen => FrameKind::Verbatim(Verb::Comment),
            Tok::Open(OpenTag::Code) => FrameKind::Verbatim(Verb::Code),
            Tok::Open(OpenTag::Css) => FrameKind::Verbatim(Verb::Css),
            Tok::Open(tag) => FrameKind::Tag(tag.clone()),
            _ => unreachable!("open_of only marks pairable openers"),
        }
    }

    fn extra_of(kind: &FrameKind<'src>) -> Option<Extra> {
        match kind {
            FrameKind::Tag(OpenTag::Tabview) => Some(Extra::Tabs(Vec::new())),
            FrameKind::Tag(OpenTag::Table { .. }) => Some(Extra::Rows(Vec::new())),
            FrameKind::Tag(OpenTag::ListPages { .. }) => Some(Extra::Lp(Box::new(LpState {
                head: Vec::new(),
                body: Vec::new(),
                foot: Vec::new(),
                main: Vec::new(),
                section: None,
                in_body: false,
            }))),
            _ => None,
        }
    }

    fn event_close(&self, i: usize) -> usize {
        let ei = self.pr.open_of[i].expect("opener");
        match self.pr.events[ei] {
            Event::Pair { close, .. } | Event::Verbatim { close, .. } => close,
        }
    }

    fn raw_of(&self, i: usize) -> String {
        self.src[self.toks[i].start..self.toks[i].end].to_string()
    }

    fn open_frame(&mut self, i: usize) {
        let kind = self.kind_of(i);
        let close = self.event_close(i);
        // The stack invariant: a block opener splits the inline frames
        // above it — sealed halves land here, fresh halves re-open inside.
        // The collapsible is the exception: its opener plants an inline
        // header leaf right after itself (the lexer's split), so any
        // inline frames wrapping it must live on — the block boundary is
        // the closer's wrap.
        let mut restarts = Vec::new();
        if !is_inline(&kind) && !matches!(kind, FrameKind::Tag(OpenTag::Collapsible { .. })) {
            while self.frames.last().is_some_and(|f| is_inline(&f.kind)) {
                let f = self.frames.pop().expect("checked");
                if let Some(r) = self.pop_frame(f, true) {
                    restarts.push(r);
                }
            }
        }
        let keep = match &kind {
            FrameKind::ListLine { .. } => Keep::List,
            FrameKind::Cell { .. } => Keep::Table,
            _ => Keep::None,
        };
        self.land(Vec::new(), keep);
        let extra = Self::extra_of(&kind);
        self.frames.push(Frame {
            open: i,
            close,
            kind,
            content: Vec::new(),
            extra,
        });
        for r in restarts {
            self.frames.push(r);
        }
    }

    /// Close the due frame at `j` plus everything above it: each frame
    /// seals top-down (inner nodes landing in the frame below), and the
    /// frames above — which all outlive this boundary, since `close_due`
    /// picked the topmost due frame — re-open on top of what remains.
    fn close_frame_at(&mut self, j: usize) {
        let mut restarts = Vec::new();
        while self.frames.len() > j {
            let f_idx = self.frames.len() - 1;
            let f = self.frames.pop().expect("len > j");
            if let Some(r) = self.pop_frame(f, f_idx > j) {
                restarts.push(r);
            }
        }
        restarts.reverse();
        for r in restarts {
            self.frames.push(r);
        }
    }

    /// Seal one popped frame. `continuing` frames were cut by a boundary
    /// their interval outlives: they re-open as a fresh half. Line frames
    /// feed their pending group instead of landing a node; the dangling
    /// empty halves a cut leaves behind vanish (they wrapped nothing).
    fn pop_frame(&mut self, f: Frame<'src>, continuing: bool) -> Option<Frame<'src>> {
        let f_idx = self.frames.len();
        let Frame {
            open,
            close,
            kind,
            mut content,
            extra,
        } = f;
        let restart = continuing.then(|| Frame {
            open,
            close,
            kind: kind.clone(),
            content: Vec::new(),
            extra: Self::extra_of(&kind),
        });
        match kind {
            FrameKind::ListLine { ordered, indent } => {
                self.list
                    .get_or_insert_with(|| (f_idx, Vec::new()))
                    .1
                    .push((indent, ordered, content));
            }
            FrameKind::Cell { header, align } => {
                content.extend(self.take_due(f_idx + 1, Keep::None));
                self.row.push(TableCell {
                    colspan: 1,
                    header,
                    align,
                    content,
                });
                // A row ends where one of its cells dies on a newline.
                if matches!(
                    self.toks.get(close),
                    Some(Token {
                        tok: Tok::Newline,
                        ..
                    })
                ) {
                    self.row_flush();
                }
            }
            kind if is_inline(&kind) && content.is_empty() => {}
            FrameKind::Tag(OpenTag::Collapsible { params }) => {
                // TODO: is `continuing` realistically helping? Probably not, could be dropped.
                // Garbage-in-garbage-out.
                if !continuing {
                    let pend = self.take_due(f_idx, Keep::None);
                    self.sink().extend(pend);
                    if !close_collapsible(self.sink()) {
                        // No reachable header leaf — a nested closer claimed
                        // it or a wrapper sealed it away. Degrade to the
                        // opener's own header over an empty body instead of
                        // letting both tags vanish; the region's content
                        // already stands in the sink.
                        let header = vec![collapsible_header(&params, self.raw_of(open))];
                        self.sink().push(Node::Collapsible {
                            header,
                            body: Vec::new(),
                        });
                    }
                }
            }
            FrameKind::Tag(OpenTag::Tab { name }) => {
                content.extend(self.take_due(f_idx + 1, Keep::None));
                let tab = Tab {
                    name: parse_sub(name),
                    content,
                };
                match self.frames.iter_mut().rev().find_map(same_extra_tabs) {
                    Some(tabs) => tabs.push(tab),
                    None => {
                        let mut nodes = vec![Node::Raw(self.raw_of(open))];
                        nodes.extend(tab.content);
                        self.land(nodes, Keep::None);
                    }
                }
            }
            FrameKind::Tag(OpenTag::Row { params }) => {
                content.extend(self.take_due(f_idx + 1, Keep::None));
                let row = BlockRow { params, content };
                match self.frames.iter_mut().rev().find_map(same_extra_rows) {
                    Some(rows) => rows.push(row),
                    None => {
                        let mut nodes = vec![Node::Raw(self.raw_of(open))];
                        nodes.extend(row.content);
                        self.land(nodes, Keep::None);
                    }
                }
            }
            kind => {
                content.extend(self.take_due(f_idx + 1, Keep::None));
                let nodes = self.seal_nodes(kind, extra, open, close, content);
                self.land(nodes, Keep::None);
            }
        }
        restart
    }

    /// The node(s) a sealed frame builds from its body and extra payload.
    fn seal_nodes(
        &self,
        kind: FrameKind<'src>,
        extra: Option<Extra>,
        open: usize,
        close: usize,
        content: Content,
    ) -> Content {
        match kind {
            FrameKind::Tag(OpenTag::Table { params }) => {
                let rows = match extra {
                    Some(Extra::Rows(rows)) => rows,
                    _ => Vec::new(),
                };
                vec![Node::BlockTable(BlockTable { params, rows })]
            }
            FrameKind::Tag(OpenTag::Tabview) => {
                let tabs = match extra {
                    Some(Extra::Tabs(tabs)) => tabs,
                    _ => Vec::new(),
                };
                vec![Node::Tabview { id: 0, tabs }]
            }
            FrameKind::Tag(OpenTag::ListPages { params }) => {
                let lp = match extra {
                    Some(Extra::Lp(lp)) => *lp,
                    _ => unreachable!("listpages frames carry their slots"),
                };
                let mut prepend = lp.head;
                if prepend.is_empty()
                    && let Some(line) = attr_value(&params, "prependline")
                {
                    prepend = parse_sub(&line);
                }
                let mut append = lp.foot;
                if append.is_empty()
                    && let Some(line) = attr_value(&params, "appendline")
                {
                    append = parse_sub(&line);
                }
                vec![Node::ListPages(ListPages {
                    params: listpages_params(&params),
                    prepend,
                    repeat: if lp.in_body { lp.body } else { lp.main },
                    append,
                })]
            }
            FrameKind::Tag(
                OpenTag::Collapsible { .. } | OpenTag::Tab { .. } | OpenTag::Row { .. },
            ) => unreachable!("structural closers"),
            FrameKind::Tag(tag) => vec![build_tag_node(tag, content)],
            FrameKind::Mark(TextStyle::Strikethrough) if rim_ws(&content) => {
                let mut out = vec![txt("—")];
                out.extend(content);
                out.push(txt("—"));
                out
            }
            FrameKind::Mark(style) => vec![Node::Container {
                kind: ContainerKind::Style(style),
                content,
            }],
            FrameKind::Color(spec) => vec![Node::Container {
                kind: ContainerKind::Color(normalize_color(spec.to_string())),
                content,
            }],
            FrameKind::Sup(true) => vec![Node::SupSubscript {
                sup: content,
                sub: Vec::new(),
            }],
            FrameKind::Sup(false) => vec![Node::SupSubscript {
                sup: Vec::new(),
                sub: content,
            }],
            FrameKind::Heading(level) => vec![Node::Heading {
                level,
                anchor: None,
                content,
            }],
            FrameKind::Center => vec![Node::Container {
                kind: ContainerKind::Align(Align {
                    floating: false,
                    side: AlignSide::Center,
                }),
                content,
            }],
            FrameKind::Quote => vec![Node::Container {
                kind: ContainerKind::Quote,
                content,
            }],
            FrameKind::Verbatim(verb) => {
                let body = self.src[self.toks[open].end..self.toks[close].start].to_string();
                match verb {
                    Verb::Code => vec![Node::Code(body.trim().to_string())],
                    Verb::Css => vec![Node::Stylesheet(body.trim().to_string())],
                    // A comment discards everything it spanned.
                    Verb::Comment => Vec::new(),
                }
            }
            FrameKind::ListLine { .. } | FrameKind::Cell { .. } => unreachable!("fed pendings"),
        }
    }

    // ── pendings ─────────────────────────────────────────────────────────

    /// The pending groups whose home level is at `d` or deeper: everything
    /// between has closed, they must land now. `keep` spares the group the
    /// next opener continues.
    fn take_due(&mut self, d: usize, keep: Keep) -> Content {
        let mut out = Vec::new();
        if keep != Keep::List
            && let Some((l, lines)) = self.list.take()
        {
            if l >= d && !lines.is_empty() {
                out.push(Node::List(build_list(&lines)));
            } else {
                self.list = Some((l, lines));
            }
        }
        if keep != Keep::Table
            && let Some((l, rows)) = self.table.take()
        {
            if l >= d && !rows.is_empty() {
                out.push(Node::Table(rows));
            } else {
                self.table = Some((l, rows));
            }
        }
        out
    }

    fn row_flush(&mut self) {
        if self.row.last().is_some_and(|c| is_ws_only(&c.content)) {
            self.row.pop();
        }
        let Some(row) = (!self.row.is_empty()).then(|| std::mem::take(&mut self.row)) else {
            return;
        };
        let d = self.frames.len();
        self.table
            .get_or_insert_with(|| (d, Vec::new()))
            .1
            .push(row);
    }

    // ── sinks ────────────────────────────────────────────────────────────

    /// Append nodes to the current sink, flushing whatever pending groups
    /// are due there first.
    fn land(&mut self, mut nodes: Content, keep: Keep) {
        let d = self.frames.len();
        let mut all = self.take_due(d, keep);
        all.append(&mut nodes);
        self.sink().extend(all);
    }

    /// The content the next node lands in: the topmost non-transparent
    /// frame's body — for a listpages frame, its active section slot.
    fn sink(&mut self) -> &mut Content {
        let Some(f) = self
            .frames
            .iter_mut()
            .rev()
            .find(|f| !matches!(f.kind, FrameKind::Tag(OpenTag::Collapsible { .. })))
        else {
            return &mut self.out;
        };
        match &mut f.extra {
            Some(Extra::Lp(lp)) => match lp.section {
                Some(SectionSlot::Head) => &mut lp.head,
                Some(SectionSlot::Body) => &mut lp.body,
                Some(SectionSlot::Foot) => &mut lp.foot,
                _ => &mut lp.main,
            },
            _ => &mut f.content,
        }
    }

    // ── leaves ───────────────────────────────────────────────────────────

    fn leaf(&mut self, i: usize) {
        if self
            .frames
            .last()
            .is_some_and(|f| matches!(f.kind, FrameKind::Verbatim(_)))
        {
            return;
        }
        let t = &self.toks[i];
        let nodes: Content = match &t.tok {
            Tok::Text(s) => vec![txt(&typography(s))],
            Tok::Newline => vec![txt("\n")],
            Tok::Rule => {
                self.skip_bare_newline(i);
                vec![Node::HorizontalRule]
            }
            Tok::Clearfloat(side) => {
                self.skip_bare_newline(i);
                vec![Node::Clearfloat(*side)]
            }
            // Stripped line scaffolding: their meaning lives in the
            // interval facts, the tokens themselves render as nothing.
            Tok::QuoteMark | Tok::Tilde | Tok::CellAlign(_) => return,
            Tok::CollapsibleHdr(params) => vec![collapsible_header(params, self.raw_of(i))],
            Tok::Tt(body) => vec![Node::Container {
                kind: ContainerKind::Tt,
                content: parse_sub(body),
            }],
            Tok::Escape(body) => vec![txt(body)],
            Tok::Url(u) => vec![Node::Link {
                target: LinkTarget::Url(u.to_string()),
                text: vec![txt(u)],
                class: None,
            }],
            Tok::AnchorTarget(name) => vec![Node::AnchorTarget(name.to_string())],
            Tok::IfExpr { cond, then, els } => vec![Node::IfExpr {
                cond: text_objs_of(cond),
                then: parse_sub(then),
                els: els.map(parse_sub).unwrap_or_default(),
            }],
            Tok::ModuleVar { name, default } => vec![Node::Text(TextObj::ModuleVar {
                name: name.to_string(),
                default: default.map(str::to_string),
            })],
            Tok::IncludeVar { name, default } => vec![Node::Text(TextObj::IncludeVar {
                name: name.to_string(),
                default: default.map(parse_sub),
            })],
            Tok::Link3 { target, text } => {
                let objs = text_objs_of(target.trim());
                vec![Node::Link {
                    target: parse_link_target_objs(&objs),
                    text: match text {
                        Some(t) => parse_sub(t),
                        None => objs.into_iter().map(Node::Text).collect(),
                    },
                    class: None,
                }]
            }
            Tok::Link1 { target, text } => {
                let objs = text_objs_of(target.trim());
                vec![Node::Link {
                    target: parse_link_target_objs(&objs),
                    text: match text {
                        Some(t) => vec![txt(t.trim())],
                        None => objs.into_iter().map(Node::Text).collect(),
                    },
                    class: None,
                }]
            }
            Tok::Open(tag) => self.open_leaf(tag, i),
            Tok::Close(tag) => vec![Node::Raw(format!("[[/{}]]", tag.opener_str()))],
            Tok::CommentOpen | Tok::CommentClose => vec![Node::Raw(self.raw_of(i))],
            Tok::ColorClose => vec![txt("##")],
            Tok::ColorOpen(_) => vec![txt(&self.raw_of(i))],
            Tok::Mark(style) => self.mark_leaf(i, *style),
            Tok::SupMark | Tok::SubMark => vec![Node::Raw(self.raw_of(i))],
            Tok::Heading(_) | Tok::CenterEq | Tok::ListMark { .. } | Tok::Pipe2 => {
                unreachable!("openers only")
            }
        };
        self.land(nodes, Keep::None);
    }

    /// A mark with no pairing fact: `-- ` before text is an em-dash that
    /// swallows the space, any other spaced mark is plain text, and an
    /// unpaired opener stays visible as raw input.
    fn mark_leaf(&mut self, i: usize, style: TextStyle) -> Content {
        let t = &self.toks[i];
        let spaced = self.src[t.end..].starts_with(' ');
        if !spaced {
            return vec![Node::Raw(self.raw_of(i))];
        }
        if style != TextStyle::Strikethrough {
            return vec![txt(&self.src[t.start..t.end])];
        }
        // The em-dash consumed `-- ` including the space.
        let mut nodes = vec![txt("— ")];
        if let Some(Token {
            tok: Tok::Text(rest),
            ..
        }) = self.toks.get(i + 1)
            && let Some(rest) = rest.strip_prefix(' ')
        {
            self.skip = true;
            if !rest.is_empty() {
                nodes.push(txt(&typography(rest)));
            }
        }
        nodes
    }

    fn open_leaf(&mut self, tag: &OpenTag<'src>, i: usize) -> Content {
        match tag {
            OpenTag::User { avatar, name } => vec![Node::User {
                name: name.to_string(),
                avatar: *avatar,
            }],
            OpenTag::Footnoteblock => vec![Node::FootnoteBlock(Vec::new())],
            OpenTag::Module { name, params } => vec![Node::Module {
                name: name.clone(),
                params: params.clone(),
            }],
            OpenTag::Include { raw } => {
                let (source, vars) = parse_include_args(raw);
                vec![Node::Include(Include { source, vars })]
            }
            OpenTag::Image {
                align,
                source,
                params,
            } => vec![Node::Image {
                align: *align,
                source: source.clone(),
                params: params.clone(),
            }],
            // Unclosed: the header leaf token right after carries the
            // toggle link with the opener's raw source; the body parses
            // on at this level.
            OpenTag::Collapsible { .. } => Vec::new(),
            OpenTag::Section(slot) => {
                let Some(f) = self
                    .frames
                    .iter_mut()
                    .rev()
                    .find(|f| matches!(f.extra, Some(Extra::Lp(_))))
                else {
                    return vec![Node::Raw(self.raw_of(i))];
                };
                let Some(Extra::Lp(lp)) = &mut f.extra else {
                    unreachable!("just found");
                };
                lp.section = *slot;
                lp.in_body |= matches!(*slot, Some(SectionSlot::Body));
                Vec::new()
            }
            _ => vec![Node::Raw(self.raw_of(i))],
        }
    }

    fn skip_bare_newline(&mut self, i: usize) {
        if matches!(
            self.toks.get(i + 1),
            Some(Token {
                tok: Tok::Newline,
                ..
            })
        ) {
            self.skip = true;
        }
    }
}

fn same_extra_tabs<'a>(f: &'a mut Frame<'_>) -> Option<&'a mut Vec<Tab>> {
    match &mut f.extra {
        Some(Extra::Tabs(tabs)) => Some(tabs),
        _ => None,
    }
}

fn same_extra_rows<'a>(f: &'a mut Frame<'_>) -> Option<&'a mut Vec<BlockRow>> {
    match &mut f.extra {
        Some(Extra::Rows(rows)) => Some(rows),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Content {
        parse_sub(src)
    }

    fn container(kind: ContainerKind, content: Content) -> Node {
        Node::Container { kind, content }
    }

    fn div(content: Content) -> Node {
        container(
            ContainerKind::Div {
                inline: false,
                block: true,
                params: Params::new(),
            },
            content,
        )
    }

    fn span(content: Content) -> Node {
        container(
            ContainerKind::Div {
                inline: true,
                block: false,
                params: Params::new(),
            },
            content,
        )
    }

    fn style(style: TextStyle, content: Content) -> Node {
        container(ContainerKind::Style(style), content)
    }

    fn size(arg: &str, content: Content) -> Node {
        container(ContainerKind::Size(arg.to_string()), content)
    }

    fn cell(content: Content) -> TableCell {
        TableCell {
            colspan: 1,
            header: false,
            align: None,
            content,
        }
    }

    /// The user's reference case: a block opener splits the inline frames
    /// above it and re-opens them inside.
    #[test]
    fn block_opener_splits_inline() {
        assert_eq!(
            parse("[[size 120%]]h1[[div]]h2[[/div]]h3[[/size]]"),
            vec![
                size("120%", vec![txt("h1")]),
                div(vec![size("120%", vec![txt("h2")])]),
                size("120%", vec![txt("h3")]),
            ]
        );
    }

    /// The user's table case: an inline interval crossing a cell boundary
    /// splits at it, re-opening inside the next cell.
    #[test]
    fn bold_splits_across_cells() {
        assert_eq!(
            parse("|| **hello || world!** ||"),
            vec![Node::Table(vec![vec![
                cell(vec![txt(" "), style(TextStyle::Bold, vec![txt("hello ")])]),
                cell(vec![style(TextStyle::Bold, vec![txt(" world!")]), txt(" ")]),
            ]])]
        );
    }

    /// The span crossing whole table rows re-opens in every cell it
    /// reaches — Wikidot's own rendering of the user's three-row sample.
    #[test]
    fn span_splits_across_rows() {
        assert_eq!(
            parse("|| [[span]]hi || xxx ||\n|| xxx || yep [[/span]] ||"),
            vec![Node::Table(vec![
                vec![
                    cell(vec![txt(" "), span(vec![txt("hi ")])]),
                    cell(vec![span(vec![txt(" xxx ")])]),
                ],
                vec![
                    cell(vec![span(vec![txt(" xxx ")])]),
                    cell(vec![span(vec![txt(" yep ")]), txt(" ")]),
                ],
            ])]
        );
    }

    #[test]
    fn crossed_close_splits_and_reopens() {
        assert_eq!(
            parse("**hi--hello**hey--"),
            vec![
                style(
                    TextStyle::Bold,
                    vec![
                        txt("hi"),
                        style(TextStyle::Strikethrough, vec![txt("hello")])
                    ]
                ),
                style(TextStyle::Strikethrough, vec![txt("hey")]),
            ]
        );
    }

    #[test]
    fn crossed_open_cuts_at_block_opener() {
        assert_eq!(
            parse("[[span]] h1 [[div]] h2 [[/span]] h3 [[/div]]"),
            vec![
                span(vec![txt(" h1 ")]),
                div(vec![span(vec![txt(" h2 ")]), txt(" h3 ")]),
            ]
        );
    }

    /// R-open on a line-scoped mark: the div opener cuts the open bold;
    /// its closer then pairs the re-opened half inside the div.
    #[test]
    fn block_opener_cuts_open_mark() {
        assert_eq!(
            parse("**a [[div]] b** c [[/div]]"),
            vec![
                style(TextStyle::Bold, vec![txt("a ")]),
                div(vec![style(TextStyle::Bold, vec![txt(" b")]), txt(" c ")]),
            ]
        );
    }

    #[test]
    fn code_region_is_opaque() {
        assert_eq!(
            parse("a [[code]] [[div]] [[/code]] b"),
            vec![txt("a "), Node::Code("[[div]]".into()), txt(" b")]
        );
    }

    /// An unclosed `[[code]]` is raw text; its interior pairs were real.
    #[test]
    fn unclosed_code_keeps_interior_pairs() {
        assert_eq!(
            parse("[[code]] **b** tail"),
            vec![
                Node::Raw("[[code]]".into()),
                txt(" "),
                style(TextStyle::Bold, vec![txt("b")]),
                txt(" tail"),
            ]
        );
    }

    /// No fact, no frame: an unclosed opener and a stray closer both stay
    /// visible as raw input.
    #[test]
    fn unpaired_render_raw() {
        assert_eq!(
            parse("[[div]] body"),
            vec![Node::Raw("[[div]]".into()), txt(" body")]
        );
        assert_eq!(
            parse("[[/div]] tail"),
            vec![Node::Raw("[[/div]]".into()), txt(" tail")]
        );
        assert_eq!(parse("**bold"), vec![Node::Raw("**".into()), txt("bold")]);
    }

    /// The em-dash rule: `-- ` before text is a dash that swallows the
    /// space; a spaced non-strikethrough mark is plain text.
    #[test]
    fn spaced_marks_stay_text() {
        assert_eq!(parse("-- a b"), vec![txt("— a b")]);
        assert_eq!(parse("a ** b"), vec![txt("a ** b")]);
    }

    #[test]
    fn heading_eats_its_newline() {
        assert_eq!(
            parse("+ h\ntail"),
            vec![
                Node::Heading {
                    level: 1,
                    anchor: None,
                    content: vec![txt("h")],
                },
                txt("tail"),
            ]
        );
    }

    #[test]
    fn rule_eats_its_newline() {
        assert_eq!(
            parse("+ h\n----\nbody"),
            vec![
                Node::Heading {
                    level: 1,
                    anchor: None,
                    content: vec![txt("h")],
                },
                Node::HorizontalRule,
                txt("body"),
            ]
        );
    }

    /// One quote level spans its run of quoted lines; a deeper line nests
    /// inside it. The newlines stay leaves; only the final one lands
    /// outside.
    #[test]
    fn quote_levels_nest() {
        let q = |content| container(ContainerKind::Quote, content);
        assert_eq!(
            parse("> a\n>> b\n> c"),
            vec![q(vec![txt("a\n"), q(vec![txt("b\n")]), txt("c"),])]
        );
    }

    /// `##f00|` without a closer dies at its newline; the newline survives.
    #[test]
    fn color_spans_one_line() {
        assert_eq!(
            parse("##f00| a\nb"),
            vec![
                container(ContainerKind::Color("#f00".into()), vec![txt(" a")]),
                txt("\nb"),
            ]
        );
    }

    #[test]
    fn trailing_pipe_opens_no_cell() {
        assert_eq!(
            parse("|| a ||"),
            vec![Node::Table(vec![vec![cell(vec![txt(" a ")])]])]
        );
    }

    #[test]
    fn header_and_align_cells() {
        let mut c = cell(vec![txt("H ")]);
        c.header = true;
        c.align = Some(Align {
            floating: false,
            side: AlignSide::Center,
        });
        assert_eq!(parse("||~= H ||"), vec![Node::Table(vec![vec![c]])]);
    }

    /// List lines group into one nested list; a blank line separates two.
    #[test]
    fn lists_nest_and_separate() {
        let list = |ordered, items| List { ordered, items };
        assert_eq!(
            parse("* a\n * b\n* c"),
            vec![Node::List(list(
                false,
                vec![
                    ListItem {
                        content: vec![txt("a")],
                        sublist: Some(Box::new(list(
                            false,
                            vec![ListItem {
                                content: vec![txt("b")],
                                sublist: None,
                            }]
                        ))),
                    },
                    ListItem {
                        content: vec![txt("c")],
                        sublist: None,
                    }
                ]
            ))]
        );
        assert_eq!(
            parse("* a\n\n* b"),
            vec![
                Node::List(list(false, one_item("a"))),
                txt("\n"),
                Node::List(list(false, one_item("b"))),
            ]
        );
    }

    #[test]
    fn collapsible_wraps_its_header() {
        assert_eq!(
            parse("[[collapsible show=\"S\" hide=\"H\"]]\nbody\n[[/collapsible]]"),
            vec![Node::Collapsible {
                header: vec![Node::CollapsibleHeader {
                    folded: true,
                    open: "S".into(),
                    close: "H".into(),
                    raw: "[[collapsible show=\"S\" hide=\"H\"]]".into(),
                }],
                body: vec![txt("\nbody\n")],
            }]
        );
    }

    /// An unclosed collapsible degrades to its raw opener; the body parses
    /// on at the same level.
    #[test]
    fn unclosed_collapsible_degrades() {
        assert_eq!(
            parse("[[collapsible]] body"),
            vec![Node::Raw("[[collapsible]]".into()), txt(" body")]
        );
    }

    /// Both headers sealed into one wrapper: the nearer closer claims them
    /// both, so the far closer finds no header and degrades to its own
    /// default header over an empty body — never vanishing.
    #[test]
    fn headerless_collapsible_keeps_a_default_header() {
        let hdr = || Node::CollapsibleHeader {
            folded: true,
            open: "+ show block".into(),
            close: "- hide block".into(),
            raw: "[[collapsible]]".into(),
        };
        assert_eq!(
            parse(
                "[[size 120%]] [[collapsible]] [[collapsible]] [[/size]] B [[/collapsible]] [[/collapsible]]"
            ),
            vec![
                Node::Collapsible {
                    header: vec![size(
                        "120%",
                        vec![txt(" "), hdr(), txt(" "), hdr(), txt(" "),]
                    )],
                    body: vec![txt(" B ")],
                },
                txt(" "),
                Node::Collapsible {
                    header: vec![hdr()],
                    body: Vec::new(),
                },
            ]
        );
    }

    #[test]
    fn tabview_gathers_tabs() {
        assert_eq!(
            parse("[[tabview]] [[tab A]] a [[/tab]] [[tab B]] b [[/tab]] [[/tabview]]"),
            vec![Node::Tabview {
                id: 0,
                tabs: vec![
                    Tab {
                        name: vec![txt("A")],
                        content: vec![txt(" a ")],
                    },
                    Tab {
                        name: vec![txt("B")],
                        content: vec![txt(" b ")],
                    },
                ],
            }]
        );
    }

    fn one_item(s: &str) -> Vec<ListItem> {
        vec![ListItem {
            content: vec![txt(s)],
            sublist: None,
        }]
    }

    /// Differential harness over the full local export repo: the builder
    /// against the old merger (the oracle). Everything diverging is triaged:
    /// builder bug → fix; whitelist (verbatim opacity, R-open, quirk
    /// degradation) → accept; merger bug → document. The strict count is
    /// inflated by the merger's line-seal quirk (an inline pair directly
    /// before a line's newline keeps the newline — sometimes the next
    /// construct — inside the line construct), so the loose pass strips
    /// newline-only runs from plain text and re-compares: what still
    /// diverges is structural. Run explicitly:
    /// `cargo test -p kolorinko --release -- --ignored --nocapture archive_diff`.
    #[test]
    #[ignore = "walks the full local export repo"]
    fn archive_diff_against_merge() {
        use std::path::{Path, PathBuf};
        let root =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.kolorinko/repo/rpcauthority/pages");
        if !root.exists() {
            eprintln!("skipping: real repo not present");
            return;
        }
        let (mut files, mut diff_files, mut loose_files) = (0, 0, 0);
        // (byte-length gap, path, old, new) — the thinnest diffs surface
        // first; big restructurings drown the subtle bugs.
        let mut diffs: Vec<(usize, PathBuf, String, String)> = Vec::new();
        visit(&root, &mut |path, src| {
            files += 1;
            let toks = lex(src);
            let old = merge::parse_toks(src, &toks);
            let new = parse_toks(src, &toks);
            if old != new {
                diff_files += 1;
                if loose(&old) != loose(&new) {
                    loose_files += 1;
                    let (a, b) = (format!("{old:?}"), format!("{new:?}"));
                    diffs.push((a.len().abs_diff(b.len()), path.to_path_buf(), a, b));
                }
            }
        });
        diffs.sort_by_key(|(gap, ..)| *gap);
        // The merger's line-seal quirk leaves a telltale in its own output:
        // a text node beginning with a newline right at the divergence
        // (the unsealed line construct swallowing what followed). Skipped
        // below; what remains is worth a human eye.
        let (mut shown, mut quirk) = (0, 0);
        for (gap, path, a, b) in diffs.iter() {
            let at = a
                .bytes()
                .zip(b.bytes())
                .position(|(x, y)| x != y)
                .unwrap_or(a.len().min(b.len()));
            let mut lo = at.saturating_sub(160);
            while !a.is_char_boundary(lo) {
                lo -= 1;
            }
            let w_old = &a[lo..at];
            if w_old.contains("\\n") {
                quirk += 1;
                continue;
            }
            if shown < 14 {
                shown += 1;
                eprintln!(
                    "REAL gap={gap} {} @ {at}\n  old: …{}…\n  new: …{}…",
                    path.display(),
                    w_old,
                    &b[lo..(at + 160).min(b.len())],
                );
            }
        }
        eprintln!("quirk-signature={quirk}/{diff_files}");
        eprintln!(
            "strict={diff_files}/{files} ({:.2}%) loose={loose_files}/{files} ({:.2}%)",
            100.0 * diff_files as f64 / files as f64,
            100.0 * loose_files as f64 / files as f64,
        );
    }

    /// The loose tree: plain text stripped of newline-only runs (the merger
    /// quirk moves them across node boundaries without structural change).
    fn loose(c: &Content) -> Content {
        c.iter()
            .filter_map(|n| match n {
                Node::Text(TextObj::Plain(s)) if s.chars().all(|ch| ch == '\n') => None,
                Node::Text(TextObj::Plain(s)) => Some(txt(s.trim_matches('\n'))),
                Node::Container { kind, content } => Some(Node::Container {
                    kind: kind.clone(),
                    content: loose(content),
                }),
                Node::List(l) => Some(Node::List(loose_list(l))),
                Node::Collapsible { header, body } => Some(Node::Collapsible {
                    header: loose(header),
                    body: loose(body),
                }),
                other => Some(other.clone()),
            })
            .collect()
    }

    fn loose_list(l: &List) -> List {
        List {
            ordered: l.ordered,
            items: l
                .items
                .iter()
                .map(|it| ListItem {
                    content: loose(&it.content),
                    sublist: it.sublist.as_ref().map(|sl| Box::new(loose_list(sl))),
                })
                .collect(),
        }
    }

    fn visit(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path, &str)) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, f);
            } else if path.extension().is_some_and(|e| e == "txt") {
                if let Ok(src) = std::fs::read_to_string(&path) {
                    f(&path, &src);
                }
            }
        }
    }
}
