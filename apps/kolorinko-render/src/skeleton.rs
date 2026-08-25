//! SSR-only document assembly, shared by the live server's SSR response and
//! the `kolorinko render` debug CLI. Gated on `ssr` because it calls
//! [`RenderHtml::to_html`]; the layout itself is mode-agnostic.

use kolorinko_rt::{LocalId, SSR_STATE_ID, SiteShell, SpaceId, SsrState, default_space_script};
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

use crate::about::about_page;
use crate::layout::{document_title, html_escape, layout, theme_link};
use crate::opengraph;

/// Render `state` (page + shell) through the shared [`layout`] into HTML —
/// exactly the tree the client renders from the same state, which is what
/// positional hydration matches against.
fn render_app(state: &SsrState) -> String {
    let (space, shell, page) = (state.space, state.shell.clone(), state.page.clone());
    let name = shell.title.clone().unwrap_or_else(|| space.as_str());
    let title = page.meta.title.clone();
    layout(
        move || Some(space),
        move || name.clone(),
        move || shell.title.clone(),
        move || shell.subtitle.clone(),
        move || shell.site.clone(),
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
/// carries the page title, the site theme `<link>` and the OpenGraph card join
/// `<head>`, and `default_space` — the space the request's host already names,
/// on a wiki's own domain — rides along as `window.__DEFAULT_SPACE_ID__` so
/// the client reads `/L…` paths (and collapses `/{default}/L…` hrefs) against
/// it. `host` — the request's `host[:port]` — absolutizes the card's URLs;
/// `canonical` — the page's canonical absolute URL — is its `og:url` (both
/// `None` in the debug CLI). `None` when the template has no placeholder
/// (unbuilt frontend).
pub fn render_ssr_document(
    index: &str,
    state: &SsrState,
    host: Option<&str>,
    canonical: Option<&str>,
    default_space: Option<SpaceId>,
) -> Option<String> {
    // The display name falls back to the space id spelling when the export
    // carries no site title.
    let site = state
        .shell
        .title
        .clone()
        .unwrap_or_else(|| state.space.as_str());
    let app = render_app(state);
    let embedded = format!(
        r#"<script type="application/json" id="{SSR_STATE_ID}">{}</script>"#,
        state.to_embedded_json()
    );
    let doc = replace_placeholder(index, &format!("{app}{embedded}"))?;
    let doc = set_title(
        &doc,
        &document_title(&site, &state.shell.title, &state.page.meta.title),
    );
    let og = opengraph::meta(&site, &state.shell, &state.page, host, canonical);
    let theme = state
        .shell
        .theme_root
        .as_deref()
        .map(&theme_link)
        .unwrap_or_default();
    let default = default_space.map(default_space_script).unwrap_or_default();
    Some(inject_before_head_end(
        &doc,
        &format!("{og}{theme}{default}"),
    ))
}

/// Seal the about screen into the same `index.html` template — an SSR page
/// like any other, minus the parts it has none of: the rendered
/// [`about_page`] replaces the app placeholder, the `<title>` is the
/// platform's, and the OpenGraph card ([`opengraph::about_meta`]) joins
/// `<head>`. No [`SsrState`] is embedded — the screen carries no data — so
/// the client boots CSR there: it clears the body (dropping this markup,
/// identical to what it re-renders) and takes over routing from
/// [`crate::about::ABOUT_PATH`]. `host` absolutizes the card's `og:url`.
/// `None` when the template has no placeholder (unbuilt frontend).
pub fn render_about_document(index: &str, host: Option<&str>) -> Option<String> {
    let doc = replace_placeholder(index, &about_page().to_html())?;
    let doc = set_title(&doc, "Dentrado");
    Some(inject_before_head_end(&doc, &opengraph::about_meta(host)))
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
    shell: &SiteShell,
    page: &ArticleView,
    base_css: Option<&str>,
) -> String {
    let body = render_app(&SsrState {
        page: page.clone(),
        // No content hashes: this is a static debug document, never a
        // hydration source; an empty hash matches nothing, so a live
        // client (if one ever loaded it) would simply get a full push.
        page_hash: String::new(),
        shell: shell.clone(),
        shell_hash: String::new(),
        // Context-less debug render: no canonical address (the ids are
        // zero placeholders, purely to satisfy the type).
        space: SpaceId::from_bytes([0; 16]),
        local: LocalId::new(0),
    });

    let site = "kolorinko";
    let title = html_escape(&document_title(site, &shell.title, &page.meta.title));
    let og = opengraph::meta(site, shell, page, None, None);
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
         {og}\
         {style}\
         {theme}\
         </head>\n\
         <body>\n{body}\n</body>\n</html>\n"
    )
}
