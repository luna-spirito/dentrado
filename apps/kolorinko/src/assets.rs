//! Frontend asset loading, shared by the HTTP/1.1 bootstrap ([`crate::web`])
//! and the HTTP/3 server ([`crate::server`]).
//!
//! Done **once** at startup (blocking `std::fs`: a one-time read of a few small
//! files), then shared across every core as a single `Arc<HashMap<String,
//! Body>>` — each core holds a refcount bump, never its own copy of the bytes.
//!
//! Each asset is zstd-compressed once here when that shrinks it; the stored
//! [`Body`] is then served verbatim with `Content-Encoding: zstd` to clients
//! that accept it (decompressed server-side for the rest), so the hot path is a
//! refcount bump + a write, never a re-compress.
//!
//! In WebTransport hash-pinning mode, [`load_assets`] also injects the WT cert
//! hash into the cached `index.html` **once** here (before compression) so the
//! wasm client can pin the self-signed QUIC cert via `serverCertificateHashes`.

use std::{collections::HashMap, path::Path};

use bytes::Bytes;
use kolorinko_rt::Body;
use log::{error, warn};

/// Minimal placeholder served when no built frontend is present, so the server
/// is usable before `trunk build` has run.
pub(crate) const PLACEHOLDER_INDEX: &str = "<!doctype html>\
<html><head><meta charset=\"utf-8\"><title>kolorinko</title></head>\
<body><h1>kolorinko</h1>\
<p>No built frontend found. Build it with \
<code>trunk build</code> in <code>apps/kolorinko-web</code>.</p></body></html>";

/// zstd compression level for one-shot asset compression. Assets are compressed
/// once at load (statics) or once per gear run (dynamics, cached thereafter), so
/// a fairly high level pays for itself in transfer size.
const ZSTD_LEVEL: i32 = 12;

/// Load the built frontend into a `path → Body` map, keyed by request path
/// (e.g. `/index.html`, `/pkg/kolorinko_web.js`). Each file is zstd-compressed
/// when that helps; `/index.html` gets the WT cert hash injected **before**
/// compression in hash-pinning mode.
pub(crate) fn load_assets(dir: &Path, wt_hash: Option<&[u8]>) -> HashMap<String, Body> {
    let mut map = HashMap::new();
    if dir.is_dir() {
        walk(dir, dir, wt_hash, &mut map);
    } else {
        warn!(
            "frontend dir {} not found; serving a placeholder page",
            dir.display()
        );
    }
    map.entry("/index.html".to_string())
        .or_insert_with(|| compress(PLACEHOLDER_INDEX.as_bytes().to_vec()));
    map
}

/// zstd-compress `bytes` when that shrinks them, otherwise keep them raw. The
/// result is shared behind a [`Bytes`] so every serve is a refcount bump.
/// Shared with the [`RepoAsset`] gear ([`crate::wikidot_page`]).
///
/// [`RepoAsset`]: kolorinko_rt gear
pub(crate) fn compress(bytes: Vec<u8>) -> Body {
    match zstd::encode_all(&bytes[..], ZSTD_LEVEL) {
        Ok(z) if z.len() < bytes.len() => Body::Zstd(Bytes::from(z)),
        _ => Body::Raw(Bytes::from(bytes)),
    }
}

/// The bytes to write for a response, plus the `Content-Encoding` to advertise
/// (if any). Built from a [`Body`] by honoring the client's `Accept-Encoding`.
pub(crate) struct Served {
    pub bytes: Bytes,
    pub encoding: Option<&'static str>,
}

/// Pick the wire form of `body` for a client that accepts zstd iff
/// `accept_zstd`. A `Zstd` body is sent verbatim with `Content-Encoding: zstd`
/// to zstd-capable clients and decompressed server-side for the rest; a `Raw`
/// body is always sent as-is.
pub(crate) fn serve_body(body: &Body, accept_zstd: bool) -> Served {
    match body {
        Body::Raw(b) => Served {
            bytes: b.clone(),
            encoding: None,
        },
        Body::Zstd(b) if accept_zstd => Served {
            bytes: b.clone(),
            encoding: Some("zstd"),
        },
        // Client won't accept zstd: decompress once here. We encoded it
        // ourselves, so failure is impossible in practice — fall back to the
        // raw compressed bytes (no encoding) rather than panic on a corrupt
        // cache.
        Body::Zstd(b) => Served {
            bytes: Bytes::from(zstd::decode_all(&b[..]).unwrap_or_else(|_| b.clone().into())),
            encoding: None,
        },
    }
}

fn walk(root: &Path, dir: &Path, wt_hash: Option<&[u8]>, map: &mut HashMap<String, Body>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, wt_hash, map);
        } else if let Ok(rel) = path.strip_prefix(root)
            && let Ok(bytes) = std::fs::read(&path)
        {
            let key = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            let bytes = if key == "/index.html"
                && let Some(hash) = wt_hash
            {
                inject_wt_hash(&bytes, hash)
            } else {
                bytes
            };
            map.insert(key, compress(bytes));
        } else if let Err(e) = path.strip_prefix(root) {
            error!("asset {}: {e}", path.display());
        }
    }
}

/// Map a request path to a MIME type by extension.
pub(crate) fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("eot") => "application/vnd.ms-fontobject",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("pdf") => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Whether a path looks like a static asset (a known file extension) rather
/// than a client-side route. Used for the SPA fallback: a missing asset 404s,
/// but a missing *route* (`/obscurative/syntax`, …) serves `index.html` so the
/// wasm app boots and the router takes over.
pub(crate) fn looks_like_asset(path: &str) -> bool {
    let Some(ext) = path
        .rsplit('/')
        .next()
        .and_then(|s| s.rsplit_once('.').map(|x| x.1))
    else {
        return false;
    };
    matches!(
        ext,
        "html" | "htm" | "js" | "mjs" | "wasm" | "css" | "json" | "svg" | "png" | "ico"
    )
}

// ---- WebTransport cert-hash injection (hash-pinning mode) -------------------
//
// Done once into the raw `index.html` bytes (see [`load_assets`]), before
// zstd compression — not per request.

/// Inject `<script>window.__WT_CERT_HASH__=new Uint8Array([...]);</script>` into
/// the page right after the opening `<head>`, so the wasm WebTransport client
/// can pin the server's self-signed cert via `serverCertificateHashes`.
fn inject_wt_hash(html: &[u8], hash: &[u8]) -> Vec<u8> {
    let script = format!(
        "<script>window.__WT_CERT_HASH__=new Uint8Array([{}]);</script>",
        hash.iter()
            .map(|b| b.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let mut out = Vec::with_capacity(html.len() + script.len());
    match find_head_open(html) {
        Some(pos) => {
            out.extend_from_slice(&html[..pos]);
            out.extend_from_slice(script.as_bytes());
            out.extend_from_slice(&html[pos..]);
        }
        None => {
            // No `<head>` (shouldn't happen for our pages); prepend defensively.
            out.extend_from_slice(script.as_bytes());
            out.extend_from_slice(html);
        }
    }
    out
}

/// Byte offset just past the first `<head ...>` tag (case-insensitive), if any.
fn find_head_open(html: &[u8]) -> Option<usize> {
    let start = html
        .windows(b"<head".len())
        .position(|w| w.eq_ignore_ascii_case(b"<head"))?;
    let gt = html[start..].iter().position(|&b| b == b'>')?;
    Some(start + gt + 1)
}
