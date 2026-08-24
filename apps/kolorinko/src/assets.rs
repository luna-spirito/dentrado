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

/// Before paying for a full-strength pass on a large blob, probe a middle slice
/// at a cheap level. Files larger than this are test-compressed first; if the
/// slice refuses to shrink, the whole file is almost certainly already
/// entropy-coded (JPEG/PNG/webp/woff2/video/…) and is stored raw instead of
/// burning CPU on a guaranteed-futile level-12 pass.
const PROBE_SIZE: usize = 64 * 1024;
const PROBE_LEVEL: i32 = 1;
/// Minimum sample compression ratio (compressed ÷ raw) below which a full pass
/// is worthwhile. `0.98` ≈ "the sample shrank by more than 2 %".
const PROBE_MIN_RATIO: f64 = 0.98;

/// Load the built frontend into a `path → Body` map, keyed by the served
/// request path, root-relative (`/index.html`, `/kolorinko_web.js`, … — the
/// files sit at the same relative paths under `dist/`). Each file is
/// zstd-compressed when that helps; `/index.html` gets the WT cert hash
/// injected **before** compression in hash-pinning mode.
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
/// Large blobs are first screened by [`likely_incompressible`]: a pre-compressed
/// image/font/video fails to shrink on a cheap probe of a middle slice, so we
/// skip the expensive full pass and store it raw.
///
/// [`RepoAsset`]: kolorinko_rt gear
pub(crate) fn compress(bytes: Vec<u8>) -> Body {
    if bytes.len() > PROBE_SIZE && likely_incompressible(&bytes) {
        return Body::Raw(Bytes::from(bytes));
    }
    match zstd::encode_all(&bytes[..], ZSTD_LEVEL) {
        Ok(z) if z.len() < bytes.len() => Body::Zstd(Bytes::from(z)),
        _ => Body::Raw(Bytes::from(bytes)),
    }
}

/// zstd-probe a deterministic middle slice of `bytes` at a cheap level; `true`
/// when it failed to compress — a strong signal the whole file is already
/// entropy-coded. Small files never reach here ([`compress`] only probes past
/// `PROBE_SIZE`); a probe failure is also treated as compressible (fall through
/// to the real pass) so a broken probe never loses compression.
fn likely_incompressible(bytes: &[u8]) -> bool {
    let mid = bytes.len() / 2;
    let start = mid.saturating_sub(PROBE_SIZE / 2);
    let end = (start + PROBE_SIZE).min(bytes.len());
    let sample = &bytes[start..end];
    let Ok(probed) = zstd::encode_all(sample, PROBE_LEVEL) else {
        return false;
    };
    (probed.len() as f64) / (sample.len() as f64) >= PROBE_MIN_RATIO
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

pub(crate) fn mime_for_ext(ext: &str) -> &'static str {
    match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "wasm" => "application/wasm",
        "css" => "text/css",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "eot" => "application/vnd.ms-fontobject",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// Map a request path to a MIME type by extension.
pub(crate) fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some(x) => mime_for_ext(x),
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

#[cfg(test)]
mod tests {
    use super::{Body::*, compress};

    /// Text (highly compressible) comes back as `Zstd`, even when large.
    #[test]
    fn large_text_is_compressed() {
        let big = "a".repeat(4 * super::PROBE_SIZE);
        assert!(matches!(compress(big.into_bytes()), Zstd(_)));
    }

    /// A large pre-compressed blob (random bytes model JPEG/PNG entropy) is
    /// stored `Raw` without paying for the full-strength pass.
    #[test]
    fn large_incompressible_is_raw() {
        assert!(matches!(compress(random(4 * super::PROBE_SIZE)), Raw(_)));
    }

    /// Small files skip the probe and rely on the ratio guard, so a small
    /// incompressible payload still resolves to `Raw`.
    #[test]
    fn small_incompressible_is_raw() {
        assert!(matches!(compress(random(512)), Raw(_)));
    }

    /// Deterministic pseudo-random bytes (high entropy, no RNG deps): models
    /// the payload of a pre-compressed image / font / video well enough to
    /// defeat zstd.
    fn random(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        let mut bytes = Vec::with_capacity(n);
        for _ in 0..n {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            bytes.push(s as u8);
        }
        bytes
    }
}
