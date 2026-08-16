//! SSR-only document assembly, shared by the live server's SSR response and
//! the `kolorinko render` debug CLI. Gated on `ssr` because it calls
//! [`RenderHtml::to_html`]; the layout itself is mode-agnostic.

use kolorinko_rt::{SSR_STATE_ID, SiteShell, SsrState};
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

use crate::layout::{document_title, html_escape, layout, theme_link};

/// Render `state` (page + shell) for `site` through the shared [`layout`] into
/// HTML — exactly the tree the client renders from the same state, which is
/// what positional hydration matches against.
fn render_app(site: &str, state: &SsrState) -> String {
    let (site, shell, page) = (site.to_string(), state.shell.clone(), state.page.clone());
    let title = page.meta.title.clone();
    layout(
        move || site.clone(),
        move || shell.title.clone(),
        move || shell.subtitle.clone(),
        move || Some(shell.nav_top.clone()),
        move || Some(shell.nav_side.clone()),
        move || Some(title.clone()),
        move || Some(page.clone()),
    )
    .to_html()
}

/// The `<div id="container"></div>` placeholder in the built frontend's
/// `index.html`, replaced by the SSR'd app + embedded state.
const APP_PLACEHOLDER: &str = r#"<div id="container"></div>"#;

/// Seal an SSR'd page into the built frontend's `index.html` template: the
/// rendered layout + embedded [`SsrState`] replace the app placeholder (so the
/// client hydrates positionally from `<body>`'s first child), the `<title>`
/// carries the page title, and the site theme `<link>` joins `<head>`. `None`
/// when the template has no placeholder (unbuilt frontend).
pub fn render_ssr_document(index: &str, site: &str, state: &SsrState) -> Option<String> {
    let app = render_app(site, state);
    let embedded = format!(
        r#"<script type="application/json" id="{SSR_STATE_ID}">{}</script>"#,
        state.to_embedded_json()
    );
    let doc = replace_placeholder(index, &format!("{app}{embedded}"))?;
    let doc = set_title(
        &doc,
        &document_title(site, &state.shell.title, &state.page.meta.title),
    );
    Some(match state.shell.theme_root.as_deref() {
        Some(href) => inject_before_head_end(&doc, &theme_link(href)),
        None => doc,
    })
}

/// Replace the placeholder — plus whatever whitespace the template wraps
/// around it — with `replacement`. Hydration walks `<body>`'s children
/// positionally from the first, so the app must land there with no stray text
/// node before it, however the template is formatted.
fn replace_placeholder(doc: &str, replacement: &str) -> Option<String> {
    let at = doc.find(APP_PLACEHOLDER)?;
    let start = doc[..at].trim_end().len();
    let after = at + APP_PLACEHOLDER.len();
    let end = doc[after..]
        .find(|c: char| !c.is_whitespace())
        .map_or(doc.len(), |i| after + i);
    Some(format!("{}{replacement}{}", &doc[..start], &doc[end..]))
}

/// Replace the contents of the template's `<title>…</title>`.
fn set_title(doc: &str, title: &str) -> String {
    match (doc.find("<title>"), doc.find("</title>")) {
        (Some(open), Some(close)) if open < close => {
            let inner = open + "<title>".len();
            format!("{}{}{}", &doc[..inner], html_escape(title), &doc[close..])
        }
        _ => doc.to_string(),
    }
}

/// Insert `element` just before the template's `</head>`.
fn inject_before_head_end(doc: &str, element: &str) -> String {
    match doc.find("</head>") {
        Some(end) => format!("{}{element}{}", &doc[..end], &doc[end..]),
        None => doc.to_string(),
    }
}

/// Render `page` under `shell` into a complete standalone HTML document (the
/// debug CLI's output). `base_css` — the Wikidot base theme stylesheet, read by
/// the caller from the frontend dist — is inlined into `<head>` when given, so
/// the output is a single self-contained file (no external
/// `/wikidot-base-theme/…` link that only resolves when served).
pub fn render_page_document(
    site: &str,
    shell: &SiteShell,
    page: &ArticleView,
    base_css: Option<&str>,
) -> String {
    let body = render_app(
        site,
        &SsrState {
            page: page.clone(),
            shell: shell.clone(),
        },
    );

    let title = html_escape(&document_title(site, &shell.title, &page.meta.title));
    let style = base_css
        .map(|css| format!("<style>\n{css}\n</style>\n"))
        .unwrap_or_default();
    let theme = shell
        .theme_root
        .as_deref()
        .map(|href| format!("{}\n", theme_link(href)))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         {style}\
         {theme}\
         </head>\n\
         <body>\n{body}\n</body>\n</html>\n"
    )
}
