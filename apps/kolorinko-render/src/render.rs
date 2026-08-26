//! Render a parsed Wikidot page ([`kolorinko_wikitext::Content`]) into Leptos
//! views whose DOM matches Wikidot's output closely enough that legacy
//! user-authored CSS themes continue to apply.
//!
//! `site` is threaded through so internal links resolve to
//! `/<site>/<category?>/<page>` — the path-prefix scheme this mirror uses
//! instead of Wikidot's per-site subdomains.

use kolorinko_rt::{LocalId, format_page_route};
use kolorinko_wikitext::{
    Align, AlignSide, BlockCell, BlockTable, ClearSide, ContainerKind, Content, LinkTarget, List,
    Node, TableCell, TextObj, TextStyle, civil_from_days, days_from_civil,
};
use leptos::prelude::*;
use leptos::tachys::html::element::custom;

/// Rendering parameters threaded through every render function: the mirror
/// The addressing scope of a render: `space` — the canonical space its
/// pages render under (the prefix internal links point at) — and `default`
/// — the space this origin already names (`None` on the server, which
/// always emits full-weight links; the host's own space on the client,
/// where every href simplifies against it — [`kolorinko_rt::simplify`]).
#[derive(Clone, Copy)]
pub struct Scope {
    pub space: Option<kolorinko_rt::SpaceId>,
    pub default: Option<kolorinko_rt::SpaceId>,
}

impl Scope {
    /// The href of the space's root (its landing page): the full `/{space}`
    /// through the scope's simplification — `/` on a wiki's own domain,
    /// or on a context-less render.
    pub fn root(&self) -> String {
        self.space.map_or_else(
            || "/".to_string(),
            |s| kolorinko_rt::simplify(self.default, &format!("/{s}")),
        )
    }
}

/// site (internal link resolution) and, inside a collapsible's header
/// subtree, which toggle link is being produced. The header subtree is
/// walked once per link — see [`render_collapsible`].
#[derive(Clone, Copy)]
pub(crate) struct RenderCtx {
    scope: Scope,
    link: Option<CollapsibleLinkCtx>,
}

/// One toggle link of a collapsible: which label the
/// [`crate::Node::CollapsibleHeader`] leaf shows, and the shared
/// unfolded-state signal the click flips.
#[derive(Clone, Copy)]
pub(crate) struct CollapsibleLinkCtx {
    unfolded: RwSignal<bool>,
    open: bool,
}

impl RenderCtx {
    fn new(scope: Scope) -> Self {
        Self { scope, link: None }
    }

    fn with_link(&self, link: CollapsibleLinkCtx) -> Self {
        Self {
            link: Some(link),
            ..*self
        }
    }
}

pub(crate) fn render_inline(ctx: &RenderCtx, content: &[Node]) -> Vec<AnyView> {
    content.iter().map(|n| render_node(ctx, n)).collect()
}

/// A view that renders to nothing at all (no hydration-marker residue).
fn empty_view() -> AnyView {
    Vec::<AnyView>::new().into_any()
}

/// Slice off leading/trailing nodes that are pure whitespace text, so inline
/// containers (`[[div_ …]]`, `[[span …]]`, grid cells) don't emit spurious
/// `<br>` from the newlines around their real content.
fn trim_ws(content: &Content) -> &[Node] {
    let is_ws = |n: &Node| matches!(n, Node::Text(TextObj::Plain(s)) if s.trim().is_empty());
    let mut start = 0;
    let mut end = content.len();
    while start < end && is_ws(&content[start]) {
        start += 1;
    }
    while end > start && is_ws(&content[end - 1]) {
        end -= 1;
    }
    &content[start..end]
}

/// Render `#page-content`: top-level inline runs are grouped into `<p>`,
/// block nodes render standalone — exactly as Wikidot's renderer does. A run of
/// blank lines inside a text node is a paragraph break; single newlines stay
/// inside their paragraph as soft breaks (`<br>`). `scope` — see [`Scope`] —
/// is the render's addressing scope: where internal links point, and what
/// they simplify against.
pub fn render_block(scope: Scope, content: &Content) -> Vec<AnyView> {
    render_block_content(&RenderCtx::new(scope), content)
}

fn render_block_content(ctx: &RenderCtx, content: &Content) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(content.len());
    let mut para: Vec<Piece> = Vec::new();
    for node in content {
        if is_block(node) {
            flush(&mut para, &mut out);
            out.push(render_node(ctx, node));
        } else if let Node::Text(TextObj::Plain(t)) = node {
            for tok in para_tokens(t) {
                match tok {
                    ParaToken::Text(s) => para.push(Piece::Text(s)),
                    ParaToken::Break => flush(&mut para, &mut out),
                }
            }
        } else if let Node::Container {
            kind:
                ContainerKind::Div {
                    inline: true,
                    block: false,
                    params,
                },
            content,
        } = node
        {
            // `[[span]]`: blank lines inside the body end the enclosing
            // paragraph, so each run becomes its own `<p><span>…</span></p>`.
            let runs = paragraph_runs(content);
            if runs.len() > 1 {
                flush(&mut para, &mut out);
                let (class, style) = params_to_class_style(params);
                for run in runs {
                    let inner = render_inline(ctx, &run);
                    out.push(
                        view! { <p><span class=class.clone() style=style.clone()>{inner}</span></p> }
                            .into_any(),
                    );
                }
            } else {
                para.push(Piece::Node(render_node(ctx, node)));
            }
        } else {
            para.push(Piece::Node(render_node(ctx, node)));
        }
    }
    flush(&mut para, &mut out);
    out
}

/// Split a text run into paragraph tokens: a blank line — two or more newlines
/// separated only by spaces/tabs — is a [`ParaToken::Break`] (paragraph
/// boundary); everything else is a verbatim text segment whose single newlines
/// remain as soft breaks. No edge trimming here: whether a segment sits at a
/// paragraph rim (trim, so `<p>`s stay clean) or at a seam between adjacent
/// inline nodes (keep verbatim, whitespace included) is only known when the
/// paragraph is assembled — see [`para_views`].
enum ParaToken {
    Text(String),
    Break,
}

fn para_tokens(s: &str) -> Vec<ParaToken> {
    let b = s.as_bytes();
    let n = b.len();
    let mut toks = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        if b[i] == b'\n' {
            // Consume a blank-line run: `\n (\s* \n)*`  (two or more newlines).
            let mut k = i;
            let mut newlines = 0;
            loop {
                if k < n && b[k] == b'\n' {
                    newlines += 1;
                    k += 1;
                    while k < n && matches!(b[k], b' ' | b'\t' | b'\r') {
                        k += 1;
                    }
                } else {
                    break;
                }
            }
            if newlines >= 2 {
                if i > start {
                    toks.push(ParaToken::Text(s[start..i].to_string()));
                }
                toks.push(ParaToken::Break);
                start = k;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    if start < n {
        toks.push(ParaToken::Text(s[start..].to_string()));
    }
    toks
}

/// A paragraph under assembly: verbatim text segments and pre-rendered inline
/// nodes, stitched together (and rim-trimmed) by [`para_views`].
enum Piece {
    Text(String),
    Node(AnyView),
}

/// Assemble a paragraph: trim whitespace off the paragraph's outer edges,
/// dropping text pieces left empty and moving inward past them — Wikidot's
/// `<p>`s never open or close with a `<br>` or stray spaces, however many
/// text nodes sit at the rim. Trimming stops at the first surviving piece
/// (text with content, or any pre-rendered inline node). Text between two
/// inline nodes is a mid-paragraph seam and survives verbatim, single
/// newlines and all. An all-whitespace paragraph yields no views (no `<p>`).
fn para_views(para: &mut Vec<Piece>) -> Vec<AnyView> {
    while let Some(Piece::Text(s)) = para.first_mut() {
        *s = s.trim_start().to_string();
        if s.is_empty() {
            para.remove(0);
        } else {
            break;
        }
    }
    while let Some(Piece::Text(s)) = para.last_mut() {
        *s = s.trim_end().to_string();
        if s.is_empty() {
            para.pop();
        } else {
            break;
        }
    }
    std::mem::take(para)
        .into_iter()
        .filter_map(|piece| match piece {
            Piece::Text(s) if s.is_empty() => None,
            Piece::Text(s) => Some(render_plain(&s)),
            Piece::Node(view) => Some(view),
        })
        .collect()
}

fn flush(para: &mut Vec<Piece>, out: &mut Vec<AnyView>) {
    let views = para_views(para);
    if !views.is_empty() {
        out.push(view! { <p>{views}</p> }.into_any());
    }
}

/// Blank-line runs inside a plain-text run: a single newline stays inside the
/// paragraph (it becomes a `<br>` later), a blank line (two or more newlines,
/// possibly with intervening spaces/tabs) is a paragraph boundary.
enum BlankTok {
    Raw(String),
    Break,
}

fn blank_tokens(s: &str) -> Vec<BlankTok> {
    let b = s.as_bytes();
    let n = b.len();
    let mut toks = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < n {
        if b[i] == b'\n' {
            let mut k = i;
            let mut newlines = 0;
            loop {
                if k < n && b[k] == b'\n' {
                    newlines += 1;
                    k += 1;
                    while k < n && matches!(b[k], b' ' | b'\t' | b'\r') {
                        k += 1;
                    }
                } else {
                    break;
                }
            }
            if newlines >= 2 {
                if i > start {
                    toks.push(BlankTok::Raw(s[start..i].to_string()));
                }
                toks.push(BlankTok::Break);
                start = k;
                i = k;
                continue;
            }
        }
        i += 1;
    }
    if start < n {
        let rest = &s[start..];
        if !rest.is_empty() {
            toks.push(BlankTok::Raw(rest.to_string()));
        }
    }
    toks
}

/// Split inline container content into paragraph runs at blank lines, keeping
/// single newlines inside each run (they render as `<br>`). Non-text nodes are
/// opaque and never split a run.
fn paragraph_runs(content: &[Node]) -> Vec<Vec<Node>> {
    let mut runs: Vec<Vec<Node>> = Vec::new();
    let mut cur: Vec<Node> = Vec::new();
    for node in content {
        match node {
            Node::Text(TextObj::Plain(s)) => {
                for tok in blank_tokens(s) {
                    match tok {
                        BlankTok::Raw(t) => {
                            cur.push(Node::Text(TextObj::Plain(t)));
                        }
                        BlankTok::Break => {
                            if !cur.is_empty() {
                                runs.push(std::mem::take(&mut cur));
                            }
                        }
                    }
                }
            }
            other => cur.push(other.clone()),
        }
    }
    if !cur.is_empty() {
        runs.push(cur);
    }
    runs
}

/// `[[div_]]` body rule: like [`render_block`] (block nodes standalone, inline
/// runs grouped into `<p>` at blank lines) except the first and last inline
/// runs are emitted unwrapped — Wikidot's `div_` quirk that leaves the rim text
/// bare. A body of only block elements (a sidebar of nested `[[div_]]` /
/// `----`) therefore renders as a flat sequence with no `<p>` at all.
fn render_block_div_(ctx: &RenderCtx, content: &Content) -> Vec<AnyView> {
    enum Unit {
        Block(AnyView),
        Inline(Vec<AnyView>),
    }
    let flush = |para: &mut Vec<Piece>, units: &mut Vec<Unit>| {
        let views = para_views(para);
        if !views.is_empty() {
            units.push(Unit::Inline(views));
        }
    };
    let mut units: Vec<Unit> = Vec::new();
    let mut para: Vec<Piece> = Vec::new();
    for node in content {
        if is_block(node) {
            flush(&mut para, &mut units);
            units.push(Unit::Block(render_node(ctx, node)));
        } else if let Node::Text(TextObj::Plain(t)) = node {
            for tok in para_tokens(t) {
                match tok {
                    ParaToken::Text(s) => para.push(Piece::Text(s)),
                    ParaToken::Break => flush(&mut para, &mut units),
                }
            }
        } else {
            para.push(Piece::Node(render_node(ctx, node)));
        }
    }
    flush(&mut para, &mut units);
    let last = units.len().saturating_sub(1);
    units
        .into_iter()
        .enumerate()
        .map(|(i, unit)| match unit {
            Unit::Block(view) => view,
            Unit::Inline(inner) => {
                if i > 0 && i < last {
                    view! { <p>{inner}</p> }.into_any()
                } else {
                    view! { <>{inner}</> }.into_any()
                }
            }
        })
        .collect()
}

fn is_block(node: &Node) -> bool {
    matches!(
        node,
        Node::Heading { .. }
            | Node::Table(_)
            | Node::BlockTable(_)
            | Node::BlockCell(_)
            | Node::Image { .. }
            | Node::HorizontalRule
            | Node::Clearfloat(_)
            | Node::Tabview { .. }
            | Node::FootnoteBlock(_)
            | Node::Container {
                kind: ContainerKind::Quote
                    | ContainerKind::Align(_)
                    | ContainerKind::Div { inline: false, .. }
                    | ContainerKind::IfTags { .. },
                ..
            }
            | Node::ListPages(_)
            | Node::Stylesheet(_)
            | Node::Include(_)
            | Node::Raw(_)
            | Node::Code { .. }
            | Node::ModuleBlock { .. }
            | Node::Module { .. }
            | Node::List(_)
            | Node::Collapsible { .. }
    )
}

fn render_node(ctx: &RenderCtx, node: &Node) -> AnyView {
    match node {
        Node::Text(t) => render_text_obj(t),
        Node::Raw(s) => view! { <span style="white-space: pre-wrap">{s.clone()}</span> }.into_any(),
        Node::Container { kind, content } => render_container(ctx, kind, content),
        Node::Heading {
            level,
            anchor,
            content,
        } => render_heading(ctx, *level, anchor.as_deref(), content),
        Node::AnchorTarget(name) => {
            use leptos::tachys::html::attribute::custom::custom_attribute;
            custom("a")
                .add_any_attr(custom_attribute("name", name.clone()))
                .child(Vec::<AnyView>::new())
                .into_any()
        }
        Node::Table(rows) => render_table(ctx, rows),
        Node::BlockTable(table) => render_grid_table(ctx, table),
        Node::BlockCell(cell) => view! { <>{render_inline(ctx, &cell.content)}</> }.into_any(),
        Node::Image {
            align,
            source,
            params,
        } => render_image(align, source, params),
        Node::Link {
            target,
            text,
            class,
        } => render_link(ctx, target, text, class.as_deref()),
        Node::SupSubscript { sup, sub } => view! {
            <>
                {(!sup.is_empty()).then(|| view! { <sup>{render_inline(ctx, sup)}</sup> })}
                {(!sub.is_empty()).then(|| view! { <sub>{render_inline(ctx, sub)}</sub> })}
            </>
        }
        .into_any(),
        Node::HorizontalRule => view! { <hr /> }.into_any(),
        Node::Stylesheet(css) => view! { <style>{css.clone()}</style> }.into_any(),
        Node::Footnote(_) | Node::FootnoteRef(_) => render_footnote_ref(node),
        Node::FootnoteBlock(bodies) => render_footnote_block(ctx, bodies),
        Node::Tabview { id, tabs } => render_tabview(ctx, *id, tabs),
        Node::ListPages(lp) => render_block_content(ctx, &lp.repeat).into_any(),
        Node::Include(_) => {
            view! { <span class="include-placeholder">"[include]"</span> }.into_any()
        }
        Node::Date { timestamp, format } => render_date(*timestamp, format.as_deref()),
        Node::IfExpr { then, .. } => view! { <>{render_inline(ctx, then)}</> }.into_any(),
        Node::Collapsible { header, body } => render_collapsible(ctx, header, body),
        Node::CollapsibleHeader { .. } => render_collapsible_header(ctx, node),
        Node::User { name, avatar } => render_user(name, *avatar),
        Node::Clearfloat(side) => {
            let clear = match side {
                ClearSide::Both => "both",
                ClearSide::Left => "left",
                ClearSide::Right => "right",
            };
            view! { <div style=format!("clear:{clear}; height: 0px; font-size: 1px")></div> }
                .into_any()
        }
        Node::Module { name, params } => match name.to_ascii_lowercase().as_str() {
            "newpage" => render_new_page_form(params),
            _ => empty_view(),
        },
        Node::ModuleBlock { name, params, body } => render_module_block(ctx, name, params, body),
        Node::Code { raw, .. } => view! {
            <div class="code"><pre><code>{raw.trim()}</code></pre></div>
        }
        .into_any(),
        Node::List(list) => render_list(ctx, list),
    }
}

fn render_text_obj(t: &TextObj) -> AnyView {
    match t {
        TextObj::Plain(s) => render_plain(s),
        TextObj::ModuleVar { name, default } => {
            let shown = default.clone().unwrap_or_else(|| format!("%%{name}%%"));
            view! { <span class="modulevar">{shown}</span> }.into_any()
        }
        TextObj::IncludeVar { name, default } => {
            let shown = default
                .as_ref()
                .and_then(|c| {
                    c.first().and_then(|n| match n {
                        Node::Text(TextObj::Plain(s)) => Some(s.clone()),
                        _ => None,
                    })
                })
                .unwrap_or_else(|| format!("{{${name}}}"));
            view! { <span class="includevar">{shown}</span> }.into_any()
        }
    }
}

/// A plain-text run: single newlines become `<br>` (soft line breaks).
fn render_plain(s: &str) -> AnyView {
    if s.contains('\n') {
        let parts: Vec<AnyView> = s
            .split('\n')
            .enumerate()
            .flat_map(|(i, seg)| {
                let mut v: Vec<AnyView> = Vec::new();
                if i > 0 {
                    v.push(view! { <br /> }.into_any());
                }
                if !seg.is_empty() {
                    v.push(view! { {seg.to_string()} }.into_any());
                }
                v
            })
            .collect();
        view! { <>{parts}</> }.into_any()
    } else {
        view! { {s.to_string()} }.into_any()
    }
}

fn render_container(ctx: &RenderCtx, kind: &ContainerKind, content: &Content) -> AnyView {
    match kind {
        ContainerKind::Style(TextStyle::Italic) => {
            view! { <em>{render_inline(ctx, content)}</em> }.into_any()
        }
        ContainerKind::Style(TextStyle::Bold) => {
            view! { <strong>{render_inline(ctx, content)}</strong> }.into_any()
        }
        ContainerKind::Style(TextStyle::Underline) => view! {
            <span style="text-decoration: underline">{render_inline(ctx, content)}</span>
        }
        .into_any(),
        ContainerKind::Style(TextStyle::Strikethrough) => view! {
            <span style="text-decoration: line-through">{render_inline(ctx, content)}</span>
        }
        .into_any(),
        ContainerKind::Tt => {
            let inner = render_inline(ctx, content);
            custom("tt").child(inner).into_any()
        }
        ContainerKind::Div {
            inline,
            block,
            params,
        } => {
            let (class, style) = params_to_class_style(params);
            let id = params_id(params);
            let class = (!class.is_empty()).then_some(class);
            if *block {
                let inner = render_block_content(ctx, content);
                return view! { <div id=id class=class style=style>{inner}</div> }.into_any();
            }
            if *inline {
                // `[[span]]`: inline body. (A span whose body crosses blank
                // lines is split into one `<p>` per run by [`render_block`] when
                // it sits in block context.)
                let inner = render_inline(ctx, content);
                return view! { <span id=id class=class style=style>{inner}</span> }.into_any();
            }
            // `[[div_]]`: a block `<div>` that suppresses Wikidot's
            // auto-paragraphing of its first and last inline run. Block
            // children always render standalone (never inside a `<p>`), so a
            // sidebar of nested `[[div_]]` / `----` renders as a flat sequence;
            // interior blank-line-separated inline runs still get `<p>`.
            let inner = render_block_div_(ctx, content);
            view! { <div id=id class=class style=style>{inner}</div> }.into_any()
        }
        ContainerKind::Color(c) => view! {
            <span style=format!("color: {c}")>{render_inline(ctx, content)}</span>
        }
        .into_any(),
        ContainerKind::Size(arg) => view! {
            <span style=format!("font-size:{}", normalize_size(arg))>
                {render_inline(ctx, content)}
            </span>
        }
        .into_any(),
        ContainerKind::Align(Align { side, .. }) => {
            let align = match side {
                AlignSide::Left => "left",
                AlignSide::Center => "center",
                AlignSide::Right => "right",
                AlignSide::Justify => "justify",
            };
            view! {
                <div style=format!("text-align: {align}")>{render_block_content(ctx, content)}</div>
            }
            .into_any()
        }
        ContainerKind::Quote => {
            view! { <blockquote>{render_block_content(ctx, content)}</blockquote> }.into_any()
        }
        // Tag gating is a server concern; render the body unconditionally.
        ContainerKind::IfTags { .. } => {
            view! { <>{render_block_content(ctx, content)}</> }.into_any()
        }
    }
}

fn render_heading(ctx: &RenderCtx, level: u32, anchor: Option<&str>, content: &Content) -> AnyView {
    let inner = view! { <span>{render_inline(ctx, content)}</span> };
    match level.min(6) {
        1 => view! { <h1 id=anchor>{inner}</h1> }.into_any(),
        2 => view! { <h2 id=anchor>{inner}</h2> }.into_any(),
        3 => view! { <h3 id=anchor>{inner}</h3> }.into_any(),
        4 => view! { <h4 id=anchor>{inner}</h4> }.into_any(),
        5 => view! { <h5 id=anchor>{inner}</h5> }.into_any(),
        _ => view! { <h6 id=anchor>{inner}</h6> }.into_any(),
    }
}

fn render_list(ctx: &RenderCtx, list: &List) -> AnyView {
    let items: Vec<AnyView> = list
        .items
        .iter()
        .map(|item| {
            let inner = render_inline(ctx, &item.content);
            let sub = item
                .sublist
                .as_ref()
                .map(|l| render_list(ctx, l))
                .unwrap_or_else(|| {
                    let _: () = view! { <></> };
                    ().into_any()
                });
            view! { <li>{inner}{sub}</li> }.into_any()
        })
        .collect();
    if list.ordered {
        view! { <ol>{items}</ol> }.into_any()
    } else {
        view! { <ul>{items}</ul> }.into_any()
    }
}

/// Wikidot's `nav:top` quirk: a top-level list item that opens a submenu has
/// its content wrapped in `<a href="javascript:;">` — the inert "button"
/// legacy themes target with `#top-bar li a`. Only the first level of each
/// list is wrapped: we descend through containers (the nav page keeps its
/// lists in `[[div class="top-bar"]]` / `mobile-top-bar`) but never into a
/// sublist, so nested entries keep their real links. Apply to the `nav:top`
/// content before [`render_block`].
pub fn wrap_topbar_lists(content: &mut Content) {
    for node in content {
        match node {
            Node::List(list) => {
                for item in &mut list.items {
                    if item.sublist.is_some() {
                        let text = std::mem::take(&mut item.content);
                        item.content = vec![Node::Link {
                            target: LinkTarget::Url("javascript:;".to_string()),
                            text,
                            class: None,
                        }];
                    }
                }
            }
            Node::Container { content, .. } => wrap_topbar_lists(content),
            _ => {}
        }
    }
}

fn render_table(ctx: &RenderCtx, rows: &[Vec<TableCell>]) -> AnyView {
    let rows_view: Vec<AnyView> = rows
        .iter()
        .map(|row| {
            let cells: Vec<AnyView> = row
                .iter()
                .map(|cell| {
                    let inner = render_inline(ctx, &cell.content);
                    let style = cell
                        .align
                        .map(|a| format!("text-align: {}", side_to_css(a.side)));
                    if cell.header {
                        view! { <th style=style>{inner}</th> }.into_any()
                    } else {
                        view! { <td colspan=cell.colspan style=style>{inner}</td> }.into_any()
                    }
                })
                .collect();
            view! { <tr>{cells}</tr> }.into_any()
        })
        .collect();
    view! {
        <table class="wiki-content-table">
            <tbody>{rows_view}</tbody>
        </table>
    }
    .into_any()
}

/// `[[table]]` / `[[row]]` / `[[cell]]` grid table. Cells are gathered from
/// each row's body — descending into `[[iftags]]` wrappers (kolorinko renders
/// every conditional branch) — so the mixed wrapped/bare layout real templates
/// use is handled uniformly. Cell content renders inline (no `<p>`). A
/// `<tbody>` is required: the HTML parser inserts one around bare `<tr>`s
/// regardless, and SSR output without it cannot hydrate.
fn render_grid_table(ctx: &RenderCtx, table: &BlockTable) -> AnyView {
    let (class, style) = params_to_class_style(&table.params);
    let rows: Vec<AnyView> = table
        .rows
        .iter()
        .map(|row| {
            let (rclass, rstyle) = params_to_class_style(&row.params);
            let cells: Vec<AnyView> = collect_grid_cells(&row.content)
                .iter()
                .map(|cell| render_grid_cell(ctx, cell))
                .collect();
            view! { <tr class=rclass style=rstyle>{cells}</tr> }.into_any()
        })
        .collect();
    view! {
        <table class=class style=style><tbody>{rows}</tbody></table>
    }
    .into_any()
}

/// Collect every [`Node::BlockCell`] in `content`, descending into `[[iftags]]`
/// wrappers so conditionally-included cells are emitted.
fn collect_grid_cells(content: &Content) -> Vec<&BlockCell> {
    let mut out = Vec::new();
    for node in content {
        match node {
            Node::BlockCell(c) => out.push(c),
            Node::Container {
                kind: ContainerKind::IfTags { .. },
                content,
            } => out.extend(collect_grid_cells(content)),
            _ => {}
        }
    }
    out
}

fn render_grid_cell(ctx: &RenderCtx, cell: &BlockCell) -> AnyView {
    let (class, style) = params_to_class_style(&cell.params);
    let inner = render_inline(ctx, trim_ws(&cell.content));
    if cell.header {
        view! { <th class=class style=style>{inner}</th> }.into_any()
    } else {
        view! { <td class=class style=style>{inner}</td> }.into_any()
    }
}

fn render_image(
    align: &Option<Align>,
    source: &[TextObj],
    params: &std::collections::HashMap<String, Vec<TextObj>>,
) -> AnyView {
    let src = text_objs_to_string(source);
    let alt = param_or(params, "alt", || filename_of(&src));
    let class = param_or(params, "class", || "image".to_string());
    let style = params
        .get("style")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty());
    let width = params
        .get("width")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty());
    let height = params
        .get("height")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty());
    let img = view! { <img class=class src=src alt=alt style=style width=width height=height /> };
    match align {
        // Alignment / float → wrap in the `image-container <floatclass>` div
        // Wikidot uses; the float is expressed via the container class.
        Some(a) => {
            let container = format!("image-container {}", image_container_class(a));
            view! { <div class=container>{img}</div> }.into_any()
        }
        // No alignment → a bare `<img>`; `link=` wraps it in an anchor.
        None => {
            let link = param_or(params, "link", String::new);
            if link.is_empty() {
                img.into_any()
            } else {
                view! { <a href=link>{img}</a> }.into_any()
            }
        }
    }
}

/// Resolve `params[key]` to a string, falling back to `default` when absent or
/// blank (so an explicit empty `alt=""` still yields the filename-derived alt).
fn param_or(
    params: &std::collections::HashMap<String, Vec<TextObj>>,
    key: &str,
    default: impl FnOnce() -> String,
) -> String {
    params
        .get(key)
        .map(|v| text_objs_to_string(v))
        .filter(|v| !v.is_empty())
        .unwrap_or_else(default)
}

/// Last path segment of a URL — Wikidot's default image `alt`.
fn filename_of(url: &str) -> String {
    url.rsplit('/').next().unwrap_or(url).to_string()
}

fn render_link(
    ctx: &RenderCtx,
    target: &LinkTarget,
    text: &Content,
    class: Option<&str>,
) -> AnyView {
    // Internal hrefs are always built full-weight — then simplified against
    // the scope's default space (a no-op on the server, whose `default` is
    // `None`). External URLs pass through `simplify` verbatim: it only
    // strips a leading `/{default}` segment.
    let href = kolorinko_rt::simplify(
        ctx.scope.default,
        &match target {
            LinkTarget::Url(u) => u.clone(),
            // Still carries unresolved variable slots (no listed page in scope):
            // flatten with the same default / verbatim `%%name%%` fallback as any
            // other text run and use it as the href.
            LinkTarget::Unresolved(objs) => text_objs_to_string(objs),
            // A link resolution looked this up and found it: build the titled
            // canonical route straight from the target's rename-stable identity —
            // the form the client router intercepts, so navigation stays in the
            // app.
            LinkTarget::Canonical { page_id, title } => LocalId::from_page_id(page_id).map_or_else(
                || "/".to_string(),
                |l| format_page_route(ctx.scope.space, l, title),
            ),
            // A page ref, resolved or not, renders the slug-family route — the
            // canonical `cat:name` form (the colon can't collide with a base64url
            // local id, so the server routes these by slug and 301s to the titled
            // canonical address). Without a space (a context-less render) the
            // href falls back to root-relative.
            LinkTarget::Page(p) | LinkTarget::Missing(p) => {
                let rest = p.path.join("/");
                match ctx.scope.space {
                    Some(space) => match &p.space {
                        Some(cat) => format!("/{space}/{cat}:{rest}"),
                        None => format!("/{space}/{rest}"),
                    },
                    None => match &p.space {
                        Some(cat) => format!("/{cat}/{rest}"),
                        None => format!("/{rest}"),
                    },
                }
            }
        },
    );
    let inner = render_inline(ctx, text);
    // `class` omitted entirely when absent — SSR would otherwise emit a
    // spurious `class=""`. A `Missing` target is a page the site doesn't
    // have, so it always carries `newpage` (appended to any author-supplied
    // class): `a.newpage` is the red-link style the base theme already ships.
    let class = match target {
        LinkTarget::Missing(_) => match class {
            Some(c) => Some(format!("{c} newpage")),
            None => Some("newpage".to_string()),
        },
        _ => class.map(str::to_string),
    };
    match class {
        Some(c) => view! { <a class=c href=href>{inner}</a> }.into_any(),
        None => view! { <a href=href>{inner}</a> }.into_any(),
    }
}

/// `[[user name]]` / `[[*user name]]` → Wikidot's `printuser` span. The
/// export carries no user ids, so the avatar image and the `onclick`
/// handlers of the live site are not reproduced.
fn render_user(name: &str, avatar: bool) -> AnyView {
    let unix = unix_name(name);
    let href = format!("http://www.wikidot.com/user:info/{unix}");
    let class = if avatar {
        "printuser avatarhover"
    } else {
        "printuser"
    };
    view! {
        <span class=class><a href=href>{name.to_string()}</a></span>
    }
    .into_any()
}

/// Wikidot's `toUnixName`: lowercase, spaces and underscores to dashes.
fn unix_name(name: &str) -> String {
    name.to_lowercase()
        .chars()
        .map(|c| match c {
            ' ' | '_' => '-',
            other => other,
        })
        .collect()
}

/// A footnote reference (`[[footnote]]`, or the numbered ref the assembly
/// pass left behind).
fn render_footnote_ref(node: &Node) -> AnyView {
    let n = match node {
        Node::FootnoteRef(n) => *n,
        _ => 1,
    };
    view! {
        <sup class="footnoteref">
            <a id=format!("footnoteref-{n}") href="javascript:;" class="footnoteref">
                {n.to_string()}
            </a>
        </sup>
    }
    .into_any()
}

/// The collected footnote bodies, at `[[footnoteblock]]` (or the page foot).
fn render_footnote_block(ctx: &RenderCtx, bodies: &[Content]) -> AnyView {
    if bodies.is_empty() {
        return empty_view();
    }
    let items: Vec<AnyView> = bodies
        .iter()
        .enumerate()
        .map(|(i, body)| {
            let n = i + 1;
            view! {
                <div class="footnote-footer" id=format!("footnote-{n}")>
                    <a href="javascript:;">{n.to_string()}</a>
                    {". "}{render_inline(ctx, body)}
                </div>
            }
            .into_any()
        })
        .collect();
    view! {
        <div class="footnotes-footer">
            <div class="title">"Footnotes"</div>
            {items}
        </div>
    }
    .into_any()
}

/// `[[module NewPage …]]` → Wikidot's new-page form.
fn render_module_block(
    ctx: &RenderCtx,
    name: &str,
    _params: &std::collections::HashMap<String, Vec<TextObj>>,
    body: &Content,
) -> AnyView {
    match name.to_ascii_lowercase().as_str() {
        // No forum data exists in the export; render the structural shell
        // with a visible disclaimer so the empty box is self-explanatory.
        "frontforum" => view! {
            <div class="front-forum-box">
                <div class="body-panel" style="text-align:center; color:#888; padding:1em">
                    "No forum data available in this archive."
                </div>
            </div>
        }
        .into_any(),
        // Body-capable modules without data: render nothing rather than a
        // template full of unresolved `%%var%%` slots.
        _ => view! { <>{render_block_content(ctx, body)}</> }.into_any(),
    }
}

fn render_new_page_form(params: &std::collections::HashMap<String, Vec<TextObj>>) -> AnyView {
    let size = params
        .get("size")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "30".into());
    let button = params
        .get("button")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "create page".into());
    let category = params
        .get("category")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty());
    let tags = params
        .get("tags")
        .map(|v| text_objs_to_string(v))
        .filter(|s| !s.is_empty());
    view! {
        <div class="new-page-box" style="text-align: center; margin: 1em 0">
            <form action="dummy.html" method="get"
                onsubmit="WIKIDOT.modules.NewPageHelperModule.listeners.create(event);">
                <input class="text" name="pageName" type="text" size=size maxlength="128" style="margin: 1px" disabled />
                <input type="submit" class="button" value=button style="margin: 1px;" disabled />
                {category.map(|c| view! { <input type="hidden" name="categoryName" value=c /> }.into_any())}
                {tags.map(|t| view! { <input type="hidden" name="tags" value=t /> }.into_any())}
            </form>
        </div>
    }
    .into_any()
}

/// `[[collapsible]]` → Wikidot's folded/unfolded two-part block with a
/// client-side toggle. The header subtree — the inline formatting context
/// around the opener — is walked twice, once per toggle link (the
/// instruction in [`RenderCtx::link`] telling the
/// [`Node::CollapsibleHeader`] leaf which label to show and which way to
/// flip the signal), which is how Wikidot duplicates the active `[[size]]`
/// / `[[span]]` around both links — the idiom
/// `[[size 120%]][[collapsible …]][[/size]]` relies on.
fn render_collapsible(ctx: &RenderCtx, header: &Content, body: &Content) -> AnyView {
    let unfolded = RwSignal::new(!header_folded(header));
    let folded_view = render_inline(
        &ctx.with_link(CollapsibleLinkCtx {
            unfolded,
            open: true,
        }),
        header,
    );
    let unfolded_view = render_inline(
        &ctx.with_link(CollapsibleLinkCtx {
            unfolded,
            open: false,
        }),
        header,
    );
    view! {
        <div class="collapsible-block">
            <div class="collapsible-block-folded" style=move || unfolded.get().then_some("display:none")>
                {folded_view}
            </div>
            <div class="collapsible-block-unfolded" style=move || (!unfolded.get()).then_some("display:none")>
                <div class="collapsible-block-unfolded-link">{unfolded_view}</div>
                <div class="collapsible-block-content">{render_block_content(ctx, body)}</div>
            </div>
        </div>
    }
    .into_any()
}

/// The header leaf's initial `folded` state (the leaf itself is rendered by
/// [`render_collapsible_header`]).
fn header_folded(header: &[Node]) -> bool {
    header
        .iter()
        .find_map(|node| match node {
            Node::CollapsibleHeader { folded, .. } => Some(*folded),
            _ => {
                let mut found = None;
                node.visit_node(&mut |children| {
                    found = found.or_else(|| header_folded_search(children))
                });
                found
            }
        })
        .unwrap_or(true)
}

fn header_folded_search(nodes: &[Node]) -> Option<bool> {
    nodes.iter().find_map(|node| match node {
        Node::CollapsibleHeader { folded, .. } => Some(*folded),
        _ => {
            let mut found = None;
            node.visit_node(&mut |children| {
                found = found.or_else(|| header_folded_search(children))
            });
            found
        }
    })
}

/// The toggle-link leaf: which label to show (spaces → `&nbsp;`, as
/// Wikidot keeps the labels from wrapping) and which way to flip the signal
/// come from [`RenderCtx::link`]. Outside a collapsible it never renders —
/// the parse pass degrades unpaired headers to raw text.
fn render_collapsible_header(ctx: &RenderCtx, node: &Node) -> AnyView {
    let Node::CollapsibleHeader { open, close, .. } = node else {
        unreachable!()
    };
    let Some(CollapsibleLinkCtx {
        unfolded,
        open: is_open,
    }) = ctx.link
    else {
        return empty_view();
    };
    let (label, unfold) = if is_open {
        (open, true)
    } else {
        (close, false)
    };
    view! {
        <a class="collapsible-block-link" href="javascript:;"
            on:click=move |_| unfolded.set(unfold)>{nbsp(label)}</a>
    }
    .into_any()
}

/// Wikidot renders collapsible show/hide labels with every space turned into
/// a non-breaking space (they must not wrap).
fn nbsp(s: &str) -> String {
    s.replace(' ', "\u{a0}")
}

/// `[[tabview]]` → the YUI DOM skeleton (`.yui-navset`) with a client-side
/// tab switch; the first tab renders selected.
fn render_tabview(ctx: &RenderCtx, id: u32, tabs: &[kolorinko_wikitext::Tab]) -> AnyView {
    if tabs.is_empty() {
        return view! { <div class="yui-navset"></div> }.into_any();
    }
    let selected = RwSignal::new(0usize);
    let nav: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let name = render_inline(ctx, &tab.name);
            view! {
                <li class=move || (selected.get() == i).then_some("selected")>
                    <a href="javascript:;" on:click=move |_| selected.set(i)>
                        <em>{name}</em>
                    </a>
                </li>
            }
            .into_any()
        })
        .collect();
    let panels: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let body = render_block_content(ctx, &tab.content);
            view! {
                <div id=format!("wiki-tab-{id}-{i}")
                    style=move || (selected.get() != i).then_some("display:none")>
                    {body}
                </div>
            }
            .into_any()
        })
        .collect();
    view! {
        <div id=format!("wiki-tabview-{id}") class="yui-navset">
            <ul class="yui-nav">{nav}</ul>
            <div class="yui-content">{panels}</div>
        </div>
    }
    .into_any()
}

fn params_to_class_style(
    params: &std::collections::HashMap<String, Vec<TextObj>>,
) -> (String, Option<String>) {
    let class = params
        .get("class")
        .map(|v| text_objs_to_string(v))
        .unwrap_or_default();
    let mut style = params
        .get("style")
        .map(|v| text_objs_to_string(v))
        .unwrap_or_default();
    // Collapse a stray trailing `;` so appending extra declarations never
    // produces a `;;` (Wikidot's golden output always joins with a single `;`).
    if style.ends_with(';') {
        style.pop();
    }
    for (k, v) in params {
        if matches!(k.as_str(), "class" | "style" | "id") {
            continue;
        }
        if !style.is_empty() {
            style.push_str("; ");
        }
        style.push_str(k);
        style.push(':');
        style.push(' ');
        style.push_str(&text_objs_to_string(v));
    }
    // tachys appends a trailing `;` when serializing a `style=` attribute,
    // so the value we hand it must not already end with one.
    while style.ends_with(';') {
        style.pop();
    }
    (class, (!style.is_empty()).then_some(style))
}

/// `[[div id="X" …]]` → Wikidot's `u-`-prefixed element id (a namespace the
/// site's own CSS cannot collide with).
fn params_id(params: &std::collections::HashMap<String, Vec<TextObj>>) -> Option<String> {
    params
        .get("id")
        .map(|v| {
            let raw = text_objs_to_string(v);
            if raw.starts_with("u-") {
                raw
            } else {
                format!("u-{raw}")
            }
        })
        .filter(|s| s != "u-")
}

pub(crate) fn text_objs_to_string(objs: &[TextObj]) -> String {
    let mut out = String::new();
    for o in objs {
        match o {
            TextObj::Plain(s) => out.push_str(s),
            TextObj::ModuleVar { name, default } => {
                out.push_str(
                    default
                        .clone()
                        .unwrap_or_else(|| format!("%%{name}%%"))
                        .as_str(),
                );
            }
            TextObj::IncludeVar { name, default } => {
                if let Some(d) = default
                    && let Some(Node::Text(TextObj::Plain(s))) = d.first()
                {
                    out.push_str(s);
                } else {
                    out.push_str(&format!("{{${name}}}"));
                }
            }
        }
    }
    out
}

fn image_container_class(a: &Align) -> String {
    match (a.floating, a.side) {
        (true, AlignSide::Left) => "floatleft".into(),
        (true, AlignSide::Right) => "floatright".into(),
        (false, AlignSide::Left) => "alignleft".into(),
        (false, AlignSide::Right) => "alignright".into(),
        _ => "aligncenter".into(),
    }
}

fn side_to_css(s: AlignSide) -> &'static str {
    match s {
        AlignSide::Left => "left",
        AlignSide::Center => "center",
        AlignSide::Right => "right",
        AlignSide::Justify => "justify",
    }
}

fn normalize_size(arg: &str) -> String {
    let arg = arg.trim();
    if arg.chars().all(|c| c.is_ascii_digit() || c == '.') && !arg.is_empty() {
        return format!("{arg}em");
    }
    if arg.ends_with('%') || arg.ends_with("em") || arg.ends_with("px") {
        return arg.into();
    }
    arg.into()
}

/// Render a concrete date (`%%created_at|format%%` bound by a ListPages
/// instantiation) as Wikidot's `odate` span: the human-readable text formatted
/// per the strftime-ish `format` (default `%d %b %Y %H:%M`), with `time_`/
/// `format_` classes carrying the raw data for user scripts.
fn render_date(timestamp: i64, format: Option<&str>) -> AnyView {
    let mut class = format!("odate time_{timestamp}");
    if let Some(f) = format {
        class.push_str(" format_");
        class.push_str(&urlencode_component(f));
    }
    // The displayed text always uses the default format; the custom format
    // lives only in the `format_…` class for user scripts.
    let shown = format_date(timestamp, None);
    view! { <span class=class>{shown}</span> }.into_any()
}

/// Percent-encode a `format_` class fragment the way Wikidot does: keep
/// URI-unreserved characters, escape everything else (notably `%` → `%25`).
fn urlencode_component(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Format a Unix timestamp (UTC) with the strftime directives Wikidot date
/// variables accept. Unrecognized directives are kept verbatim.
fn format_date(ts: i64, fmt: Option<&str>) -> String {
    let fmt = fmt.unwrap_or("%d %b %Y %H:%M");
    let days = ts.div_euclid(86_400);
    let secs = ts.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let weekday = (days.rem_euclid(7) + 4) % 7; // 1970-01-01 was a Thursday
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    let yday = days - days_from_civil(y, 1, 1) + 1;
    let mut out = String::with_capacity(fmt.len());
    let mut dirs = fmt.chars().peekable();
    while let Some(c) = dirs.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match dirs.next() {
            Some('Y') => out.push_str(&y.to_string()),
            Some('y') => out.push_str(&format!("{:02}", y.rem_euclid(100))),
            Some('m') => out.push_str(&format!("{m:02}")),
            Some('d') => out.push_str(&format!("{d:02}")),
            Some('e') => out.push_str(&format!("{d:2}")),
            Some('H') => out.push_str(&format!("{hh:02}")),
            Some('I') => out.push_str(&format!("{:02}", hh % 12)),
            Some('M') => out.push_str(&format!("{mm:02}")),
            Some('S') => out.push_str(&format!("{ss:02}")),
            Some('p') => out.push_str(if hh < 12 { "AM" } else { "PM" }),
            Some('a') => out.push_str(WEEKDAYS[weekday as usize]),
            Some('A') => out.push_str(match weekday {
                0 => "Sunday",
                1 => "Monday",
                2 => "Tuesday",
                3 => "Wednesday",
                4 => "Thursday",
                5 => "Friday",
                _ => "Saturday",
            }),
            Some('b') | Some('h') => out.push_str(MONTHS[(m - 1) as usize]),
            Some('B') => out.push_str(match m {
                1 => "January",
                2 => "February",
                3 => "March",
                4 => "April",
                5 => "May",
                6 => "June",
                7 => "July",
                8 => "August",
                9 => "September",
                10 => "October",
                11 => "November",
                _ => "December",
            }),
            Some('j') => out.push_str(&format!("{yday:03}")),
            Some('Z') => out.push_str("UTC"),
            Some('%') => out.push('%'),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}
