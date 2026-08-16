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

/// A resolved response: what to write, regardless of transport.
pub(crate) struct Reply {
    pub status: u16,
    pub mime: &'static str,
    pub served: Served,
}

/// Resolve a GET `full` request path (query string included, if any) into a
/// [`Reply`]. `accept_zstd` picks the wire form of storable bodies.
pub(crate) async fn resolve(
    full: &str,
    accept_zstd: bool,
    assets: &Arc<HashMap<String, Body>>,
    repo_meta: RepoMeta,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Reply {
    let path = full.split('?').next().unwrap_or(full);
    let key: &str = if path == "/" { "/index.html" } else { path };
    match assets.get(key) {
        Some(b) => Reply::ok(mime_for(key), serve_body(b, accept_zstd)),
        None => match repo::serve(full, repo_meta.clone(), core).await {
            Some(RepoResp::Ok { mime, body }) => Reply::ok(mime, serve_body(&body, accept_zstd)),
            None if !looks_like_asset(key) => match parse_route(path) {
                Some((site, slug)) => {
                    match crate::ssr::document(assets, repo_meta, core, site, slug).await {
                        Some(html) => Reply::ok(
                            "text/html; charset=utf-8",
                            serve_body(&compress(html.into_bytes()), accept_zstd),
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

fn index_fallback(assets: &Arc<HashMap<String, Body>>, accept_zstd: bool) -> Reply {
    Reply::ok(
        "text/html; charset=utf-8",
        serve_body(
            assets.get("/index.html").expect("index.html always loaded"),
            accept_zstd,
        ),
    )
}

impl Reply {
    fn ok(mime: &'static str, served: Served) -> Self {
        Self {
            status: 200,
            mime,
            served,
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
        }
    }
}
