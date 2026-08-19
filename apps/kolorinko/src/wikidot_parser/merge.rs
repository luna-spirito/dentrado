//! The merge pass: a plain recursive-descent walk over the [`Token`] slice
//! produced by [`lex`], pairing openers with closers into [`Node`]s. No
//! backtracking, no cloning of parsed output — each token is consumed exactly
//! once and nodes are built in place.
//!
//! The structure mirrors the old chumsky grammar's central ideas:
//! * [`Merger::loop_until_closer_at`] is the old `content_loop`: it stops at
//!   (and consumes) the first closing tag — or comment close — reporting why,
//!   and *flattens* a container whose closer never came into a `Node::Raw`
//!   opener plus its body, propagating the foreign stop to the ancestor.
//! * [`Merger::body_until`] is the old `content_before`: elements until a
//!   caller-supplied sigil (a mark, a newline). Closers stop it too, but are
//!   left for the enclosing loop.
//! * Degradation: any token that appears where its construct is not
//!   recognized (`||` outside a table, `[[head]]` outside a listpages body, a
//!   stray `--]`) becomes the text the old parser's character fallback
//!   produced.

use super::lexer::{OpenTag, Params, SectionSlot, Tok, Token, lex};
use super::*;

/// Why a content loop stopped. The merge counterpart of the old
/// `ContentExitReason`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    Eof,
    Tag(ClosedTag),
    Comment,
}

/// Parse a token slice as a standalone document, absorbing stray closers as
/// raw text (the old `parse()`).
pub(crate) fn parse_toks(src: &str, toks: &[Token]) -> Content {
    let mut m = Merger { src, toks, pos: 0 };
    merge_text(m.content())
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
}

/// Uniform `[[…]]`-opener handler signature. Every arm is called through this
/// one shape so [`Merger::open_tag`] reserves a single call frame — it sits on
/// the recursive container cycle, where per-arm stack slots would multiply
/// per nesting level.
type TagArm<'src> = fn(&mut Merger<'src>, (usize, usize), OpenTag<'src>) -> (Content, Option<Stop>);

/// Uniform token-handler signature for [`Merger::element`], same rationale as
/// [`TagArm`].
type TokArm<'src> = fn(&mut Merger<'src>, &Tok<'src>) -> (Content, Option<Stop>);

impl<'src> Merger<'src> {
    fn peek(&self) -> Option<&Tok<'src>> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek_is(&self, f: impl FnOnce(&Tok) -> bool) -> bool {
        self.peek().is_some_and(f)
    }

    /// The byte offset of the next token (or EOF), for slicing verbatim
    /// bodies that end here.
    fn here(&self) -> usize {
        self.toks.get(self.pos).map_or(self.src.len(), |t| t.start)
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
                Stop::Tag(tag) => nodes.push(Node::Raw(format!("[[/{}]]", tag.opener_str()))),
            }
        }
    }

    /// Elements until the first closer — the old `content_loop`. Returns the
    /// nodes, the byte offset where the body ended (the closer's start — raw
    /// bodies slice `src[opener.end..end]`), and why it stopped. An element
    /// that flattened against a foreign closer propagates that stop; the
    /// closer it consumed is gone either way.
    fn loop_until_closer_at(&mut self, comment: bool) -> (Content, usize, Stop) {
        let mut nodes = Content::new();
        loop {
            match self.peek() {
                None => return (nodes, self.src.len(), Stop::Eof),
                Some(Tok::Close(tag)) => {
                    let end = self.toks[self.pos].start;
                    let stop = Stop::Tag(tag.clone());
                    self.pos += 1;
                    return (nodes, end, stop);
                }
                Some(Tok::CommentClose) if comment => {
                    let end = self.toks[self.pos].start;
                    self.pos += 1;
                    return (nodes, end, Stop::Comment);
                }
                _ => {}
            }
            let start = self.toks[self.pos].start;
            let (sub, reason) = self.element();
            nodes.extend(sub);
            if let Some(stop) = reason {
                return (nodes, start, stop);
            }
        }
    }

    /// Elements until a stop-sigil token (peeked, the caller consumes it) —
    /// the old `content_before`. Closers stop it too, unconsumed, so an
    /// enclosing loop can claim them.
    fn body_until(&mut self, stop_is: impl Fn(&Tok) -> bool + Copy) -> Content {
        let mut nodes = Content::new();
        loop {
            match self.peek() {
                None | Some(Tok::Close(_)) => return nodes,
                Some(tok) if stop_is(tok) => return nodes,
                _ => {}
            }
            let (sub, reason) = self.element();
            nodes.extend(sub);
            if reason.is_some() {
                return nodes;
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
    /// container flattened against a foreign closer it consumed. Deliberately
    /// a fn-pointer dispatcher — it sits on the recursive container cycle,
    /// where per-arm stack slots would multiply per nesting level.
    fn element(&mut self) -> (Content, Option<Stop>) {
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
            // Unreachable at loop level (the loops check closers first);
            // defensive degrade like the old char fallback.
            Tok::Close(_) => Self::degrade_tok,
            Tok::Open(_) => Self::open_bracket,
        };
        arm(self, &tok)
    }

    fn text(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::Text(s) = tok else { unreachable!() };
        self.pos += 1;
        // Typography (the Text_Wiki rule): `...` / `. . .` → ellipsis.
        (vec![self.text_node(&typography(s))], None)
    }

    fn tt(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn clearfloat(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::Clearfloat(side) = *tok else {
            unreachable!()
        };
        self.pos += 1;
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        (vec![Node::Clearfloat(side)], None)
    }

    fn anchor_target(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::AnchorTarget(name) = tok else {
            unreachable!()
        };
        self.pos += 1;
        (vec![Node::AnchorTarget(name.to_string())], None)
    }

    fn if_expr(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn newline(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.pos += 1;
        (vec![self.text_node("\n")], None)
    }

    fn escape(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::Escape(body) = tok else {
            unreachable!()
        };
        self.pos += 1;
        (vec![self.text_node(body)], None)
    }

    fn url_link(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn module_var(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn include_var(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn sup(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.sup_sub(true)
    }

    fn sub(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.sup_sub(false)
    }

    fn sup_sub(&mut self, sup: bool) -> (Content, Option<Stop>) {
        let is_mark: fn(&Tok) -> bool = if sup {
            |t| matches!(t, Tok::SupMark)
        } else {
            |t| matches!(t, Tok::SubMark)
        };
        self.pos += 1;
        let body = self.body_until(|t| is_mark(t) || matches!(t, Tok::Newline));
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
        (vec![node], None)
    }

    fn heading(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::Heading(level) = *tok else {
            unreachable!()
        };
        self.pos += 1;
        (
            vec![Node::Heading {
                level,
                anchor: None,
                content: self.line_body(),
            }],
            None,
        )
    }

    fn rule(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.pos += 1;
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        (vec![Node::HorizontalRule], None)
    }

    fn list_line(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        (vec![self.list_block()], None)
    }

    fn center_eq(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.pos += 1;
        (
            vec![Node::Container {
                kind: ContainerKind::Align(Align {
                    floating: false,
                    side: AlignSide::Center,
                }),
                content: self.line_body(),
            }],
            None,
        )
    }

    fn table_lines(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        (vec![self.table_block()], None)
    }

    fn degrade_tok(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        (vec![self.degrade()], None)
    }

    fn link3(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    fn link1(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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

    /// `//`, `**`, `__`, `--`. An opener immediately followed by a space is
    /// not a span (the old `just(' ').not()` guard); `-- ` there is an
    /// em-dash that swallows the space. A `--` with no closer on the line (or
    /// one whose body has whitespace rims) is not strikethrough either — the
    /// Text_Wiki regex requires non-space edges — so both marks render as
    /// em-dashes around the free body.
    fn mark(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
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
            (TextStyle::Strikethrough, false) => self.strikethrough(),
            (_, false) => (vec![self.style_body(style)], None),
        }
    }

    /// A `--` opener with a non-space body: strikethrough only when a `--`
    /// closer follows on the same line and the body has non-space rims;
    /// otherwise each mark is an em-dash and the body stays free.
    fn strikethrough(&mut self) -> (Content, Option<Stop>) {
        let mut body =
            self.body_until(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough) | Tok::Newline));
        if !self.peek_is(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough))) {
            let mut out = vec![self.text_node("—")];
            out.append(&mut body);
            return (out, None);
        }
        self.pos += 1;
        let rim_ws = matches!(body.first(), Some(Node::Text(TextObj::Plain(s))) if s.starts_with(char::is_whitespace))
            || matches!(body.last(), Some(Node::Text(TextObj::Plain(s))) if s.ends_with(char::is_whitespace));
        if rim_ws {
            let mut out = vec![self.text_node("—")];
            out.append(&mut body);
            out.push(self.text_node("—"));
            (out, None)
        } else {
            (
                vec![Node::Container {
                    kind: ContainerKind::Style(TextStyle::Strikethrough),
                    content: body,
                }],
                None,
            )
        }
    }

    /// The body of a just-consumed style mark, up to the next mark of the
    /// same style or the end of line (the mark, if present, consumed).
    fn style_body(&mut self, style: TextStyle) -> Node {
        let content = self
            .body_until(|t| matches!(t, Tok::Mark(s) if *s == style) || matches!(t, Tok::Newline));
        if self.peek_is(|t| matches!(t, Tok::Mark(s) if *s == style)) {
            self.pos += 1;
        }
        Node::Container {
            kind: ContainerKind::Style(style),
            content,
        }
    }

    /// `##spec|…##`. The closer is any `##` token; one carrying a spec (an
    /// opener that landed in closer position) leaves its `spec|` as text
    /// after the node, exactly what the old unconditional closer consume did.
    fn color_span(&mut self, tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Tok::ColorOpen(spec) = tok else {
            unreachable!()
        };
        self.pos += 1;
        let body =
            self.body_until(|t| matches!(t, Tok::ColorOpen(_) | Tok::ColorClose | Tok::Newline));
        let mut trailing = None;
        match self.peek() {
            Some(Tok::ColorClose) => self.pos += 1,
            Some(Tok::ColorOpen(next_spec)) => {
                let spec = *next_spec;
                self.pos += 1;
                trailing = Some(format!("{spec}|"));
            }
            _ => {}
        }
        let node = Node::Container {
            kind: ContainerKind::Color(normalize_color(spec.to_string())),
            content: body,
        };
        match trailing {
            None => (vec![node], None),
            Some(t) => (vec![node, self.text_node(&t)], None),
        }
    }

    /// A stray `--]` outside a comment: the old parser saw a `--`
    /// strikethrough opener whose body starts with `]`.
    fn stray_comment_close(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        self.pos += 1;
        let mut body = vec![self.text_node("]")];
        body.extend(
            self.body_until(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough) | Tok::Newline)),
        );
        if self.peek_is(|t| matches!(t, Tok::Mark(TextStyle::Strikethrough))) {
            self.pos += 1;
        }
        (
            vec![Node::Container {
                kind: ContainerKind::Style(TextStyle::Strikethrough),
                content: body,
            }],
            None,
        )
    }

    // ── quote region ──────────────────────────────────────────────────────

    /// A `>` blockquote: the maximal run of marked lines, one marker level
    /// stripped per line, recursively merged as a document. `>>` and `> >`
    /// nest by leaving a QuoteMark in the stripped inner. A closer inside the
    /// region cuts it short and propagates to the container outside the
    /// quote, the way the old marked-closer check let `> [[/div]]` close an
    /// outer `[[div]]`.
    fn blockquote(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
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
        let mut m = Merger {
            src: self.src,
            toks: &inner,
            pos: 0,
        };
        let (content, _, stop) = m.loop_until_closer_at(false);
        let node = Node::Container {
            kind: ContainerKind::Quote,
            content,
        };
        match stop {
            Stop::Eof => (vec![node], None),
            stop => (vec![node], Some(stop)),
        }
    }

    // ── line blocks ───────────────────────────────────────────────────────

    /// The rest of the line and its newline (the old `content_before(line_end)`
    /// plus `line_end()`).
    fn line_body(&mut self) -> Content {
        let content = self.body_until(|t| matches!(t, Tok::Newline));
        if self.peek_is(|t| matches!(t, Tok::Newline)) {
            self.pos += 1;
        }
        content
    }

    /// One or more `*` / `#` lines folded into a nested [`List`] (the old
    /// `list_block` + `build_list`).
    fn list_block(&mut self) -> Node {
        let mut lines: Vec<(usize, bool, Content)> = Vec::new();
        while let Some(Tok::ListMark { ordered, indent }) = self.peek().cloned() {
            self.pos += 1;
            let content = self.line_body();
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
                let content = self.body_until(|t| matches!(t, Tok::Pipe2 | Tok::Newline));
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
        stop: Stop,
    ) -> (Content, Option<Stop>) {
        let mut out = Vec::with_capacity(body.len() + 1);
        out.push(Node::Raw(self.src[opener.0..opener.1].to_string()));
        out.extend(body);
        (out, Some(stop))
    }

    /// The old `balanced`: body until any closer; on the matching one `build`
    /// forms the node, otherwise flatten and propagate.
    fn balanced_node(
        &mut self,
        opener: (usize, usize),
        closer: ClosedTag,
        build: impl FnOnce(Content) -> Node,
    ) -> (Content, Option<Stop>) {
        let (body, _, stop) = self.loop_until_closer_at(false);
        if stop == Stop::Tag(closer) {
            (vec![build(body)], None)
        } else {
            self.raw_flatten(opener, body, stop)
        }
    }

    /// Dispatch for `[[…]]` openers: pick the uniform arm, call it once.
    fn open_tag(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let arm: TagArm<'src> = match tag {
            OpenTag::Div { .. } => Self::div_arm,
            OpenTag::Span { .. } => Self::span_arm,
            OpenTag::Anchor { .. } => Self::anchor_arm,
            OpenTag::Collapsible { .. } => Self::collapsible_arm,
            OpenTag::Size(_) => Self::size_arm,
            OpenTag::IfTags(_) => Self::iftags_arm,
            OpenTag::Align { .. } => Self::align_arm,
            OpenTag::User { .. } => Self::user_arm,
            OpenTag::Footnote => Self::footnote_arm,
            OpenTag::Footnoteblock => Self::footnoteblock_arm,
            OpenTag::ModuleBlock { .. } => Self::module_block_arm,
            // `[[cell]]` / `[[hcell]]` are recognized at element level
            // anywhere (that is how a cell wrapped in `[[iftags]]` inside a
            // grid-table row parses); both close with `[[/cell]]`-style tags.
            OpenTag::Cell { .. } => Self::cell_arm,
            OpenTag::Code => Self::code_arm,
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
        };
        arm(self, opener, tag)
    }

    fn div_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Div { underscore, params } = tag else {
            unreachable!()
        };
        self.balanced_node(opener, ClosedTag::Div, move |content| Node::Container {
            kind: ContainerKind::Div {
                inline: false,
                block: !underscore,
                params,
            },
            content,
        })
    }

    fn span_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Span { params } = tag else {
            unreachable!()
        };
        self.balanced_node(opener, ClosedTag::Span, move |content| Node::Container {
            kind: ContainerKind::Div {
                inline: true,
                block: false,
                params,
            },
            content,
        })
    }

    fn anchor_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::Anchor { params } = tag else {
            unreachable!()
        };
        // `[[a]]` is just a link that also carries a class: classify the href
        // like any other target (so it gets auto-rewritten), and thread the
        // class through to the renderer.
        let class = attr_value(&params, "class").filter(|s| !s.is_empty());
        let target = params
            .get("href")
            .map(|v| parse_link_target_objs(v))
            .unwrap_or(LinkTarget::Url("#".to_string()));
        self.balanced_node(opener, ClosedTag::Anchor, move |content| Node::Link {
            target,
            text: content,
            class,
        })
    }

    fn user_arm(&mut self, _opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
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

    fn footnote_arm(
        &mut self,
        opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        self.balanced_node(opener, ClosedTag::Footnote, Node::Footnote)
    }

    fn footnoteblock_arm(
        &mut self,
        _opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        (vec![Node::FootnoteBlock(Vec::new())], None)
    }

    fn module_block_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::ModuleBlock { name, params } = tag else {
            unreachable!()
        };
        self.balanced_node(opener, ClosedTag::Module, move |body| Node::ModuleBlock {
            name,
            params,
            body,
        })
    }

    fn collapsible_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::Collapsible { params } = tag else {
            unreachable!()
        };
        let show = attr_value(&params, "show").unwrap_or_else(|| "+ show block".into());
        let hide = attr_value(&params, "hide").unwrap_or_else(|| "- hide block".into());
        let folded = !matches!(
            attr_value(&params, "folded").as_deref(),
            Some("no") | Some("false")
        );
        self.balanced_node(opener, ClosedTag::Collapsible, move |content| {
            Node::Collapsible {
                folded,
                show,
                hide,
                content,
            }
        })
    }

    fn size_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Size(arg) = tag else {
            unreachable!()
        };
        self.balanced_node(opener, ClosedTag::Size, move |content| Node::Container {
            kind: ContainerKind::Size(arg.to_string()),
            content,
        })
    }

    fn iftags_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::IfTags(filter) = tag else {
            unreachable!()
        };
        let (has_all, has_none) = parse_tag_filter(filter);
        self.balanced_node(opener, ClosedTag::IfTags, move |content| Node::Container {
            kind: ContainerKind::IfTags { has_all, has_none },
            content,
        })
    }

    fn align_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Align { floating, side } = tag else {
            unreachable!()
        };
        self.balanced_node(
            opener,
            ClosedTag::Align { floating, side },
            move |content| Node::Container {
                kind: ContainerKind::Align(Align { floating, side }),
                content,
            },
        )
    }

    fn cell_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Cell { header, params } = tag else {
            unreachable!()
        };
        self.balanced_node(opener, ClosedTag::Cell, move |content| {
            Node::BlockCell(BlockCell {
                header,
                params,
                content,
            })
        })
    }

    fn table_arm(&mut self, opener: (usize, usize), tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        let OpenTag::Table { params } = tag else {
            unreachable!()
        };
        self.block_table(opener, params)
    }

    fn tabview_arm(
        &mut self,
        opener: (usize, usize),
        _tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        self.tabview(opener)
    }

    fn code_arm(&mut self, opener: (usize, usize), _tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        self.raw_body(opener, ClosedTag::Code, |body| {
            Node::Code(body.trim().to_string())
        })
    }

    fn css_arm(&mut self, opener: (usize, usize), _tag: OpenTag<'src>) -> (Content, Option<Stop>) {
        self.raw_body(opener, ClosedTag::Module, |body| {
            Node::Stylesheet(body.trim().to_string())
        })
    }

    fn listpages_arm(
        &mut self,
        opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::ListPages { params } = tag else {
            unreachable!()
        };
        self.listpages(opener, params)
    }

    fn module_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
        let OpenTag::Module { name, params } = tag else {
            unreachable!()
        };
        (vec![Node::Module { name, params }], None)
    }

    fn include_arm(
        &mut self,
        _opener: (usize, usize),
        tag: OpenTag<'src>,
    ) -> (Content, Option<Stop>) {
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
    ) -> (Content, Option<Stop>) {
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
    ) -> (Content, Option<Stop>) {
        self.pos -= 1;
        (vec![self.degrade()], None)
    }

    /// A `[[` opener: hand the tag and its span to the tag dispatcher.
    fn open_bracket(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let Token { tok, start, end } = &self.toks[self.pos];
        let Tok::Open(tag) = tok else { unreachable!() };
        let opener = (*start, *end);
        self.pos += 1;
        self.open_tag(opener, tag.clone())
    }

    /// The old `raw_balanced`: the body is sliced verbatim from the source
    /// between the opener and the stop point; the parsed nodes matter only on
    /// the flatten path.
    fn raw_body(
        &mut self,
        opener: (usize, usize),
        closer: ClosedTag,
        build: impl Fn(&str) -> Node,
    ) -> (Content, Option<Stop>) {
        let (body, body_end, stop) = self.loop_until_closer_at(false);
        if stop == Stop::Tag(closer) {
            (vec![build(&self.src[opener.1..body_end])], None)
        } else {
            self.raw_flatten(opener, body, stop)
        }
    }

    /// The old `module_block` ListPages branch: the body's four slots, closed
    /// only by a `[[/module]]`.
    fn listpages(&mut self, opener: (usize, usize), params: Params) -> (Content, Option<Stop>) {
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
                    stop = Stop::Tag(tag);
                    break;
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
                    let (sub, reason) = self.element();
                    match slot {
                        Some(SectionSlot::Head) => &mut head,
                        Some(SectionSlot::Body) => &mut body,
                        Some(SectionSlot::Foot) => &mut foot,
                        None => &mut main,
                    }
                    .extend(sub);
                    if let Some(stop_inner) = reason {
                        stop = stop_inner;
                        break;
                    }
                }
            }
        }
        let mut lp = ListPages {
            params: listpages_params(&params),
            prepend: head,
            repeat: if body_section { body } else { main },
            append: foot,
        };
        if stop == Stop::Tag(ClosedTag::Module) {
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
    fn block_table(&mut self, opener: (usize, usize), params: Params) -> (Content, Option<Stop>) {
        let mut rows: Vec<BlockRow> = Vec::new();
        let mut stray = Content::new();
        let mut stop = Stop::Eof;
        loop {
            self.skip_container_ws();
            match self.peek().cloned() {
                None => break,
                Some(Tok::Close(tag)) => {
                    self.pos += 1;
                    stop = Stop::Tag(tag);
                    break;
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
                    let (sub, reason) = self.element();
                    stray.extend(sub);
                    if let Some(s) = reason {
                        stop = s;
                        break;
                    }
                }
            }
        }
        if stop == Stop::Tag(ClosedTag::Table) {
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
    fn tabview(&mut self, opener: (usize, usize)) -> (Content, Option<Stop>) {
        let mut tabs: Vec<Tab> = Vec::new();
        let mut stray = Content::new();
        let mut stop = Stop::Eof;
        loop {
            self.skip_container_ws();
            match self.peek().cloned() {
                None => break,
                Some(Tok::Close(tag)) => {
                    self.pos += 1;
                    stop = Stop::Tag(tag);
                    break;
                }
                Some(Tok::Open(OpenTag::Tab { name })) => {
                    self.pos += 1;
                    let name = parse_sub(name);
                    // A tab ends at any closer, consumed and dropped.
                    let (content, _, _) = self.loop_until_closer_at(false);
                    tabs.push(Tab { name, content });
                }
                _ => {
                    let (sub, reason) = self.element();
                    stray.extend(sub);
                    if let Some(s) = reason {
                        stop = s;
                        break;
                    }
                }
            }
        }
        if stop == Stop::Tag(ClosedTag::Tabview) {
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
    fn comment(&mut self, _tok: &Tok<'src>) -> (Content, Option<Stop>) {
        let opener = (self.toks[self.pos].start, self.toks[self.pos].end);
        self.pos += 1;
        let (body, _, stop) = self.loop_until_closer_at(true);
        if stop == Stop::Comment {
            (Content::new(), None)
        } else {
            self.raw_flatten(opener, body, stop)
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
