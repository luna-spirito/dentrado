//! SSR page rendering: resolve a route's page + site shell through the gear
//! runtime — the same `shell` / `article_latest` cone the WebTransport client
//! subscribes to — and seal them into the served document (see
//! [`kolorinko_render::render_ssr_document`]).

use std::{collections::HashMap, rc::Rc, sync::Arc};

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::{
    Body, LocalId, PageAddr, SafePathComponent, SiteShell, Slug, SpaceId, SsrState, wire::GearOut,
};
use kolorinko_wikitext::ArticleView;

use crate::runtime::{KolorinkoRT, article_latest, shell};

/// Resolve and render the full SSR document for `(site, slug)`, or `None` when
/// the frontend template can't host SSR output (no app placeholder — unbuilt
/// frontend); the caller then falls back to serving plain `index.html`.
/// `route` is the canonical `(space, local)` address when the URL was
/// canonical (embedded into the state so the hydrating client holds its
/// subscription keys without a resolution round-trip); `host` — the request's
/// `host[:port]` — absolutizes the OpenGraph card's URLs.
pub(crate) async fn document(
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    site: SafePathComponent,
    slug: Slug,
    route: Option<(SpaceId, LocalId)>,
    host: Option<&str>,
) -> Option<String> {
    let state = resolve(core, route, site, slug).await;
    let index = index_template(assets)?;
    kolorinko_render::render_ssr_document(&index, &state, host)
}

/// Resolve the page and shell for one route. Subscribes both before reading
/// either — holding both keeps the shared `repo` oracle active across the two
/// queries (one clone total), exactly as the render CLI does. Awaits the gears'
/// current outputs: the same resolution a CSR boot blocks on over WebTransport,
/// just server-side.
async fn resolve(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    route: Option<(SpaceId, LocalId)>,
    site: SafePathComponent,
    slug: Slug,
) -> SsrState {
    let page_q = article_latest(site.clone(), slug.clone());
    let shell_q = shell(site.clone());
    let page_sub = page_q.subscribe(core).await;
    let shell_sub = shell_q.subscribe(core).await;
    // The getters return `SharedView<…>` (a `!Send` refcount handle); clone the
    // payloads out into owned values. The hashes are of the wire encodings —
    // exactly what a push would carry — so a hydrating client can skip
    // re-fetching what the document already shows.
    let page: ArticleView = (*(page_q.getter)(page_sub.current())).clone();
    let shell: SiteShell = (*(shell_q.getter)(shell_sub.current())).clone();
    SsrState {
        page_hash: crate::server::out_hash(&GearOut::ArticleLatestOut(page.clone())),
        page,
        shell_hash: crate::server::out_hash(&GearOut::ShellOut(shell.clone())),
        shell,
        // The resolved address: what the client needs to re-subscribe `page` /
        // `shell` without resolving the URL again.
        addr: PageAddr { site, slug },
        route,
    }
}

/// The built frontend's `index.html` as a string (decompressed), or `None` if
/// absent.
fn index_template(assets: &Arc<HashMap<String, Body>>) -> Option<String> {
    let body = assets.get("/index.html")?;
    let bytes = crate::assets::serve_body(body, false).bytes;
    String::from_utf8(bytes.into()).ok()
}
