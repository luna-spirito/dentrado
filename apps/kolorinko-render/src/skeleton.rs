//! SSR-only: render a fully-resolved page (plus its `nav:top` / `nav:side`
//! pages) into a standalone HTML document matching the browser layout, for
//! the `kolorinko render` debug CLI.
//!
//! Gated on `ssr` because it calls [`RenderHtml::to_html`] (via
//! `leptos::prelude::*`, which re-exports `tachys::prelude::*`). The render
//! functions in [`crate::render`] are mode-agnostic; this module wraps them in
//! the `#container-wrap-wrap` skeleton [`kolorinko_web`]'s `layout` produces
//! in the browser, then seals the result into a complete `<!doctype html>`
//! document with the base theme CSS inlined (so the output is a self-contained
//! file you can open directly).
//!
//! [`kolorinko_web`]: https://docs.rs/kolorinko-web

use kolorinko_wikitext::ArticleView;
use leptos::prelude::*;

use crate::render::render_block;

/// Render `page` (and optional `nav:top` / `nav:side` pages) into a complete
/// HTML document. `base_css` — the Wikidot base theme stylesheet, read by the
/// caller from the frontend dist — is inlined into `<head>` when given, so the
/// output is a single self-contained file (no external `/wikidot-base-theme/…`
/// link that only resolves when served).
pub fn render_page_document(
    site: &str,
    page: &ArticleView,
    nav_top: Option<&ArticleView>,
    nav_side: Option<&ArticleView>,
    base_css: Option<&str>,
) -> String {
    let body = view! {
        <div id="container-wrap-wrap">
        <div id="container-wrap">
            <div id="container">
                <div id="header">
                    <h1><a href="/">{site.to_string()}</a></h1>
                    <div id="top-bar">{nav_blocks(site, nav_top)}</div>
                </div>
                <div id="content-wrap">
                    <div id="side-bar">{nav_blocks(site, nav_side)}</div>
                    <div id="main-content">
                        <div id="page-title">{page.meta.title.clone()}</div>
                        <div id="page-content">{render_block(site, &page.content)}</div>
                    </div>
                </div>
            </div>
        </div>
        </div>
    }
    .to_html();

    let style = base_css
        .map(|css| format!("<style>\n{css}\n</style>\n"))
        .unwrap_or_default();
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{title}</title>\n\
         {style}\
         </head>\n\
         <body>\n{body}\n</body>\n</html>\n",
        title = html_escape(&page.meta.title)
    )
}

fn nav_blocks(site: &str, nav: Option<&ArticleView>) -> Vec<AnyView> {
    match nav {
        Some(a) => render_block(site, &a.content),
        None => Vec::new(),
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
