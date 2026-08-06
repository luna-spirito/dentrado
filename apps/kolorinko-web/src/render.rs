//! Render a parsed Wikidot page ([`kolorinko_wikitext::Content`]) into Leptos
//! views whose DOM matches Wikidot's output closely enough that legacy
//! user-authored CSS themes continue to apply.
//!
//! `site` is threaded through so internal links resolve to
//! `/<site>/<category?>/<page>` — the path-prefix scheme this mirror uses
//! instead of Wikidot's per-site subdomains.

use kolorinko_wikitext::{
    Align, AlignSide, BlockCell, BlockTable, ContainerKind, Content, LinkTarget, Node, TableCell,
    TextStyle, TextObj,
};
use leptos::prelude::*;

pub(crate) fn render_inline(site: &str, content: &Content) -> Vec<AnyView> {
    content.iter().map(|n| render_node(site, n)).collect()
}

/// Render `#page-content`: top-level inline runs are grouped into `<p>`,
/// block nodes render standalone — exactly as Wikidot's renderer does.
pub(crate) fn render_block(site: &str, content: &Content) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(content.len());
    let mut para: Vec<AnyView> = Vec::new();
    for node in content {
        if is_block(node) {
            flush(&mut para, &mut out);
            out.push(render_node(site, node));
        } else if let Node::Text(TextObj::Plain(t)) = node
            && t.trim().is_empty()
            && t.contains('\n')
        {
            flush(&mut para, &mut out);
        } else {
            para.push(render_node(site, node));
        }
    }
    flush(&mut para, &mut out);
    out
}

fn flush(para: &mut Vec<AnyView>, out: &mut Vec<AnyView>) {
    if !para.is_empty() {
        let p = std::mem::take(para);
        out.push(view! { <p>{p}</p> }.into_any());
    }
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
        Node::BlockCell(cell) => {
            view! { <>{render_inline(site, &cell.content)}</> }.into_any()
        }
        Node::Image { align, source, params } => render_image(align, source, params),
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
        Node::Include(_) => view! { <span class="include-placeholder">"[include]"</span> }.into_any(),
        Node::Date { timestamp, .. } => {
            view! { <span class="odate">{format!("#{timestamp}")}</span> }.into_any()
        }
    }
}

fn render_text_obj(t: &TextObj) -> AnyView {
    match t {
        TextObj::Plain(s) => {
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
                view! { {s.clone()} }.into_any()
            }
        }
        TextObj::ModuleVar { name, default } => {
            let shown = default.clone().unwrap_or_else(|| format!("%%{name}%%"));
            view! { <span class="modulevar">{shown}</span> }.into_any()
        }
        TextObj::IncludeVar { name, default } => {
            let shown = default
                .as_ref()
                .and_then(|c| c.first().and_then(|n| match n {
                    Node::Text(TextObj::Plain(s)) => Some(s.clone()),
                    _ => None,
                }))
                .unwrap_or_else(|| format!("{{${name}}}"));
            view! { <span class="includevar">{shown}</span> }.into_any()
        }
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
        ContainerKind::Div { inline, params } => {
            let (class, style) = params_to_class_style(params);
            let inner = render_block(site, content);
            if *inline {
                view! { <span class=class style=style>{inner}</span> }.into_any()
            } else {
                view! { <div class=class style=style>{inner}</div> }.into_any()
            }
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
        ContainerKind::Quote => view! { <blockquote>{render_block(site, content)}</blockquote> }.into_any(),
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
    let inner = render_inline(site, &cell.content);
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
    let mut classes = vec!["image-container".to_string()];
    let mut img_style = String::new();
    if let Some(a) = align {
        classes.push(image_container_class(a));
        img_style.push_str(&format!("float: {};", side_to_float(a.side)));
    }
    let src = text_objs_to_string(source);
    let alt = params
        .get("alt")
        .map(|v| text_objs_to_string(v))
        .unwrap_or_default();
    view! {
        <div class=classes.join(" ")>
            <img class="image" src=src alt=alt style=img_style />
        </div>
    }
    .into_any()
}

fn render_link(site: &str, target: &LinkTarget, text: &Content) -> AnyView {
    let href = match target {
        LinkTarget::Url(u) => u.clone(),
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

fn params_to_class_style(params: &std::collections::HashMap<String, Vec<TextObj>>) -> (String, String) {
    let class = params.get("class").map(|v| text_objs_to_string(v)).unwrap_or_default();
    let mut style = params.get("style").map(|v| text_objs_to_string(v)).unwrap_or_default();
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
    (class, style)
}

pub(crate) fn text_objs_to_string(objs: &[TextObj]) -> String {
    let mut out = String::new();
    for o in objs {
        match o {
            TextObj::Plain(s) => out.push_str(s),
            TextObj::ModuleVar { name, default } => {
                out.push_str(default.clone().unwrap_or_else(|| format!("%%{name}%%")).as_str());
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

fn side_to_float(s: AlignSide) -> &'static str {
    match s {
        AlignSide::Left => "left",
        AlignSide::Right => "right",
        _ => "none",
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
