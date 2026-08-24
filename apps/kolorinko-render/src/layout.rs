//! The page skeleton (`#container-wrap-wrap` …), shared by the CSR web client
//! (`kolorinko-web`), the SSR server response, and the SSR debug CLI — the
//! single source of truth for the browser layout, so server-rendered HTML and
//! the hydrated client match by construction.
//!
//! The layout is reactive but platform-agnostic: it takes getter closures for
//! the current site, site shell, and page. The client feeds them signals
//! (seeded from the SSR state, then kept live by WebTransport); the server
//! feeds resolved gear outputs and serializes the result once with
//! [`RenderHtml::to_html`]. Hydration is positional, so both sides must render
//! the same tree from the same data — sharing this function is what guarantees
//! that.

use kolorinko_wikitext::{ArticleView, PageDep};
use leptos::prelude::*;

use crate::render::render_block;

/// `<link id>` of the per-site theme stylesheet, injected into `<head>` by the
/// server (SSR) and managed reactively by the client.
pub const THEME_LINK_ID: &str = "kolorinko-site-theme";

/// Fallback `<title>` when a page has no title of its own.
pub const TITLE_FALLBACK: &str = "dntrd";

/// Render the full page layout. `space` is the canonical space the page
/// renders under (the header link and internal-link prefix; `None` in a
/// context-less render); `site_name` is the human-readable site name (the
/// header's fallback text). The shell getters expose one chrome field each
/// — `None` while the shell loads — so each reactive node clones only the
/// field it renders, never the whole [`SiteShell`]; `page` likewise for the
/// current page (or `None` while it loads).
pub fn layout(
    space: impl Fn() -> Option<kolorinko_rt::SpaceId> + Clone + Send + 'static,
    site_name: impl Fn() -> String + Clone + Send + 'static,
    shell_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    shell_subtitle: impl Fn() -> Option<String> + Clone + Send + 'static,
    nav_top: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    nav_side: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    page_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    page: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
) -> AnyView {
    // One clone per capture site: `view!` moves each closure into the tree.
    // `site_top`/`site_side`/`site_body`/`deps_space`/`home` carry the space
    // (link prefix); `site_title` carries the display name.
    let site_title = site_name;
    let [site_top, site_side, site_body, deps_space, home] = [
        space.clone(),
        space.clone(),
        space.clone(),
        space.clone(),
        space,
    ];
    let [page_body, page_deps] = [page.clone(), page];
    let show_deps = RwSignal::new(false);
    view! {
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1>
                        <a href=move || {
                            home()
                                .map(|s| format!("/{s}"))
                                .unwrap_or_else(|| "/".to_string())
                        }>
                            <span>{move || shell_title().unwrap_or_else(&site_title)}</span>
                        </a>
                    </h1>
                    <h2><span>{move || shell_subtitle().unwrap_or_default()}</span></h2>
                    <div id="top-bar">{move || view_nav(site_top(), nav_top(), true)}</div>
                </div>
                <div id="content-wrap">
                    <div id="side-bar">{move || view_nav(site_side(), nav_side(), false)}</div>
                    <div id="main-content">
                        <div id="page-title">
                            {move || page_title().unwrap_or_default()}
                        </div>
                        <div id="page-content">{move || match page_body() {
                            Some(a) => {
                                let blocks = render_block(site_body(), &a.content);
                                view! { <>{blocks}</> }.into_any()
                            }
                            None => view! { <p class="kolorinko-status">"loading…"</p> }.into_any(),
                        }}</div>
                        // The page-options bar (Wikidot's Edit / Rate / Tags /
                        // … section) — ours carries the dependency-tree toggle.
                        <div id="page-options-bottom" class="page-options-bottom">
                            <a
                                id="deps-button"
                                href="javascript:;"
                                on:click=move |_| show_deps.update(|open| *open = !*open)
                            >"Dependencies"</a>
                        </div>
                        {move || {
                            let page_deps = page_deps.clone();
                            let deps_space = deps_space.clone();
                            show_deps.get().then(|| view! {
                                <div id="action-area">
                                    {move || match page_deps().map(|a| a.deps) {
                                        Some(deps) if !deps.is_empty() => view_deps(deps_space(), &deps),
                                        _ => view! {
                                            <p class="kolorinko-status">"no dependencies"</p>
                                        }.into_any(),
                                    }}
                                </div>
                            })
                        }}
                    </div>
                </div>
            </div>
        </div>
        </div>
    }
    .into_any()
}

/// A page's dependency tree as nested lists: each fetched include target
/// links to its page — the canonical slug form `/{space}/cat:name` (the
/// server resolves the slug and 301s; `None` in a context-less render links
/// nowhere), its own dependencies nested beneath it. Debug UI: cross-site
/// includes link through the rendered page's space, which is right for the
/// same-site cone and merely approximate otherwise.
fn view_deps(space: Option<kolorinko_rt::SpaceId>, deps: &[PageDep]) -> AnyView {
    let items: Vec<AnyView> = deps
        .iter()
        .map(|d| {
            let name = d.page.as_str();
            let label = match &d.category {
                Some(cat) => format!("{cat}:{name}"),
                None => name.to_string(),
            };
            let href = match (space, &d.category) {
                (Some(s), Some(cat)) => format!("/{s}/{cat}:{name}"),
                (Some(s), None) => format!("/{s}/{name}"),
                (None, _) => "javascript:;".to_string(),
            };
            let sub = (!d.deps.is_empty()).then(|| view_deps(space, &d.deps));
            view! { <li><a href=href>{label}</a>{sub}</li> }.into_any()
        })
        .collect();
    view! { <ul class="kolorinko-deps">{items}</ul> }.into_any()
}

/// Render a navigation page (`nav:top` / `nav:side`) — or nothing before it
/// arrives. The same renderer as page bodies; nav pages are just pages. When
/// `top_bar` is set the `nav:top` list transform is applied first, wrapping
/// each top-level submenu item in the `<a href="javascript:;">` legacy themes
/// target.
fn view_nav(
    space: Option<kolorinko_rt::SpaceId>,
    nav: Option<ArticleView>,
    top_bar: bool,
) -> AnyView {
    match nav {
        Some(mut a) => {
            if top_bar {
                crate::render::wrap_topbar_lists(&mut a.content);
            }
            let blocks = render_block(space, &a.content);
            view! { <>{blocks}</> }.into_any()
        }
        None => ().into_any(),
    }
}

/// The browser tab title for a page title (possibly empty).
pub fn document_title(site: &str, title: &Option<String>, page_title: &str) -> String {
    let site = match title.as_ref() {
        Some(x) => x.as_str(),
        None => site,
    };
    if page_title.is_empty() {
        format!("{site} | {TITLE_FALLBACK}")
    } else {
        format!("{page_title} - {site} | {TITLE_FALLBACK}")
    }
}

/// The per-site theme `<link>` element for `<head>` (SSR side; the client
/// manages the same element by [`THEME_LINK_ID`]).
pub fn theme_link(href: &str) -> String {
    format!(r#"<link id="{THEME_LINK_ID}" rel="stylesheet" href="{href}">"#)
}

/// Escape text for HTML element content or a double-quoted attribute.
pub fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
