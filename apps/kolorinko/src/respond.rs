//! Shared GET-response resolution for the HTTP/1.1 bootstrap ([`crate::web`])
//! and the HTTP/3 server ([`crate::server`]): one place that maps a request
//! path to a status/mime/body. Precedence:
//! 1. static assets (the built frontend, loaded at startup),
//! 2. `/repo/` mirrored content-addressed assets ([`crate::repo`]),
//! 3. the system namespace `/-…` — reserved (404 for now; future platform
//!    APIs and static files live here),
//! 4. canonical page routes `/SPACE/LOCAL[/slug]` — SSR'd from the resolved
//!    page + shell; a decorative slug 301s to the canonical two-segment form,
//!    a bare space 301s to its start page,
//! 5. legacy `/site[/cat/page]` routes — 301 to the canonical form when the
//!    site is registered as a space, else SSR'd in place (unregistered sites
//!    keep working),
//! 6. anything else non-asset — the `index.html` CSR fallback,
//! 7. asset-like paths that don't exist — 404.

use std::{collections::HashMap, rc::Rc, sync::Arc};

use bytes::Bytes;
use dentrado::core::{core_ctx::Core, gear::GearResult, storage::InMemoryStorage};
use kolorinko_rt::{
    Body, LocalId, PageAddr, SafePathComponent, Slug, SpaceId, SYSTEM_PREFIX, parse_canonical,
    parse_route,
};

use crate::assets::{Served, compress, looks_like_asset, mime_for, serve_body};
use crate::repo::{self, RepoResp};
use crate::runtime::{GearOutShared, KolorinkoRT, legacy_page_id, page_addr};
use kolorinko_rt::START_PAGE;

/// Cache-Control policies. CA assets (trunk-hashed outputs, `/repo/` blobs,
/// the legacy `wikidot-base-theme/**` tree — path-versioned on the rare change
/// → safe under `immutable`) are cached forever; HTML (the shell, SSR pages,
/// the SPA fallback) is `no-cache` since the ServiceWorker owns client-side
/// stale-while-revalidate; errors are `no-store`.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const HTML: &str = "no-cache";
const NOSTORE: &str = "no-store";

/// A resolved response: what to write, regardless of transport.
pub(crate) struct Reply {
    pub status: u16,
    pub mime: &'static str,
    pub served: Served,
    pub cache_control: &'static str,
    /// `Location` for redirect replies (301); absent otherwise.
    pub location: Option<String>,
}

/// Resolve a GET `full` request path (query string included, if any) into a
/// [`Reply`]. `accept_zstd` picks the wire form of storable bodies; `host` —
/// the request's `host[:port]` — absolutizes SSR pages' OpenGraph URLs.
pub(crate) async fn resolve(
    full: &str,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
) -> Reply {
    let path = full.split('?').next().unwrap_or(full);
    let key: &str = if path == "/" { "/index.html" } else { path };
    match assets.get(key) {
        Some(b) => Reply::ok(
            mime_for(key),
            serve_body(b, accept_zstd),
            static_policy(key),
        ),
        None => match repo::serve(full, core).await {
            Some(RepoResp::Ok { mime, body }) => {
                Reply::ok(mime, serve_body(&body, accept_zstd), IMMUTABLE)
            }
            None if !looks_like_asset(key) => route(path, accept_zstd, assets, core, host).await,
            None => Reply::not_found(),
        },
    }
}

/// Content routing after assets and `/repo/`: system namespace, canonical
/// spaces, legacy site paths, then the CSR fallback.
async fn route(
    path: &str,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
) -> Reply {
    // The system namespace (`/-/api`, `/-/static`, …) is reserved: never a
    // content id, never the SPA fallback. Nothing is served under it yet.
    if path.starts_with(SYSTEM_PREFIX) {
        return Reply::not_found();
    }

    let segs = segments(path);
    // Canonical: the first segment parses as a space id (exactly 22 base64url
    // chars — syntactically distinct from any legacy site name).
    if let Some(space) = segs.first().and_then(|s| SpaceId::parse(s)) {
        return canonical(&segs, space, accept_zstd, assets, core, host).await;
    }

    // Legacy `/site[/cat/page]`.
    if let Some((site, slug)) = parse_route(path) {
        // Registered site → its pages have canonical addresses; redirect.
        if let Some(target) = legacy_target(core, &site, &slug).await {
            return Reply::moved(&target);
        }
        return ssr(accept_zstd, assets, core, host, site, slug, None).await;
    }

    index_fallback(assets, accept_zstd)
}

/// `/SPACE/LOCAL[/slug…]` — the canonical page route family.
async fn canonical(
    segs: &[&str],
    space: SpaceId,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
) -> Reply {
    // A decorative slug (or anything beyond the local id) is not canonical:
    // 301 to the two-segment form. The slug is never inspected — it exists
    // only so human-readable links work.
    if segs.len() >= 3 {
        let Some(local) = segs.get(1).and_then(|s| LocalId::parse(s)) else {
            return Reply::not_found();
        };
        return Reply::moved(&format!("/{space}/{local}"));
    }
    match parse_canonical(&format!("/{space}/{}", segs.get(1).copied().unwrap_or_default())) {
        Some((space, local)) => match resolve_page_addr(core, space, local).await {
            Some(addr) => {
                let route = Some((space, local));
                ssr(
                    accept_zstd,
                    assets,
                    core,
                    host,
                    addr.site.clone(),
                    addr.slug.clone(),
                    route,
                )
                .await
            }
            // A well-formed canonical route for an unknown space/page is a
            // plain 404 — it looked like a page, not a SPA route.
            None => Reply::not_found(),
        },
        // Bare `/SPACE` (or a malformed local segment): redirect to the
        // space's start page when one exists.
        None => {
            let Some(site) = crate::globals::site_of(&space) else {
                return Reply::not_found();
            };
            let start = (
                None,
                SafePathComponent::new(START_PAGE.to_string()).expect("start is a safe name"),
            );
            match legacy_target(core, site, &start).await {
                Some(target) => Reply::moved(&target),
                None => Reply::not_found(),
            }
        }
    }
}

/// Resolve `(space, local)` through the [`PageAddr`](crate::runtime::page_addr)
/// gear: the dataset site + current slug serving that canonical address.
async fn resolve_page_addr(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    space: SpaceId,
    local: LocalId,
) -> Option<PageAddr> {
    let q = page_addr(space, local);
    let GearResult::Shared(s) = core.read_gear(q.id).await else {
        return None;
    };
    match &*s {
        GearOutShared::PageAddrOut(addr) => addr.clone(),
        _ => None,
    }
}

/// The canonical target for a legacy address: `/{space}/{local}` when the site
/// is registered and the page exists, else `None`.
async fn legacy_target(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<String> {
    let space = crate::globals::space_of(site)?;
    let q = legacy_page_id(site.clone(), slug.clone());
    let GearResult::Shared(s) = core.read_gear(q.id).await else {
        return None;
    };
    let GearOutShared::LegacyPageIdOut(Some(local)) = &*s else {
        return None;
    };
    Some(format!("/{space}/{local}"))
}

/// SSR the page at `(site, slug)`. `route` is the canonical address the URL
/// was served under (embedded into the SSR state so the client hydrates with
/// its subscription keys, no resolution round-trip).
async fn ssr(
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
    site: SafePathComponent,
    slug: Slug,
    route: Option<(SpaceId, LocalId)>,
) -> Reply {
    match crate::ssr::document(assets, core, site, slug, route, host).await {
        Some(html) => Reply::ok(
            "text/html; charset=utf-8",
            serve_body(&compress(html.into_bytes()), accept_zstd),
            HTML,
        ),
        None => index_fallback(assets, accept_zstd),
    }
}

/// Non-empty path segments, leading slash dropped (so `/a/b` and `a/b` agree;
/// interior empties like `/a//b` are kept as-is and simply never parse).
fn segments(path: &str) -> Vec<&str> {
    let mut s = path.strip_prefix('/').unwrap_or(path).split('/');
    
    Vec::from_iter(s.by_ref())
}

/// `no-cache` for the HTML shell and the SW script (both revalidated every
/// load); `immutable` for every other static asset (CA / path-versioned).
fn static_policy(key: &str) -> &'static str {
    match key {
        "/index.html" | "/sw.js" => HTML,
        _ => IMMUTABLE,
    }
}

fn index_fallback(assets: &Arc<HashMap<String, Body>>, accept_zstd: bool) -> Reply {
    Reply::ok(
        "text/html; charset=utf-8",
        serve_body(
            assets.get("/index.html").expect("index.html always loaded"),
            accept_zstd,
        ),
        HTML,
    )
}

impl Reply {
    fn ok(mime: &'static str, served: Served, cache_control: &'static str) -> Self {
        Self {
            status: 200,
            mime,
            served,
            cache_control,
            location: None,
        }
    }

    /// A permanent redirect to `location`.
    fn moved(location: &str) -> Self {
        Self {
            status: 301,
            mime: "text/plain",
            served: Served {
                bytes: Bytes::from_static(b"moved\n"),
                encoding: None,
            },
            cache_control: HTML,
            location: Some(location.to_string()),
        }
    }

    fn not_found() -> Self {
        Self {
            status: 404,
            mime: "text/plain",
            served: Served {
                bytes: Bytes::from_static(b"not found\n"),
                encoding: None,
            },
            cache_control: NOSTORE,
            location: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::segments;

    #[test]
    fn segments_split() {
        assert_eq!(segments("/a/b"), ["a", "b"]);
        assert_eq!(segments("/a/"), ["a", ""]);
        assert_eq!(segments("/"), [""]);
        assert_eq!(segments("/a//b"), ["a", "", "b"]);
    }
}
