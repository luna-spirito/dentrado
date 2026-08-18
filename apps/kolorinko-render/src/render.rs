//! Render a parsed Wikidot page ([`kolorinko_wikitext::Content`]) into Leptos
//! views whose DOM matches Wikidot's output closely enough that legacy
//! user-authored CSS themes continue to apply.
//!
//! `site` is threaded through so internal links resolve to
//! `/<site>/<category?>/<page>` — the path-prefix scheme this mirror uses
//! instead of Wikidot's per-site subdomains.

use kolorinko_wikitext::{
    Align, AlignSide, BlockCell, BlockTable, ContainerKind, Content, LinkTarget, List, Node,
    TableCell, TextObj, TextStyle, civil_from_days, days_from_civil,
};
use leptos::prelude::*;

pub(crate) fn render_inline(site: &str, content: &[Node]) -> Vec<AnyView> {
    content.iter().map(|n| render_node(site, n)).collect()
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
/// inside their paragraph as soft breaks (`<br>`).
pub fn render_block(site: &str, content: &Content) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(content.len());
    let mut para: Vec<AnyView> = Vec::new();
    for node in content {
        if is_block(node) {
            flush(&mut para, &mut out);
            out.push(render_node(site, node));
        } else if let Node::Text(TextObj::Plain(t)) = node {
            for tok in para_tokens(t) {
                match tok {
                    ParaToken::Text(s) => para.push(render_plain(&s)),
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
                    let inner = render_inline(site, &run);
                    out.push(
                        view! { <p><span class=class.clone() style=style.clone()>{inner}</span></p> }
                            .into_any(),
                    );
                }
            } else {
                para.push(render_node(site, node));
            }
        } else {
            para.push(render_node(site, node));
        }
    }
    flush(&mut para, &mut out);
    out
}

/// Split a text run into paragraph tokens: a blank line — two or more newlines
/// separated only by spaces/tabs — is a [`ParaToken::Break`] (paragraph
/// boundary); everything else is a text segment whose single newlines remain
/// as soft breaks. Segment edges are trimmed to match Wikidot's clean `<p>`s.
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
                    toks.push(ParaToken::Text(s[start..i].trim().to_string()));
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
        let rest = s[start..].trim();
        if !rest.is_empty() {
            toks.push(ParaToken::Text(rest.to_string()));
        }
    }
    toks
}

fn flush(para: &mut Vec<AnyView>, out: &mut Vec<AnyView>) {
    if !para.is_empty() {
        let p = std::mem::take(para);
        out.push(view! { <p>{p}</p> }.into_any());
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
fn render_block_div_(site: &str, content: &Content) -> Vec<AnyView> {
    enum Unit {
        Block(AnyView),
        Inline(Vec<AnyView>),
    }
    let mut units: Vec<Unit> = Vec::new();
    let mut para: Vec<AnyView> = Vec::new();
    for node in content {
        if is_block(node) {
            if !para.is_empty() {
                units.push(Unit::Inline(std::mem::take(&mut para)));
            }
            units.push(Unit::Block(render_node(site, node)));
        } else if let Node::Text(TextObj::Plain(t)) = node {
            for tok in para_tokens(t) {
                match tok {
                    ParaToken::Text(s) => para.push(render_plain(&s)),
                    ParaToken::Break => {
                        if !para.is_empty() {
                            units.push(Unit::Inline(std::mem::take(&mut para)));
                        }
                    }
                }
            }
        } else {
            para.push(render_node(site, node));
        }
    }
    if !para.is_empty() {
        units.push(Unit::Inline(std::mem::take(&mut para)));
    }
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
            | Node::Tabview(_)
            | Node::Footnote(_)
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
            | Node::Code(_)
            | Node::List(_)
    )
}

fn render_node(site: &str, node: &Node) -> AnyView {
    match node {
        Node::Text(t) => render_text_obj(t),
        Node::Raw(s) => view! { <span style="white-space: pre-wrap">{s.clone()}</span> }.into_any(),
        Node::Container { kind, content } => render_container(site, kind, content),
        Node::Heading { level, content } => render_heading(site, *level, content),
        Node::Table(rows) => render_table(site, rows),
        Node::BlockTable(table) => render_grid_table(site, table),
        Node::BlockCell(cell) => view! { <>{render_inline(site, &cell.content)}</> }.into_any(),
        Node::Image {
            align,
            source,
            params,
        } => render_image(align, source, params),
        Node::Link { target, text } => render_link(site, target, text),
        Node::SupSubscript { sup, sub } => view! {
            <>
                {(!sup.is_empty()).then(|| view! { <sup>{render_inline(site, sup)}</sup> })}
                {(!sub.is_empty()).then(|| view! { <sub>{render_inline(site, sub)}</sub> })}
            </>
        }
        .into_any(),
        Node::HorizontalRule => view! { <hr /> }.into_any(),
        Node::Stylesheet(css) => view! { <style>{css.clone()}</style> }.into_any(),
        Node::Footnote(content) => view! {
            <sup class="footnoteref">{render_inline(site, content)}</sup>
        }
        .into_any(),
        Node::Tabview(tabs) => render_tabview(site, tabs),
        Node::ListPages(lp) => render_block(site, &lp.repeat).into_any(),
        Node::Include(_) => {
            view! { <span class="include-placeholder">"[include]"</span> }.into_any()
        }
        Node::Date { timestamp, format } => render_date(*timestamp, format.as_deref()),
        Node::Module(_) => {
            let _: () = view! { <></> };
            ().into_any()
        }
        Node::Code(s) => view! {
            <div class="code"><pre><code>{s.clone()}</code></pre></div>
        }
        .into_any(),
        Node::List(list) => render_list(site, list),
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
                v.push(view! { {seg.to_string()} }.into_any());
                v
            })
            .collect();
        view! { <>{parts}</> }.into_any()
    } else {
        view! { {s.to_string()} }.into_any()
    }
}

fn render_container(site: &str, kind: &ContainerKind, content: &Content) -> AnyView {
    match kind {
        ContainerKind::Style(TextStyle::Italic) => {
            view! { <em>{render_inline(site, content)}</em> }.into_any()
        }
        ContainerKind::Style(TextStyle::Bold) => {
            view! { <strong>{render_inline(site, content)}</strong> }.into_any()
        }
        ContainerKind::Style(TextStyle::Underline) => view! {
            <span style="text-decoration: underline">{render_inline(site, content)}</span>
        }
        .into_any(),
        ContainerKind::Style(TextStyle::Strikethrough) => view! {
            <span style="text-decoration: line-through">{render_inline(site, content)}</span>
        }
        .into_any(),
        ContainerKind::Div {
            inline,
            block,
            params,
        } => {
            let (class, style) = params_to_class_style(params);
            if *block {
                let inner = render_block(site, content);
                return view! { <div class=class style=style>{inner}</div> }.into_any();
            }
            if *inline {
                // `[[span]]`: inline body. (A span whose body crosses blank
                // lines is split into one `<p>` per run by [`render_block`] when
                // it sits in block context.)
                let inner = render_inline(site, content);
                return view! { <span class=class style=style>{inner}</span> }.into_any();
            }
            // `[[div_]]`: a block `<div>` that suppresses Wikidot's
            // auto-paragraphing of its first and last inline run. Block
            // children always render standalone (never inside a `<p>`), so a
            // sidebar of nested `[[div_]]` / `----` renders as a flat sequence;
            // interior blank-line-separated inline runs still get `<p>`.
            let inner = render_block_div_(site, content);
            view! { <div class=class style=style>{inner}</div> }.into_any()
        }
        ContainerKind::Color(c) => view! {
            <span style=format!("color: {c}")>{render_inline(site, content)}</span>
        }
        .into_any(),
        ContainerKind::Size(arg) => view! {
            <span style=format!("font-size: {}", normalize_size(arg))>
                {render_inline(site, content)}
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
                <div style=format!("text-align: {align}")>{render_block(site, content)}</div>
            }
            .into_any()
        }
        ContainerKind::Quote => {
            view! { <blockquote>{render_block(site, content)}</blockquote> }.into_any()
        }
        // Tag gating is a server concern; render the body unconditionally.
        ContainerKind::IfTags { .. } => view! { <>{render_block(site, content)}</> }.into_any(),
    }
}

fn render_heading(site: &str, level: u32, content: &Content) -> AnyView {
    let inner = render_inline(site, content);
    match level.min(6) {
        1 => view! { <h1>{inner}</h1> }.into_any(),
        2 => view! { <h2>{inner}</h2> }.into_any(),
        3 => view! { <h3>{inner}</h3> }.into_any(),
        4 => view! { <h4>{inner}</h4> }.into_any(),
        5 => view! { <h5>{inner}</h5> }.into_any(),
        _ => view! { <h6>{inner}</h6> }.into_any(),
    }
}

fn render_list(site: &str, list: &List) -> AnyView {
    let items: Vec<AnyView> = list
        .items
        .iter()
        .map(|item| {
            let inner = render_inline(site, &item.content);
            let sub = item
                .sublist
                .as_ref()
                .map(|l| render_list(site, l))
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
                        }];
                    }
                }
            }
            Node::Container { content, .. } => wrap_topbar_lists(content),
            _ => {}
        }
    }
}

fn render_table(site: &str, rows: &[Vec<TableCell>]) -> AnyView {
    let rows_view: Vec<AnyView> = rows
        .iter()
        .map(|row| {
            let cells: Vec<AnyView> = row
                .iter()
                .map(|cell| {
                    let inner = render_inline(site, &cell.content);
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
/// use is handled uniformly. Cell content renders inline (no `<p>`), and no
/// `<tbody>` is emitted, matching Wikidot.
fn render_grid_table(site: &str, table: &BlockTable) -> AnyView {
    let (class, style) = params_to_class_style(&table.params);
    let rows: Vec<AnyView> = table
        .rows
        .iter()
        .map(|row| {
            let (rclass, rstyle) = params_to_class_style(&row.params);
            let cells: Vec<AnyView> = collect_grid_cells(&row.content)
                .iter()
                .map(|cell| render_grid_cell(site, cell))
                .collect();
            view! { <tr class=rclass style=rstyle>{cells}</tr> }.into_any()
        })
        .collect();
    view! {
        <table class=class style=style>{rows}</table>
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

fn render_grid_cell(site: &str, cell: &BlockCell) -> AnyView {
    let (class, style) = params_to_class_style(&cell.params);
    let inner = render_inline(site, trim_ws(&cell.content));
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

fn render_link(site: &str, target: &LinkTarget, text: &Content) -> AnyView {
    let href = match target {
        LinkTarget::Url(u) => u.clone(),
        // Still carries unresolved variable slots (no listed page in scope):
        // flatten with the same default / verbatim `%%name%%` fallback as any
        // other text run and use it as the href.
        LinkTarget::Unresolved(objs) => text_objs_to_string(objs),
        LinkTarget::Page(p) => {
            let rest = p.path.join("/");
            match &p.space {
                Some(cat) => format!("/{site}/{cat}/{rest}"),
                None => format!("/{site}/{rest}"),
            }
        }
    };
    let inner = render_inline(site, text);
    view! { <a href=href>{inner}</a> }.into_any()
}

/// `[[tabview]]` → the YUI DOM skeleton (`.yui-navset`), first tab shown.
fn render_tabview(site: &str, tabs: &[kolorinko_wikitext::Tab]) -> AnyView {
    if tabs.is_empty() {
        return view! { <div class="yui-navset"></div> }.into_any();
    }
    let nav: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let name = render_inline(site, &tab.name);
            let class = if i == 0 { Some("selected") } else { None };
            view! { <li class=class><em>{name}</em></li> }.into_any()
        })
        .collect();
    let panels: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let body = render_block(site, &tab.content);
            let style = if i == 0 { None } else { Some("display:none") };
            view! { <div style=style>{body}</div> }.into_any()
        })
        .collect();
    view! {
        <div class="yui-navset yui-navset-top">
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
    for (k, v) in params {
        if matches!(k.as_str(), "class" | "style" | "id") {
            continue;
        }
        if !style.is_empty() {
            style.push(';');
        }
        style.push_str(k);
        style.push(':');
        style.push_str(&text_objs_to_string(v));
    }
    (class, (!style.is_empty()).then_some(style))
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
    let shown = format_date(timestamp, format);
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
    let fmt = fmt.unwrap_or("%d %b %Y %H:%M UTC");
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
