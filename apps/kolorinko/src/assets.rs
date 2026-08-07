//! Frontend asset loading, shared by the HTTP/1.1 bootstrap ([`crate::web`])
//! and the HTTP/3 server ([`crate::server`]).
//!
//! Done once per worker at startup. Uses blocking `std::fs` because it is a
//! one-time read of (typically) a few small files, well under a millisecond.
//!
//! In WebTransport hash-pinning mode, [`load_assets`] also injects the WT cert
//! hash into the cached `index.html` **once** here (rather than on every
//! request) so the wasm client can pin the self-signed QUIC cert via
//! `serverCertificateHashes`.

use std::{collections::HashMap, path::Path};

use log::{error, warn};

/// Minimal placeholder served when no built frontend is present, so the server
/// is usable before `trunk build` has run.
pub(crate) const PLACEHOLDER_INDEX: &str = "<!doctype html>\
<html><head><meta charset=\"utf-8\"><title>kolorinko</title></head>\
<body><h1>kolorinko</h1>\
<p>No built frontend found. Build it with \
<code>trunk build</code> in <code>apps/kolorinko-web</code>.</p></body></html>";

/// Load the built frontend into a `path → bytes` map, keyed by request path
/// (e.g. `/index.html`, `/pkg/kolorinko_web.js`).
pub(crate) fn load_assets(dir: &Path, wt_hash: Option<&[u8]>) -> HashMap<String, Vec<u8>> {
    let mut map = HashMap::new();
    if dir.is_dir() {
        walk(dir, dir, &mut map);
    } else {
        warn!(
            "frontend dir {} not found; serving a placeholder page",
            dir.display()
        );
    }
    map.entry("/index.html".to_string())
        .or_insert_with(|| PLACEHOLDER_INDEX.as_bytes().to_vec());
    // Inject the WebTransport cert hash into the cached `index.html` **once**
    // here (not on every request), so the wasm client can pin the self-signed
    // QUIC cert via `serverCertificateHashes`. Only in hash-pinning mode; in
    // pooling mode the QUIC endpoint presents the CA-trusted cert and no hash
    // is needed.
    if let Some(hash) = wt_hash
        && let Some(html) = map.get_mut("/index.html")
    {
        *html = inject_wt_hash(html, hash);
    }
    map
}

fn walk(root: &Path, dir: &Path, map: &mut HashMap<String, Vec<u8>>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(root, &path, map);
        } else if let Ok(rel) = path.strip_prefix(root)
            && let Ok(bytes) = std::fs::read(&path)
        {
            let key = format!("/{}", rel.to_string_lossy().replace('\\', "/"));
            map.insert(key, bytes);
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
// Done once into the cached `index.html` (see [`load_assets`]), not per request.

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
