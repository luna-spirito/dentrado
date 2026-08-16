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

use kolorinko_rt::SiteShell;
use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

use crate::render::render_block;

/// `<link id>` of the per-site theme stylesheet, injected into `<head>` by the
/// server (SSR) and managed reactively by the client.
pub const THEME_LINK_ID: &str = "kolorinko-site-theme";

/// Fallback `<title>` when a page has no title of its own.
pub const TITLE_FALLBACK: &str = "dntrd";

/// Render the full page layout. `site` is the current site name (used for the
/// header link and as the internal-link prefix), `shell` the site chrome (or
/// `None` while it loads), `page` the current page (or `None` while it loads).
pub fn layout(
    site: impl Fn() -> String + Clone + Send + 'static,
    shell: impl Fn() -> Option<SiteShell> + Clone + Send + 'static,
    page: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
) -> AnyView {
    // One clone per capture site: `view!` moves each closure into the tree.
    let [site_href, site_title, site_top, site_side, site_body] =
        [site.clone(), site.clone(), site.clone(), site.clone(), site];
    let [shell_title, shell_sub, shell_top, shell_side] =
        [shell.clone(), shell.clone(), shell.clone(), shell];
    let [page_title, page_body] = [page.clone(), page];
    view! {
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1>
                        <a href=move || format!("/{}/", site_href())>
                            <span>{move || {
                                shell_title().and_then(|x| x.title).unwrap_or_else(&site_title)
                            }}</span>
                        </a>
                    </h1>
                    <h2><span>{move || shell_sub().and_then(|s| s.subtitle).unwrap_or_default()}</span></h2>
                    <div id="top-bar">{move || view_nav(&site_top(), shell_top().map(|s| s.nav_top), true)}</div>
                </div>
                <div id="content-wrap">
                    <div id="side-bar">{move || view_nav(&site_side(), shell_side().map(|s| s.nav_side), false)}</div>
                    <div id="main-content">
                        <div id="page-title">
                            {move || page_title().map(|a| a.meta.title).unwrap_or_default()}
                        </div>
                        <div id="page-content">{move || match page_body() {
                            Some(a) => {
                                let blocks = render_block(&site_body(), &a.content);
                                view! { <>{blocks}</> }.into_any()
                            }
                            None => view! { <p class="kolorinko-status">"loading…"</p> }.into_any(),
                        }}</div>
                    </div>
                </div>
            </div>
        </div>
        </div>
    }
    .into_any()
}

/// Render a navigation page (`nav:top` / `nav:side`) — or nothing before it
/// arrives. The same renderer as page bodies; nav pages are just pages. When
/// `top_bar` is set the `nav:top` list transform is applied first, wrapping
/// each top-level submenu item in the `<a href="javascript:;">` legacy themes
/// target.
fn view_nav(site: &str, nav: Option<ArticleView>, top_bar: bool) -> AnyView {
    match nav {
        Some(mut a) => {
            if top_bar {
                crate::render::wrap_topbar_lists(&mut a.content);
            }
            let blocks = render_block(site, &a.content);
            view! { <>{blocks}</> }.into_any()
        }
        None => ().into_any(),
    }
}

/// The browser tab title for a page title (possibly empty).
pub fn document_title(page_title: &str) -> String {
    if page_title.is_empty() {
        TITLE_FALLBACK.to_string()
    } else {
        format!("{page_title} | {TITLE_FALLBACK}")
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
