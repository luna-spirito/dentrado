//! Render a parsed Wikidot page ([`kolorinko_wikitext::Content`]) into Leptos
//! views whose DOM matches Wikidot's output closely enough that legacy
//! user-authored CSS themes continue to apply.
//!
//! The contract we aim for (the "generals"):
//!
//! | AST node                 | DOM emitted                                             |
//! | ------------------------ | ------------------------------------------------------- |
//! | inline run               | grouped into `<p>` per blank-line-separated paragraph  |
//! | `Container::Style(Italic)` | `<em>`                                                |
//! | `Container::Style(Bold)`   | `<strong>`                                            |
//! | `Container::Style(Underline)` | `<span style="text-decoration: underline">`       |
//! | `Container::Style(Strikethrough)` | `<span style="text-decoration: line-through">` |
//! | `Container::Div`         | `<div>`/`<span>` with raw `style`/`class` attributes    |
//! | `Container::Color`       | `<span style="color: …">`                              |
//! | `Container::Size`        | `<span style="font-size: …">`                          |
//! | `Container::Align`       | `<div style="text-align: …">`                          |
//! | `Container::Quote`       | `<blockquote>`                                          |
//! | `Heading`                | `<h1>`..`<h6>`                                          |
//! | `Table`                  | `<table class="wiki-content-table">`                   |
//! | `Image`                  | `<div class="image-container …"><img class="image">`    |
//! | `Link`                   | `<a href>`                                              |
//! | `SupSubscript`           | `<sup>` / `<sub>`                                       |
//! | `Tabview`                | `<div class="yui-navset">` (static, first tab visible)  |
//! | `HorizontalRule`         | `<hr>`                                                  |
//! | `Footnote`               | `<sup class="footnoteref">` (collected at page foot)    |
//! | `Stylesheet`             | `<style>` injected into the document head               |
//!
//! Unknown / dynamic nodes (`ListPages`, `Include`, module vars) degrade to a
//! best-effort inline rendering; this is a read-only view of an export.

use kolorinko_wikitext::{
    Align, AlignSide, ContainerKind, Content, LinkTarget, Node, TableCell, TextStyle, TextObj,
};
use leptos::prelude::*;

/// Render a list of nodes as siblings (no paragraph grouping). Used inside
/// containers where Wikidot does not wrap children in `<p>` (table cells,
/// headings, spans).
pub(crate) fn render_inline(content: &Content) -> Vec<AnyView> {
    content.iter().map(render_node).collect()
}

/// Render the body of `#page-content`: groups top-level *inline* runs into
/// `<p>` elements, while block nodes (`Heading`, `Table`, `Image`, …) render
/// standalone, exactly as Wikidot's renderer does.
pub(crate) fn render_block(content: &Content) -> Vec<AnyView> {
    let mut out: Vec<AnyView> = Vec::with_capacity(content.len());
    let mut para: Vec<AnyView> = Vec::new();

    let flush = |para: &mut Vec<AnyView>, out: &mut Vec<AnyView>| {
        if !para.is_empty() {
            let p = std::mem::take(para);
            out.push(view! { <p>{p}</p> }.into_any());
        }
    };

    for node in content {
        if is_block(node) {
            flush(&mut para, &mut out);
            out.push(render_node(node));
        } else {
            // A blank text node breaks the current paragraph (blank line).
            if let Node::Text(TextObj::Plain(t)) = node
                && t.trim().is_empty()
                && t.contains('\n')
            {
                flush(&mut para, &mut out);
            } else {
                para.push(render_node(node));
            }
        }
    }
    flush(&mut para, &mut out);
    out
}

/// Whether a node is rendered as a block-level element (and thus flushes the
/// current inline paragraph).
fn is_block(node: &Node) -> bool {
    matches!(
        node,
        Node::Heading { .. }
            | Node::Table(_)
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

/// Render a single node.
fn render_node(node: &Node) -> AnyView {
    match node {
        Node::Text(t) => render_text_obj(t).into_any(),
        Node::Raw(s) => view! { <span style="white-space: pre-wrap">{s.clone()}</span> }.into_any(),
        Node::Container { kind, content } => render_container(kind, content),
        Node::Heading { level, content } => render_heading(*level, content),
        Node::Table(rows) => render_table(rows),
        Node::Image { align, source, params } => render_image(align, source, params),
        Node::Link { target, text } => render_link(target, text),
        Node::SupSubscript { sup, sub } => {
            let has_sup = !sup.is_empty();
            let has_sub = !sub.is_empty();
            view! {
                <>
                    {has_sup.then(|| view! { <sup>{render_inline(sup)}</sup> })}
                    {has_sub.then(|| view! { <sub>{render_inline(sub)}</sub> })}
                </>
            }
            .into_any()
        }
        Node::HorizontalRule => view! { <hr /> }.into_any(),
        Node::Stylesheet(css) => {
            // Inject as an in-document <style>. Leptos renders it inline; the
            // browser hoists <style> to <head> per the HTML spec.
            view! { <style>{css.clone()}</style> }.into_any()
        }
        Node::Footnote(content) => {
            // Inline marker; full footnote block is rendered at page foot by
            // render_page. Render the content inline as a best-effort.
            let inner = render_inline(content);
            view! { <sup class="footnoteref">{inner}</sup> }.into_any()
        }
        Node::Tabview(tabs) => render_tabview(tabs),
        Node::ListPages(lp) => {
            // Static export: render the repeat body once (no pages to iterate).
            render_block(&lp.repeat).into_any()
        }
        Node::Include(_) => view! { <span class="include-placeholder">"[include]"</span> }.into_any(),
        Node::Date { timestamp, .. } => {
            view! { <span class="odate">{format!("#{timestamp}")}</span> }.into_any()
        }
    }
}

/// Render a text object (plain text or a variable placeholder).
fn render_text_obj(t: &TextObj) -> AnyView {
    match t {
        TextObj::Plain(s) => view! { {s.clone()} }.into_any(),
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

/// Render a container (`//italic//`, `[[div]]`, `[[=]]`, `##color|…##`, …).
fn render_container(kind: &ContainerKind, content: &Content) -> AnyView {
    match kind {
        ContainerKind::Style(TextStyle::Italic) => {
            view! { <em>{render_inline(content)}</em> }.into_any()
        }
        ContainerKind::Style(TextStyle::Bold) => {
            view! { <strong>{render_inline(content)}</strong> }.into_any()
        }
        ContainerKind::Style(TextStyle::Underline) => {
            view! {
                <span style="text-decoration: underline">{render_inline(content)}</span>
            }
            .into_any()
        }
        ContainerKind::Style(TextStyle::Strikethrough) => {
            view! {
                <span style="text-decoration: line-through">{render_inline(content)}</span>
            }
            .into_any()
        }
        ContainerKind::Div { inline, params } => {
            let (class, style) = params_to_class_style(params);
            let inner = render_block(content);
            // The element name is statically either div or span; emit both
            // branches rather than building a dynamic tag.
            if *inline {
                view! {
                    <span class=class style=style>{inner}</span>
                }
                .into_any()
            } else {
                view! {
                    <div class=class style=style>{inner}</div>
                }
                .into_any()
            }
        }
        ContainerKind::Color(c) => {
            view! {
                <span style=format!("color: {c}")>{render_inline(content)}</span>
            }
            .into_any()
        }
        ContainerKind::Size(arg) => {
            view! {
                <span style=format!("font-size: {}", normalize_size(arg))>
                    {render_inline(content)}
                </span>
            }
            .into_any()
        }
        ContainerKind::Align(Align { side, .. }) => {
            let align = match side {
                AlignSide::Left => "left",
                AlignSide::Center => "center",
                AlignSide::Right => "right",
                AlignSide::Justify => "justify",
            };
            view! {
                <div style=format!("text-align: {align}")>{render_block(content)}</div>
            }
            .into_any()
        }
        ContainerKind::Quote => {
            view! { <blockquote>{render_block(content)}</blockquote> }.into_any()
        }
        ContainerKind::IfTags { .. } => {
            // Tag gating is a server concern; render the body unconditionally.
            view! { <>{render_block(content)}</> }.into_any()
        }
    }
}

/// `<h1>`..`<h6>`. Levels above 6 clamp to 6.
fn render_heading(level: u32, content: &Content) -> AnyView {
    let inner = render_inline(content);
    match level.min(6) {
        1 => view! { <h1>{inner}</h1> }.into_any(),
        2 => view! { <h2>{inner}</h2> }.into_any(),
        3 => view! { <h3>{inner}</h3> }.into_any(),
        4 => view! { <h4>{inner}</h4> }.into_any(),
        5 => view! { <h5>{inner}</h5> }.into_any(),
        _ => view! { <h6>{inner}</h6> }.into_any(),
    }
}

/// `<table class="wiki-content-table">` with `<th>`/`<td>` cells.
fn render_table(rows: &[Vec<TableCell>]) -> AnyView {
    let rows_view: Vec<AnyView> = rows
        .iter()
        .map(|row| {
            let cells: Vec<AnyView> = row
                .iter()
                .map(|cell| {
                    let inner = render_inline(&cell.content);
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

/// `<div class="image-container …"><img class="image" />`.
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
    let class = classes.join(" ");
    view! {
        <div class=class>
            <img class="image" src=src alt=alt style=img_style />
        </div>
    }
    .into_any()
}

/// `<a href>`; external URLs verbatim, internal refs become `/site/page`.
fn render_link(target: &LinkTarget, text: &Content) -> AnyView {
    let href = match target {
        LinkTarget::Url(u) => u.clone(),
        LinkTarget::Page(p) => {
            let slug = p.path.join("/");
            match &p.space {
                Some(space) => format!("/{space}/{slug}"),
                None => format!("/{slug}"),
            }
        }
    };
    let inner = render_inline(text);
    view! { <a href=href>{inner}</a> }.into_any()
}

/// `[[tabview]]` → a static tab strip. Wikidot's is JS-driven (YUI); we render
/// the same DOM skeleton (`.yui-navset`, `.yui-nav`, `.yui-content`) and show
/// the first tab. Interactivity is a later concern.
fn render_tabview(tabs: &[kolorinko_wikitext::Tab]) -> AnyView {
    if tabs.is_empty() {
        return view! { <div class="yui-navset"></div> }.into_any();
    }
    let nav: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let name = render_inline(&tab.name);
            let class = if i == 0 { Some("selected") } else { None };
            view! {
                <li class=class><em>{name}</em></li>
            }
            .into_any()
        })
        .collect();
    let panels: Vec<AnyView> = tabs
        .iter()
        .enumerate()
        .map(|(i, tab)| {
            let body = render_block(&tab.content);
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

// ---- helpers ------------------------------------------------------------------

/// Extract `(class, style)` from a `[[div]]`/`[[span]]` attribute map, joining
/// the attribute name "class" and the rest as inline `style`/passthrough.
///
/// Wikidot passes `style="…"`, `class="…"` and arbitrary keys straight
/// through; we honour the two common ones and best-effort the rest into style.
fn params_to_class_style(
    params: &std::collections::HashMap<String, Vec<TextObj>>,
) -> (String, String) {
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
    (class, style)
}

/// Flatten a list of [`TextObj`] into a single string (variables resolved to
/// their defaults / placeholders). Used where only a plain value makes sense
/// (img `src`, attribute values).
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

/// Wikidot image-container class for a given alignment.
fn image_container_class(a: &Align) -> String {
    match (a.floating, a.side) {
        (true, AlignSide::Left) => "floatleft".into(),
        (true, AlignSide::Right) => "floatright".into(),
        (false, AlignSide::Left) => "alignleft".into(),
        (false, AlignSide::Right) => "alignright".into(),
        _ => "aligncenter".into(),
    }
}

/// CSS `text-align` value for an [`AlignSide`].
fn side_to_css(s: AlignSide) -> &'static str {
    match s {
        AlignSide::Left => "left",
        AlignSide::Center => "center",
        AlignSide::Right => "right",
        AlignSide::Justify => "justify",
    }
}

/// CSS `float` value for an [`AlignSide`].
fn side_to_float(s: AlignSide) -> &'static str {
    match s {
        AlignSide::Left => "left",
        AlignSide::Right => "right",
        _ => "none",
    }
}

/// Normalize a `[[size ARG]]` argument into a CSS font-size value.
fn normalize_size(arg: &str) -> String {
    let arg = arg.trim();
    // Pure number → em.
    if arg.chars().all(|c| c.is_ascii_digit() || c == '.') && !arg.is_empty() {
        return format!("{arg}em");
    }
    // Bare percentage already includes %.
    if arg.ends_with('%') || arg.ends_with("em") || arg.ends_with("px") {
        return arg.into();
    }
    // Named keywords (xx-small … xx-large, smaller, larger) pass through.
    arg.into()
}
