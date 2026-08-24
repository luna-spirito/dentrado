//! Shared GET-response resolution for the HTTP/1.1 bootstrap ([`crate::web`])
//! and the HTTP/3 server ([`crate::server`]): one place that maps a request
//! path to a status/mime/body. Precedence:
//! 1. the system namespace `/-…` — the built frontend's static files and the
//!    mirrored content-addressed blobs under `/-/repo/…`
//!    ([`crate::repo`]); an unknown system path is a plain 404 (no content,
//!    no SPA fallback — future platform endpoints live here too). Served
//!    identically on every origin: asset URLs are root-relative and
//!    content-addressed, so a page's own origin serves its assets.
//! 2. `/SPACE/LOCAL[/TITLE]` — the canonical page route, SSR'd from the
//!    `article_latest(space, local)` + `shell(space)` cone (valid on any
//!    origin — the canonical address carries its own space),
//! 3. `/SPACE/[cat:]slug…` and a bare `/SPACE` — a page named by its slug:
//!    resolved and permanently redirected to the titled canonical form
//!    `/SPACE/LOCAL/TITLE` (the title regenerated from the page's own
//!    title),
//! 4. a configured custom domain (`Host` names a space — the wiki's own
//!    domain): the same page family without the space segment —
//!    `LOCAL[/TITLE]` canonical, `[cat:]slug…` redirected — and `/`
//!    SSR'ing the wiki's landing page as its homepage. No SPA shell on a
//!    wiki domain: the platform's client app is not this origin's face, an
//!    unknown page is a plain 404,
//! 5. `/` — SSR'd in place with the first registered space's landing page,
//! 6. anything else non-asset — the `/-/index.html` SPA shell (the client's
//!    not-found view),
//! 7. asset-like paths that don't exist — 404.
//!
//! The slug family of 3–4 also carries Wikidot's code-block endpoint:
//! `[cat:]slug…/code/N` serves the page's Nth `[[code]]` block in place
//! (never redirected, and never on the canonical `L…` address — a block has
//! no permanent URL). CSS `@import`s are rewritten to this shape at render
//! time ([`crate::wikidot_page::resources`]).
//!
//! Legacy `/site/cat/page` URLs are not served on the main origin: a first
//! segment that doesn't parse as a space id is just an unknown path (rule 6);
//! on a custom domain that same shape is the wiki's slug family (rule 4).

use std::{collections::HashMap, rc::Rc, sync::Arc};

use bytes::Bytes;
use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::{Body, LocalId, SYSTEM_PREFIX, SafePathComponent, SpaceId, format_page_route};

use crate::assets::{Served, compress, looks_like_asset, mime_for, serve_body};
use crate::repo::{self, RepoResp};
use crate::runtime::{KolorinkoRT, repo_l_article_latest};
use kolorinko_rt::Slug;

/// Cache-Control policies. CA assets (trunk-hashed outputs, `/-/repo/` blobs,
/// the base-theme tree — path-versioned on the rare change → safe under
/// `immutable`) are cached forever; HTML (the shell, SSR pages, the SPA
/// fallback) is `no-cache` since the ServiceWorker owns client-side
/// stale-while-revalidate; code blocks track the page's latest revision, so
/// they revalidate via their ETag; errors are `no-store`.
const IMMUTABLE: &str = "public, max-age=31536000, immutable";
const NOCACHE: &str = "no-cache";
const NOSTORE: &str = "no-store";

/// A resolved response: what to write, regardless of transport.
pub(crate) struct Reply {
    pub status: u16,
    pub mime: &'static str,
    pub served: Served,
    pub cache_control: &'static str,
    /// `Location` for redirect replies (301); absent otherwise.
    pub location: Option<String>,
    /// Strong ETag of `served`'s decoded body; set iff the reply is
    /// revalidatable (a code block). A matching `If-None-Match` collapses
    /// the reply to a 304 ([`Reply::revalidated`]).
    pub etag: Option<String>,
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
    // The system namespace: static files first (a plain map hit), then the
    // mirrored CA blobs, then 404 — never content routing, never the SPA
    // fallback. Content ids start with 'S'/'L', never '-', so the whole
    // `/-…` prefix is safely reserved.
    if path.starts_with(SYSTEM_PREFIX) {
        if let Some(b) = assets.get(path) {
            return Reply::ok(
                mime_for(path),
                serve_body(b, accept_zstd),
                static_policy(path),
            );
        }
        if let Some(RepoResp::Ok { mime, body }) = repo::serve(full, core).await {
            return Reply::ok(mime, serve_body(&body, accept_zstd), IMMUTABLE);
        }
        return Reply::not_found();
    }
    route(path, accept_zstd, assets, core, host).await
}

/// Content routing: canonical spaces, custom domains, then the SSR/SPA
/// fallbacks.
async fn route(
    path: &str,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
) -> Reply {
    let segs = segments(path);
    // `/SPACE/…` — the first segment parses as a space id (the marker char
    // makes this purely syntactic: no slug can imitate it — slugs are
    // lowercase, ids start with 'S').
    if let Some(space) = segs.first().and_then(|s| SpaceId::parse(s)) {
        return tail_route(&segs[1..], space, true, accept_zstd, assets, core, host).await;
    }
    // A wiki's own domain (`Host` names a registered space): the same page
    // family addressed without the space segment, and `/` — the wiki's
    // homepage — its landing page SSR'd in place.
    if let Some((space, reg)) = host.and_then(crate::globals::space_of_domain) {
        if segs.as_slice() == [""] {
            let slug = (None, reg.landing.clone());
            return match page_local(core, &space, &slug).await {
                Some((local, _title)) => ssr(accept_zstd, assets, core, host, space, local).await,
                None => Reply::not_found(),
            };
        }
        return tail_route(&segs, space, false, accept_zstd, assets, core, host).await;
    }
    // `/` — the first registered space's landing page, SSR'd in place.
    if segs.as_slice() == [""] {
        return match crate::globals::first_space() {
            Some((space, reg)) => {
                let slug = (None, reg.landing.clone());
                match page_local(core, &space, &slug).await {
                    Some((local, _title)) => {
                        ssr(accept_zstd, assets, core, host, space, local).await
                    }
                    None => index_fallback(assets, accept_zstd),
                }
            }
            None => index_fallback(assets, accept_zstd),
        };
    }
    // Anything else: the SPA shell for routes, a 404 for asset-like paths.
    if !looks_like_asset(path) {
        return index_fallback(assets, accept_zstd);
    }
    Reply::not_found()
}

/// The page family of one space, addressed by the segments after the space
/// segment: `LOCAL[/TITLE]` — the canonical route, SSR'd (the title is
/// decorative, never inspected); `[cat:]slug…` — a page named by its slug:
/// resolved and permanently redirected to its titled canonical form; a bare
/// tail — the space's landing page, same redirect. `explicit` — whether
/// redirect targets carry the space segment (the main origin) or rely on the
/// host already naming the space (the wiki's own domain).
async fn tail_route(
    tail: &[&str],
    space: SpaceId,
    explicit: bool,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
) -> Reply {
    if let Some(local) = tail.first().and_then(|s| LocalId::parse(s)) {
        if tail.len() > 2 {
            return Reply::not_found();
        }
        return ssr(accept_zstd, assets, core, host, space, local).await;
    }
    // `…/code/N` — Wikidot's code-block endpoint rides the slug family (on
    // either origin): the Nth `[[code]]` block served in place, never
    // redirected. Not on the canonical `L…` address above — a block has no
    // permanent URL of its own.
    if let Some((slug_tail, n)) = code_tail(tail)
        && let Some(slug) = slug_of(slug_tail, &space)
    {
        return code_reply(space, slug, n, accept_zstd, core).await;
    }
    let slug = match slug_of(tail, &space) {
        Some(slug) => slug,
        None => return Reply::not_found(),
    };
    match page_local(core, &space, &slug).await {
        // The title segment is regenerated from the page's own title, so a
        // rename never leaves a stale pretty URL behind.
        Some((local, title)) => {
            Reply::moved(&format_page_route(explicit.then_some(space), local, &title))
        }
        None => Reply::not_found(),
    }
}

/// The slug-family tail of a `…/code/N` request: the segments naming the
/// page (empty = the space's landing page, like Wikidot's site-root
/// `/code/N`) plus the 1-indexed block number. `None` unless the tail
/// literally ends in `/code/<u32>` — anything else flows on to the plain
/// slug routing.
fn code_tail<'a>(tail: &'a [&'a str]) -> Option<(&'a [&'a str], u32)> {
    match tail {
        [prefix @ .., "code", n] => Some((prefix, n.parse().ok()?)),
        _ => None,
    }
}

/// Serve one `…/code/N` request through the [`CodeBlock`] gear: the block's
/// interior, `text/css; charset=utf-8` for a `type="css"` block and
/// `text/plain` otherwise, under `no-cache` + the block's strong ETag (the
/// bytes track the page's latest revision, so clients revalidate instead of
/// caching forever). A missing page or block is a plain 404 — Wikidot's odd
/// `200 text/plain` "No valid codeblock found." serves no client better.
///
/// [`CodeBlock`]: kolorinko_rt gear
async fn code_reply(
    space: SpaceId,
    slug: Slug,
    n: u32,
    accept_zstd: bool,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Reply {
    let Some(site) = crate::globals::site_of(&space) else {
        return Reply::not_found();
    };
    let block = crate::runtime::code_block(site.clone(), slug, n)
        .subscribe(core)
        .await
        .current();
    let Some(block) = &*block else {
        return Reply::not_found();
    };
    Reply {
        status: 200,
        mime: if block.css {
            "text/css; charset=utf-8"
        } else {
            "text/plain; charset=utf-8"
        },
        served: serve_body(&block.body, accept_zstd),
        cache_control: NOCACHE,
        location: None,
        etag: Some(block.etag.clone()),
    }
}

/// The `(category, name)` slug named by the segments after the space id:
/// `cat:name`, `name`, or the old flattened `cat/name`; no segments at all
/// names the space's landing page.
fn slug_of(tail: &[&str], space: &SpaceId) -> Option<Slug> {
    let spc = |s: &str| SafePathComponent::new(s.to_string());
    match tail {
        [] => {
            let reg = crate::globals::reg_of(space)?;
            Some((None, reg.landing.clone()))
        }
        [x] => match x.split_once(':') {
            Some((cat, name)) => Some((Some(spc(cat)?), spc(name)?)),
            None => Some((None, spc(x)?)),
        },
        [a, b] => Some((Some(spc(a)?), spc(b)?)),
        _ => None,
    }
}

/// Resolve a slug within a registered space to its canonical address plus the
/// page's title (for the redirect's title segment). `None` when the space is
/// unregistered or the site has no such page.
async fn page_local(
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    space: &SpaceId,
    slug: &Slug,
) -> Option<(LocalId, String)> {
    let site = crate::globals::site_of(space)?.clone();
    let latest = repo_l_article_latest(site, slug.clone())
        .subscribe(core)
        .await
        .current();
    // A missing page projects to the empty `ArticleLatest` — an empty
    // `page_id` never parses, so it fails right here.
    let local = LocalId::from_page_id(&latest.meta.page_id)?;
    Some((local, latest.meta.title.clone()))
}

/// SSR the page at a canonical address. The 404 check is part of the deal: an
/// address that parses canonically but names nothing (unknown space, or a
/// local id the site has no page for) looked like a page, not a SPA route —
/// it answers as one.
async fn ssr(
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    host: Option<&str>,
    space: SpaceId,
    local: LocalId,
) -> Reply {
    let state = crate::ssr::state(core, space, local).await;
    if state.page.meta.page_id.is_empty() {
        return Reply::not_found();
    }
    match crate::ssr::document(assets, core, space, local, host).await {
        Some(html) => Reply::ok(
            "text/html; charset=utf-8",
            serve_body(&compress(html.into_bytes()), accept_zstd),
            NOCACHE,
        ),
        None => index_fallback(assets, accept_zstd),
    }
}

/// Non-empty path segments, leading slash dropped and trailing slashes
/// trimmed (so `/a/b`, `/a/b/`, and `a/b` agree; interior empties like
/// `/a//b` are kept as-is and simply never parse).
fn segments(path: &str) -> Vec<&str> {
    let p = path.trim_end_matches('/');
    let s = p.strip_prefix('/').unwrap_or(p).split('/');
    Vec::from_iter(s)
}

/// `no-cache` for the HTML shell and the SW script (both revalidated every
/// load); `immutable` for every other static asset (CA / path-versioned).
fn static_policy(key: &str) -> &'static str {
    match key {
        "/-/index.html" | "/-/sw.js" => NOCACHE,
        _ => IMMUTABLE,
    }
}

fn index_fallback(assets: &Arc<HashMap<String, Body>>, accept_zstd: bool) -> Reply {
    Reply::ok(
        "text/html; charset=utf-8",
        serve_body(
            assets
                .get("/-/index.html")
                .expect("index.html always loaded"),
            accept_zstd,
        ),
        NOCACHE,
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
            etag: None,
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
            cache_control: NOCACHE,
            location: Some(location.to_string()),
            etag: None,
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
            etag: None,
        }
    }

    /// Collapse to a 304 when the request's `If-None-Match` matches this
    /// reply's ETag; the caller passes the raw header value (`None`: no
    /// header). The 304 keeps the `ETag` and `Cache-Control` headers (what a
    /// revalidation is for) and drops the body.
    pub(crate) fn revalidated(self, if_none_match: Option<&str>) -> Self {
        let Some(etag) = self.etag.clone() else {
            return self;
        };
        if if_none_match.is_some_and(|h| matches_etag(h, &etag)) {
            return Self {
                status: 304,
                mime: "text/plain",
                served: Served {
                    bytes: Bytes::new(),
                    encoding: None,
                },
                cache_control: self.cache_control,
                location: None,
                etag: Some(etag),
            };
        }
        self
    }
}

/// RFC 9110 `If-None-Match` vs a strong ETag: `*` matches anything, and a
/// weak `W/"…"` entry compares by opaque value after stripping the prefix
/// (a 304 answer is always a weak comparison).
fn matches_etag(header: &str, etag: &str) -> bool {
    header.split(',').any(|t| {
        let t = t.trim();
        t == "*" || t.strip_prefix("W/").unwrap_or(t) == etag
    })
}

#[cfg(test)]
mod tests {
    use super::{Slug, segments, slug_of};
    use kolorinko_rt::SpaceId;

    #[test]
    fn segments_split() {
        assert_eq!(segments("/a/b"), ["a", "b"]);
        // Trailing slashes are insignificant on content routes.
        assert_eq!(segments("/a/"), ["a"]);
        assert_eq!(segments("/a//"), ["a"]);
        assert_eq!(segments("/"), [""]);
        assert_eq!(segments("//"), [""]);
        assert_eq!(segments("/a//b"), ["a", "", "b"]);
    }

    /// `slug_of` as `(Option<category>, name)` strings, for plain asserts.
    fn strs(s: Option<Slug>) -> Option<(Option<String>, String)> {
        s.map(|(cat, name)| (cat.map(|c| c.to_string()), name.to_string()))
    }

    #[test]
    fn slug_of_shapes() {
        let space = SpaceId::parse("S70P6lbBZxbc-kcpGOCYmZA").unwrap();
        let slug = |tail: &[&str]| strs(slug_of(tail, &space));
        // `name`, `cat:name`, and the old flattened `cat/name`.
        assert_eq!(slug(&["name"]), Some((None, "name".into())));
        assert_eq!(
            slug(&["cat:name"]),
            Some((Some("cat".into()), "name".into()))
        );
        assert_eq!(
            slug(&["cat", "name"]),
            Some((Some("cat".into()), "name".into()))
        );
        // Deeper than any legacy form, or an empty name — never a slug.
        assert_eq!(slug(&["a", "b", "c"]), None);
        assert_eq!(slug(&[""]), None);
        assert_eq!(slug(&["cat:"]), None);
        assert_eq!(slug(&[":name"]), None);
    }

    #[test]
    fn code_tail_shapes() {
        // The page-naming segments' count plus the parsed block number.
        let t = |tail: &[&str]| super::code_tail(tail).map(|(p, n)| (p.len(), n));
        assert_eq!(t(&["component:theme", "code", "1"]), Some((1, 1)));
        assert_eq!(t(&["cat", "name", "code", "40"]), Some((2, 40)));
        // No page segments: the landing page's block, like Wikidot's
        // site-root `/code/N`.
        assert_eq!(t(&["code", "1"]), Some((0, 1)));
        // Not the endpoint shape: a flattened `x/code` slug, a non-numeric N,
        // too few segments.
        assert_eq!(t(&["x", "code"]), None);
        assert_eq!(t(&["x", "code", "a"]), None);
        assert_eq!(t(&["x"]), None);
    }
}
