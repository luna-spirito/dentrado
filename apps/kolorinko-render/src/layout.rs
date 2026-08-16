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

use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

use crate::render::render_block;

/// `<link id>` of the per-site theme stylesheet, injected into `<head>` by the
/// server (SSR) and managed reactively by the client.
pub const THEME_LINK_ID: &str = "kolorinko-site-theme";

/// Fallback `<title>` when a page has no title of its own.
pub const TITLE_FALLBACK: &str = "dntrd";

/// Render the full page layout. `site` is the current site name (used for the
/// header link and as the internal-link prefix); the shell getters expose one
/// chrome field each — `None` while the shell loads — so each reactive node
/// clones only the field it renders, never the whole [`SiteShell`]; `page`
/// likewise for the current page (or `None` while it loads).
pub fn layout(
    site: impl Fn() -> String + Clone + Send + 'static,
    shell_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    shell_subtitle: impl Fn() -> Option<String> + Clone + Send + 'static,
    nav_top: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    nav_side: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    page_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    page: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
) -> AnyView {
    // One clone per capture site: `view!` moves each closure into the tree.
    let [site_href, site_title, site_top, site_side, site_body] =
        [site.clone(), site.clone(), site.clone(), site.clone(), site];
    view! {
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1>
                        <a href=move || format!("/{}/", site_href())>
                            <span>{move || shell_title().unwrap_or_else(&site_title)}</span>
                        </a>
                    </h1>
                    <h2><span>{move || shell_subtitle().unwrap_or_default()}</span></h2>
                    <div id="top-bar">{move || view_nav(&site_top(), nav_top(), true)}</div>
                </div>
                <div id="content-wrap">
                    <div id="side-bar">{move || view_nav(&site_side(), nav_side(), false)}</div>
                    <div id="main-content">
                        <div id="page-title">
                            {move || page_title().unwrap_or_default()}
                        </div>
                        <div id="page-content">{move || match page() {
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
