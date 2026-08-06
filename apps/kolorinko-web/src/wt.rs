//! WebTransport client for kolorinko.
//!
//! The whole site is one origin — `https://<host>:<port>`. The page loads over
//! HTTP/1.1-over-TLS on the first visit, then over HTTP/3 once the browser
//! caches the `Alt-Svc` hint. This module opens a WebTransport session to that
//! **same origin** (`window.location.origin`) over one bidirectional stream
//! and speaks the dumb subscribe/cancel/push envelope from
//! [`kolorinko_rt::wire`].
//!
//! The client owns every `sub` handle (a monotonic `u64`): [`WtClient::subscribe`]
//! registers a typed callback under a fresh handle, sends `Subscribe { sub, id }`,
//! and the read loop decodes each `Update { sub, out }` via the `GearQuery`'s
//! `getter` before invoking the callback. [`WtClient::cancel`] sends `Cancel` and
//! drops the callback. The server tears down all of a disconnecting client's
//! subscriptions automatically.
//!
//! `allowPooling: true` *hints* the browser to run this session on an existing
//! HTTP/3 connection to the origin instead of opening a fresh QUIC connection.
//! It's only a hint — browsers may still use a dedicated connection.
//!
//! # Browser caveat
//! WebTransport rejects self-signed certificates outright (no interactive
//! acceptance like `wss://`). Local testing needs a *trusted* cert (e.g.
//! `mkcert localhost`) on the server, *or* the bootstrap-injected cert hash.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use kolorinko_rt::wire::{ClientMsg, GearOut, GearQuery, ServerMsg};
use leptos::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportOptions, WritableStreamDefaultWriter,
};

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

/// A typed update callback: decodes a raw `GearOut` via the `GearQuery`'s
/// `getter` and hands the typed value to the subscriber.
type UpdateCb = Rc<dyn Fn(GearOut)>;

/// A live WebTransport session with a registry of active subscriptions.
///
/// One instance per page (see [`connect_wt`]); share it across components via a
/// leptos signal/context. The read loop owns a clone of `subs` and routes every
/// `Update` to its callback; writes (subscribe/cancel) are fire-and-forget on
/// the shared writer — the JS `WritableStream` serializes queued writes, and
/// `spawn_local` schedules them in call order.
pub struct WtClient {
    writer: Rc<WritableStreamDefaultWriter>,
    next_sub: Cell<u64>,
    subs: Rc<RefCell<HashMap<u64, UpdateCb>>>,
}

impl WtClient {
    /// Subscribe to a gear query: register `on_update` under a fresh `sub`
    /// handle and send `Subscribe`. Returns the handle (for [`cancel`](Self::cancel)).
    pub fn subscribe<Out: 'static>(
        &self,
        query: GearQuery<Out>,
        on_update: impl Fn(Out) + 'static,
    ) -> u64 {
        let sub = self.next_sub.get();
        self.next_sub.set(sub.wrapping_add(1));
        let getter = query.getter;
        let cb: UpdateCb = Rc::new(move |out| on_update(getter(out)));
        self.subs.borrow_mut().insert(sub, cb);
        self.send(ClientMsg::Subscribe { sub, id: query.id });
        sub
    }

    /// Stop a subscription: send `Cancel` and drop the callback.
    pub fn cancel(&self, sub: u64) {
        self.subs.borrow_mut().remove(&sub);
        self.send(ClientMsg::Cancel { sub });
    }

    fn send(&self, msg: ClientMsg) {
        let Ok(json) = serde_json::to_string(&msg) else {
            return;
        };
        let frame = format!("{json}\n");
        let chunk = js_sys::Uint8Array::from(frame.as_bytes());
        let writer = self.writer.clone();
        spawn_local(async move {
            let _ = JsFuture::from(writer.write_with_chunk(&chunk)).await;
        });
    }
}

/// Open a WebTransport session to the page origin and return a [`WtClient`].
///
/// `set_status` receives connection-lifecycle messages; per-subscription content
/// arrives through callbacks registered via [`WtClient::subscribe`].
pub(crate) async fn connect_wt(
    set_status: WriteSignal<String>,
) -> Result<Rc<WtClient>, JsValue> {
    set_status.set("connecting (wt)…".into());

    let Some(window) = web_sys::window() else {
        set_status.set("no window".into());
        return Err(JsValue::from_str("no window"));
    };
    let url = window.location().origin()?;
    let options = WebTransportOptions::new();
    if let Some(hash) = cert_hash_from_window(&window) {
        // Hash pinning and connection pooling are mutually exclusive (the cert
        // hash is only meaningful on a dedicated connection), so pin WITHOUT
        // pooling.
        options.set_allow_pooling(false);
        let entry = web_sys::WebTransportHash::new();
        entry.set_algorithm("sha-256");
        entry.set_value_u8_array(&hash);
        options.set_server_certificate_hashes(&[entry]);
    } else {
        // No hash available: rely on normal CA trust and allow the browser to
        // pool onto an existing H3 connection.
        options.set_allow_pooling(true);
    }
    let wt = WebTransport::new_with_options(&url, &options)?;
    JsFuture::from(wt.ready()).await?;

    set_status.set("loading…".into());

    let bi_js = JsFuture::from(wt.create_bidirectional_stream()).await?;
    let bi: WebTransportBidirectionalStream = bi_js.dyn_into()?;
    let reader = ReadableStreamDefaultReader::new(&bi.readable())?;
    let writer = WritableStreamDefaultWriter::new(&bi.writable())?;
    let writer = Rc::new(writer);

    let subs: Rc<RefCell<HashMap<u64, UpdateCb>>> = Rc::new(RefCell::new(HashMap::new()));
    let route_subs = subs.clone();
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
                match serde_json::from_str::<ServerMsg>(line) {
                    Ok(ServerMsg::Update { sub, out }) => {
                        // Clone the `Rc` out of the borrow before calling, so a
                        // callback that (un)subscribes doesn't deadlock the map.
                        let cb = route_subs.borrow().get(&sub).cloned();
                        if let Some(cb) = cb {
                            cb(out);
                        }
                    }
                    Ok(ServerMsg::Dropped { sub }) => {
                        route_subs.borrow_mut().remove(&sub);
                    }
                    Err(e) => set_status.set(format!("decode: {e}")),
                }
            }
        }
    });

    // Keep the session alive for the page lifetime.
    std::mem::forget(wt);
    Ok(Rc::new(WtClient {
        writer,
        next_sub: Cell::new(1),
        subs,
    }))
}
