//! Shared GET-response resolution for the HTTP/1.1 bootstrap ([`crate::web`])
//! and the HTTP/3 server ([`crate::server`]): one place that maps a request
//! path to a status/mime/body. Precedence:
//! 1. static assets (the built frontend, loaded at startup),
//! 2. `/repo/` mirrored content-addressed assets ([`crate::repo`]),
//! 3. page routes — SSR'd with the resolved page + shell ([`crate::ssr`]),
//! 4. anything else non-asset — the `index.html` CSR fallback,
//! 5. asset-like paths that don't exist — 404.

use std::{collections::HashMap, rc::Rc, sync::Arc};

use bytes::Bytes;
use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::{Body, parse_route};

use crate::assets::{Served, compress, looks_like_asset, mime_for, serve_body};
use crate::repo::{self, RepoResp};
use crate::runtime::KolorinkoRT;
use crate::wikidot_page::RepoMeta;

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
}

/// Resolve a GET `full` request path (query string included, if any) into a
/// [`Reply`]. `accept_zstd` picks the wire form of storable bodies; `host` —
/// the request's `host[:port]` — absolutizes SSR pages' OpenGraph URLs.
pub(crate) async fn resolve(
    full: &str,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    repo_meta: RepoMeta,
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
        None => match repo::serve(full, repo_meta.clone(), core).await {
            Some(RepoResp::Ok { mime, body }) => {
                Reply::ok(mime, serve_body(&body, accept_zstd), IMMUTABLE)
            }
            None if !looks_like_asset(key) => match parse_route(path) {
                Some((site, slug)) => {
                    match crate::ssr::document(assets, repo_meta, core, site, slug, host).await {
                        Some(html) => Reply::ok(
                            "text/html; charset=utf-8",
                            serve_body(&compress(html.into_bytes()), accept_zstd),
                            HTML,
                        ),
                        None => index_fallback(assets, accept_zstd),
                    }
                }
                None => index_fallback(assets, accept_zstd),
            },
            None => Reply::not_found(),
        },
    }
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
        }
    }
}
