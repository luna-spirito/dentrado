//! WebTransport client for kolorinko.
//!
//! The whole site is one origin — `https://<host>:<port>` (default
//! `https://localhost:4433`). The page loads over HTTP/1.1-over-TLS on the
//! first visit, then over HTTP/3 once the browser caches the `Alt-Svc` hint.
//! This module opens a WebTransport session to that **same origin**
//! (`window.location.origin`), requests the default page over one
//! bidirectional stream, and streams newline-delimited JSON replies (initial
//! content + live pushes).
//!
//! `allowPooling: true` *hints* the browser to run this session on an existing
//! HTTP/3 connection to the origin (e.g. the one that loaded the page) instead
//! of opening a fresh QUIC connection. It's only a hint — browsers may still
//! use a dedicated connection.
//!
//! # Browser caveat
//! WebTransport rejects self-signed certificates outright (no interactive
//! acceptance like `wss://`). Local testing needs a *trusted* cert (e.g.
//! `mkcert localhost`) on the server.

use kolorinko_wikitext::Content;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportOptions, WritableStreamDefaultWriter,
};

/// Default page requested on connect: the Obscurative syntax lecture.
const DEFAULT_SITE: &str = "obscurative";
const DEFAULT_PAGE: &str = "syntax";

/// Read `window.__WT_CERT_HASH__` (a `Uint8Array` injected by the bootstrap) if
/// present, for WebTransport `serverCertificateHashes`.
fn cert_hash_from_window(window: &web_sys::Window) -> Option<js_sys::Uint8Array> {
    let val = js_sys::Reflect::get(window.as_ref(), &JsValue::from_str("__WT_CERT_HASH__"))
        .ok()?;
    if val.is_undefined() || val.is_null() {
        return None;
    }
    val.dyn_into::<js_sys::Uint8Array>().ok()
}

/// A client request (mirrors the server's `Request`).
#[derive(Serialize)]
#[serde(tag = "t")]
enum Request {
    #[serde(rename = "load")]
    Load {
        site: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        category: Option<String>,
        page: String,
    },
}

/// A server reply (mirrors the server's `Reply` — note `Repo { pages }`,
/// matching the server, unlike the legacy WS client's `Repo { path }`).
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Reply {
    #[serde(rename = "page")]
    Page { content: Content },
    #[serde(rename = "repo")]
    Repo { pages: usize },
    #[serde(rename = "error")]
    Error { error: String },
}

/// Open a WebTransport session, request the default page, and route replies
/// into the provided signals. Runs the read loop in a detached `spawn_local`
/// task and leaks the `WebTransport` handle so the session outlives this call
/// (same trick as the WS client's `mem::forget`).
pub(crate) async fn connect_wt(
    set_page: WriteSignal<Option<Content>>,
    set_title: WriteSignal<String>,
    set_status: WriteSignal<String>,
) -> Result<(), JsValue> {
    set_status.set("connecting (wt)…".into());

    let Some(window) = web_sys::window() else {
        set_status.set("no window".into());
        return Err(JsValue::from_str("no window"));
    };
    // Same origin the page loaded from (`https://<host>:<port>`); both the
    // HTTPS bootstrap and the H3+WT server live on this one origin.
    let url = window.location().origin()?;
    let options = WebTransportOptions::new();
    // Pin the server's short-lived self-signed cert by SHA-256: browsers won't
    // honor a local CA for the WebTransport QUIC handshake, so the bootstrap
    // page injects the cert hash as `window.__WT_CERT_HASH__` (a Uint8Array)
    // and we hand it to `serverCertificateHashes`, bypassing CA validation.
    if let Some(hash) = cert_hash_from_window(&window) {
        // Hash pinning and connection pooling are mutually exclusive (the cert
        // hash is only meaningful on a dedicated connection), so pin WITHOUT
        // pooling. Some Chromium versions reject `serverCertificateHashes` +
        // `allowPooling: true` outright.
        options.set_allow_pooling(false);
        let entry = web_sys::WebTransportHash::new();
        entry.set_algorithm("sha-256");
        entry.set_value_u8_array(&hash);
        options.set_server_certificate_hashes(&[entry]);
    } else {
        // No hash available: rely on normal CA trust (production / real cert)
        // and allow the browser to pool onto an existing H3 connection.
        options.set_allow_pooling(true);
    }
    let wt = WebTransport::new_with_options(&url, &options)?;;;
    // Wait for the session to be ready (TLS + QUIC handshake + WT CONNECT).
    JsFuture::from(wt.ready()).await?;

    set_title.set("Лекция Синтаксис".into());
    set_status.set("loading…".into());

    // Open the single bidi stream the server expects.
    let bi_js = JsFuture::from(wt.create_bidirectional_stream()).await?;
    let bi: WebTransportBidirectionalStream = bi_js.dyn_into()?;
    let reader = ReadableStreamDefaultReader::new(&bi.readable())?;
    let writer = WritableStreamDefaultWriter::new(&bi.writable())?;

    // Kick off the default page load.
    send_request(
        &writer,
        &Request::Load {
            site: DEFAULT_SITE.into(),
            category: None,
            page: DEFAULT_PAGE.into(),
        },
    )
    .await?;
    // The writer is no longer needed for the default flow; releasing its lock
    // keeps the underlying writable side open.
    writer.release_lock();

    // Detached read loop: reassemble NDJSON frames and route replies.
    spawn_local(async move {
        let mut buf = String::new();
        loop {
            let read_res = match JsFuture::from(reader.read()).await {
                Ok(r) => r,
                Err(e) => {
                    set_status.set(format!("wt read: {e:?}"));
                    break;
                }
            };
            let done = js_sys::Reflect::get(&read_res, &JsValue::from("done"))
                .map(|v| v.as_bool().unwrap_or(true))
                .unwrap_or(true);
            if done {
                set_status.set("wt closed".into());
                break;
            }
            let Ok(value) = js_sys::Reflect::get(&read_res, &JsValue::from("value")) else {
                continue;
            };
            let bytes = value.unchecked_into::<js_sys::Uint8Array>().to_vec();
            buf.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(i) = buf.find('\n') {
                let line: String = buf.drain(..=i).collect();
                let line = line.trim_end_matches('\n');
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Reply>(line) {
                    Ok(Reply::Page { content }) => {
                        set_page.set(Some(content));
                        set_status.set(String::new());
                    }
                    Ok(Reply::Repo { pages }) => {
                        set_status.set(format!("repo pages: {pages}"));
                    }
                    Ok(Reply::Error { error }) => {
                        set_status.set(format!("error: {error}"));
                    }
                    Err(e) => set_status.set(format!("decode: {e}")),
                }
            }
        }
    });

    // Keep the session alive for the page lifetime.
    std::mem::forget(wt);
    Ok(())
}

/// Serialize a `Request` and write it as one `json + '\n'` frame.
async fn send_request(
    writer: &WritableStreamDefaultWriter,
    req: &Request,
) -> Result<(), JsValue> {
    let json = serde_json::to_string(req)
        .map_err(|e| JsValue::from_str(&format!("encode request: {e}")))?;
    let frame = format!("{json}\n");
    let chunk = js_sys::Uint8Array::from(frame.as_bytes());
    JsFuture::from(writer.write_with_chunk(&chunk)).await?;
    Ok(())
}
