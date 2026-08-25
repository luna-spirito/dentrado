//! SSR page rendering: resolve a canonical route's page + site shell through
//! the gear runtime — the same `article_latest(space, local)` / `shell(space)`
//! cone the WebTransport client subscribes to — and seal them into the served
//! document (see [`kolorinko_render::render_ssr_document`]).

use std::{collections::HashMap, rc::Rc, sync::Arc};

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::{Body, LocalId, SiteShell, SpaceId, SsrState, format_page_route, wire::GearOut};
use kolorinko_wikitext::ArticleView;

use crate::runtime::{KolorinkoRT, article_latest, shell};

/// Resolve and render the full SSR document for the canonical address
/// `(space, local)`, or `None` when the frontend template can't host SSR
/// output (no app placeholder — unbuilt frontend); the caller then falls back
/// to serving the plain shell. `host` — the request's `host[:port]` —
/// absolutizes the OpenGraph card's URLs, names the page's canonical address
/// (`og:url`: without the space segment when the host is the space's own
/// configured domain, with it elsewhere), and, on such a domain, rides into
/// the document as `window.__DEFAULT_SPACE_ID__` (see
/// [`kolorinko_rt::DEFAULT_SPACE_GLOBAL`]).
pub(crate) async fn document(
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    space: SpaceId,
    local: LocalId,
    host: Option<&str>,
) -> Option<String> {
    let state = state(core, space, local).await;
    let index = index_template(assets)?;
    // The page's canonical URL — `og:url`. The space segment is dropped when
    // the request's host is the space's own domain: there the space-less
    // path is the canonical address.
    let own_domain = |h: &str| crate::globals::space_of_domain(h).is_some_and(|(s, _)| s == space);
    let canonical = host.map(|h| {
        format!(
            "https://{h}{}",
            format_page_route(
                (!own_domain(h)).then_some(space),
                local,
                &state.page.meta.title
            )
        )
    });
    // The space the host itself names (whatever page it renders) — the
    // client collapses `/S<default>/…` hrefs to `/…` against it.
    let default_space = host
        .and_then(crate::globals::space_of_domain)
        .map(|(s, _)| s);
    kolorinko_render::render_ssr_document(&index, &state, host, canonical.as_deref(), default_space)
}

/// Resolve the page and shell for one canonical address. Subscribes both
/// before reading either — holding both keeps the shared `repo` oracle active
/// across the two queries (one clone total), exactly as the render CLI does.
/// Awaits the gears' current outputs: the same resolution a CSR boot blocks
/// on over WebTransport, just server-side. The hashes are of the wire
/// encodings — exactly what a push would carry — so a hydrating client can
/// skip re-fetching what the document already shows.
pub(crate) async fn state(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    space: SpaceId,
    local: LocalId,
) -> SsrState {
    let page_q = article_latest(space, local);
    let shell_q = shell(space);
    let page_sub = page_q.subscribe(core).await;
    let shell_sub = shell_q.subscribe(core).await;
    // `current()` yields the `SharedView<…>` projections (a `!Send` refcount
    // handle); clone the payloads out into owned values.
    let page: ArticleView = (*page_sub.current()).clone();
    let shell: SiteShell = (*shell_sub.current()).clone();
    SsrState {
        page_hash: crate::server::out_hash(&GearOut::ArticleLatestOut(page.clone())),
        page,
        shell_hash: crate::server::out_hash(&GearOut::ShellOut(shell.clone())),
        shell,
        space,
        local,
    }
}

/// The platform's about screen as a served document — SSR'd into the same
/// shell template every page uses (see
/// [`kolorinko_render::render_about_document`]) — or `None` when the
/// frontend template can't host SSR output; the caller then falls back to
/// serving the plain shell.
pub(crate) fn about_document(
    assets: &Arc<HashMap<String, Body>>,
    host: Option<&str>,
) -> Option<String> {
    kolorinko_render::render_about_document(&index_template(assets)?, host)
}

/// The built frontend's `/index.html` as a string (decompressed), or `None`
/// if absent.
fn index_template(assets: &Arc<HashMap<String, Body>>) -> Option<String> {
    let body = assets.get("/index.html")?;
    let bytes = crate::assets::serve_body(body, false).bytes;
    String::from_utf8(bytes.into()).ok()
}
