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

use crate::about::ABOUT_PATH;
use crate::render::{Scope, render_block};

/// `<link id>` of the per-site theme stylesheet, injected into `<head>` by the
/// server (SSR) and managed reactively by the client.
pub const THEME_LINK_ID: &str = "kolorinko-site-theme";

/// Fallback `<title>` when a page has no title of its own.
pub const TITLE_FALLBACK: &str = "dntrd";

/// The Dentrado read-only banner's stylesheet, rendered by [`layout`] as a
/// `<style>` element through `inner_html` (raw on both the SSR and CSR
/// side — no entity escaping, which a raw-text element would take
/// literally). Kept free of `<`, `>`, `&`, and quotes by construction:
/// escaped text inside `<style>` is spelled as entities and would break the
/// rules (asserted by `banner_css_survives_raw`).
const BANNER_CSS: &str = "\
#dentrado-banner{display:flex;flex-wrap:wrap;gap:.25em 1.5em;align-items:baseline;justify-content:space-between;padding:.55em 1.25em;background:#0e1116;color:#a7b0bf;font:500 12.5px/1.45 ui-sans-serif,system-ui,sans-serif;text-align:left}\
#dentrado-banner a{color:#e6965a;text-decoration:none;border-bottom:1px solid rgba(230,150,90,.35)}\
#dentrado-banner a:hover{border-bottom-color:#e6965a}\
#dentrado-banner .dentrado-brand{color:#f4f6f9;font-weight:700;letter-spacing:.02em}\
#dentrado-banner .dentrado-about{white-space:nowrap}\
";

/// Render the full page layout. `scope` — see [`Scope`] — is the render's
/// addressing scope: the space pages render under and, on the client, the
/// origin's default space every href simplifies against (the server passes
/// `default: None` and emits full-weight links). `site_name` is the
/// human-readable site name (the header's fallback text). The shell getters
/// expose one chrome field each — `None` while the shell loads — so each
/// reactive node clones only the field it renders, never the whole
/// [`SiteShell`]; `page` likewise for the current page (or `None` while it
/// loads). `shell_site` is the mirrored Wikidot site slug
/// (`<site>.wikidot.com`) — the read-only banner's backlink — and is the one
/// field that cannot wait for the shell: it is what the banner exists to say.
pub fn layout(
    scope: impl Fn() -> Scope + Clone + Send + 'static,
    site_name: impl Fn() -> String + Clone + Send + 'static,
    shell_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    shell_subtitle: impl Fn() -> Option<String> + Clone + Send + 'static,
    shell_site: impl Fn() -> Option<String> + Clone + Send + 'static,
    nav_top: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    nav_side: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
    page_title: impl Fn() -> Option<String> + Clone + Send + 'static,
    page: impl Fn() -> Option<ArticleView> + Clone + Send + 'static,
) -> AnyView {
    // One clone per capture site: `view!` moves each closure into the tree.
    // `site_top`/`site_side`/`site_body`/`deps_scope`/`home` feed the render
    // scope to the reactive nodes; `site_title` carries the display name.
    let site_title = site_name;
    let [site_top, site_side, site_body, deps_scope, home] = [
        scope.clone(),
        scope.clone(),
        scope.clone(),
        scope.clone(),
        scope,
    ];
    let [page_body, page_slug, page_deps] = [page.clone(), page.clone(), page];
    let show_deps = RwSignal::new(false);
    view! {
        <style>{BANNER_CSS}</style>
        <div id="dentrado-banner">
            <span class="dentrado-note">
                <span class="dentrado-brand">"Dentrado"</span>
                <span>", read-only mode: mirroring "
                    {move || match shell_site() {
                        Some(site) => {
                            // The page's Wikidot fullname (`cat:slug`) rides
                            // the same URL the source wiki served; `None`
                            // until the page arrives (never at hydration —
                            // the SSR state already carries it). Cloning the
                            // getter keeps this closure `Fn` (a fresh `url`
                            // per re-run, the shared getter captured once).
                            let page_slug = page_slug.clone();
                            let url = move || page_slug()
                                .map(|a| a.meta.slug)
                                .map(|slug| format!("http://{site}.wikidot.com/{slug}"))
                                .unwrap_or_else(|| format!("http://{site}.wikidot.com/"));
                            let href = url.clone();
                            view! { <a class="dentrado-mirror" href=href>{url}</a> }.into_any()
                        }
                        // A space the registry doesn't know (context-less
                        // debug render) has no source wiki to point at.
                        None => view! { <span class="dentrado-mirror">"…"</span> }.into_any(),
                    }}
                </span>
            </span>
            <a class="dentrado-about" href=ABOUT_PATH>"About Dentrado"</a>
        </div>
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1>
                        <a href=move || home().root()>
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
                            let deps_scope = deps_scope.clone();
                            show_deps.get().then(|| view! {
                                <div id="action-area">
                                    {move || match page_deps().map(|a| a.deps) {
                                        Some(deps) if !deps.is_empty() => view_deps(deps_scope(), &deps),
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
/// server resolves the slug and 301s; simplified on a wiki's own domain,
/// context-less renders link nowhere), its own dependencies nested beneath
/// it. Debug UI: cross-site includes link through the rendered page's space,
/// which is right for the same-site cone and merely approximate otherwise.
fn view_deps(scope: Scope, deps: &[PageDep]) -> AnyView {
    let items: Vec<AnyView> = deps
        .iter()
        .map(|d| {
            let name = d.page.as_str();
            let label = match &d.category {
                Some(cat) => format!("{cat}:{name}"),
                None => name.to_string(),
            };
            let href = match scope.space {
                Some(s) => kolorinko_rt::simplify(
                    scope.default,
                    &match &d.category {
                        Some(cat) => format!("/{s}/{cat}:{name}"),
                        None => format!("/{s}/{name}"),
                    },
                ),
                None => "javascript:;".to_string(),
            };
            let sub = (!d.deps.is_empty()).then(|| view_deps(scope, &d.deps));
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
fn view_nav(scope: Scope, nav: Option<ArticleView>, top_bar: bool) -> AnyView {
    match nav {
        Some(mut a) => {
            if top_bar {
                crate::render::wrap_topbar_lists(&mut a.content);
            }
            let blocks = render_block(scope, &a.content);
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

#[cfg(test)]
mod tests {
    use super::*;
    use kolorinko_wikitext::ArticleView;

    /// Render the layout once, SSR-style, with the given mirrored site slug
    /// and page fullname.
    fn html(site: Option<&str>, slug: &str) -> String {
        let mut page = ArticleView::default();
        page.meta.slug = slug.into();
        let site = site.map(str::to_string);
        layout(
            || Scope {
                space: None,
                default: None,
            },
            || "site".into(),
            || None,
            || None,
            move || site.clone(),
            || None,
            || None,
            || None,
            move || Some(page.clone()),
        )
        .to_html()
    }

    /// `<style>` is a raw-text element: any entity the SSR serializer spelled
    /// out would be taken literally by the CSS parser, breaking its rule. The
    /// stylesheet avoids the escapable characters by construction, and must
    /// survive serialization verbatim.
    #[test]
    fn banner_css_survives_raw() {
        assert!(!BANNER_CSS.contains(['<', '>', '&', '"', '\'']));
        assert!(html(None, "").contains(BANNER_CSS));
    }

    /// The banner names the source wiki (and page) it mirrors; a space the
    /// registry doesn't know has no source to point at.
    #[test]
    fn banner_mirrors_source_url() {
        let known = html(Some("obscurative"), "docs:guide");
        assert!(known.contains("http://obscurative.wikidot.com/docs:guide"));
        assert!(known.contains(r#"href="/~/about""#));
        assert!(!html(None, "").contains("wikidot.com"));
    }
}
