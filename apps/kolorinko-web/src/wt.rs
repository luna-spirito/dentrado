//! WebTransport client for kolorinko.
//!
//! The whole site is one origin — `https://<host>:<port>`. The page loads over
//! HTTP/1.1-over-TLS on the first visit, then over HTTP/3 once the browser
//! caches the `Alt-Svc` hint. This module opens a WebTransport session to that
//! **same origin** (`window.location.origin`) and speaks the subscribe/push
//! envelope from [`kolorinko_rt::wire`]: **one bidirectional stream per
//! subscription** — [`WtClient::subscribe`] opens a stream, sends the stream's
//! single `Subscribe { id, hash }` frame, and the stream's read loop feeds
//! typed updates to the callback until either side closes it. Closing the
//! stream *is* the cancel/dropped signal; there are no other control messages.
//!
//! # Reconnection
//! Sessions don't survive server restarts or network blips, so [`connect`]
//! spawns a supervisor that re-establishes the session forever. The registry —
//! not the connection — is the source of truth: a fresh session replays every
//! live subscription on its own fresh stream, while per-stream work on a dead
//! session ends on its own. The reconnect delay doubles per attempt and resets
//! once a session stayed up for a while.
//!
//! # Content hashes
//! The server hashes every push (SHA-256 of the wire `GearOut` JSON); the
//! client just remembers the last hash per subscription and echoes it in
//! `Subscribe` — on reconnect *and* on SSR hydration (the hashes ride in
//! [`SsrState`]). A matching hash means the server sends nothing until the
//! content actually changes, so unchanged payloads cross the wire zero times.
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

use kolorinko_rt::wire::{ClientMsg, GearId, GearOut, GearQuery, ServerMsg};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    ReadableStreamDefaultReader, WebTransport, WebTransportBidirectionalStream,
    WebTransportOptions, WritableStreamDefaultWriter,
};
// `WebTransportCloseInfo` feature is enabled for `WebTransport::closed`.

/// First reconnect delay; doubles per failed attempt, capped.
const BACKOFF_MIN_MS: i32 = 250;
const BACKOFF_MAX_MS: i32 = 5_000;
/// A session that stayed up at least this long resets the backoff.
const BACKOFF_RESET_MS: f64 = 10_000.0;

/// Read `window.__WT_CERT_HASH__` (a `Uint8Array` injected by the bootstrap) if
/// present, for WebTransport `serverCertificateHashes`.
fn cert_hash_from_window(window: &web_sys::Window) -> Option<js_sys::Uint8Array> {
    let val = js_sys::Reflect::get(window.as_ref(), &JsValue::from_str("__WT_CERT_HASH__")).ok()?;
    if val.is_undefined() || val.is_null() {
        return None;
    }
    val.dyn_into::<js_sys::Uint8Array>().ok()
}

/// A typed update callback: decodes a raw `GearOut` via the `GearQuery`'s
/// `getter` and hands the typed value to the subscriber.
type UpdateCb = Rc<dyn Fn(GearOut)>;

/// One live subscription: what to (re)subscribe to, where updates go, and the
/// hash of the content the client last held. Its stream writer (present only
/// while its stream is up) doubles as the cancel handle — closing it half-
/// closes the stream, which the server reads as a cancel.
struct Registration {
    id: GearId,
    cb: UpdateCb,
    hash: RefCell<Option<String>>,
    writer: RefCell<Option<Rc<WritableStreamDefaultWriter>>>,
}

/// Client state shared by [`WtClient`], the supervisor, and per-stream tasks:
/// the live session transport (absent between sessions — also the liveness
/// flag for stream teardown decisions), the session generation counter (so a stale
/// stream task from a dead session can't mistake the next session for its
/// own), the sub-handle counter, and the subscription registry. Because the
/// registry — not the connection — is the source of truth, subscriptions
/// survive reconnects.
struct Inner {
    transport: RefCell<Option<WebTransport>>,
    session_gen: Cell<u64>,
    next_sub: Cell<u64>,
    subs: RefCell<HashMap<u64, Rc<Registration>>>,
}

/// A WebTransport client whose subscriptions outlive any one session.
///
/// One instance per page (see [`connect`]); share it across components via a
/// leptos signal/context. Each subscription's stream read loop routes pushes
/// to its callback; the one client→server write per stream (the `Subscribe`
/// frame) is fire-and-forget on the stream's own writer.
pub struct WtClient {
    inner: Rc<Inner>,
}

impl WtClient {
    /// Subscribe to a gear query: register `on_update` under a fresh handle
    /// and, while a session is up, open its stream and send `Subscribe`.
    /// `known` is the hash of the content the caller already holds (SSR
    /// hydration), if any; the server then skips re-sending it. Returns the
    /// handle for [`cancel`](Self::cancel). Safe while disconnected: the
    /// registration is replayed once the next session is up.
    pub fn subscribe<Out: 'static>(
        &self,
        query: GearQuery<Out>,
        known: Option<&str>,
        on_update: impl Fn(Out) + 'static,
    ) -> u64 {
        let sub = self.inner.next_sub.get();
        self.inner.next_sub.set(sub.wrapping_add(1));
        let id = query.id().clone();
        let extract = move |out| query.extract(out);
        let cb: UpdateCb = Rc::new(move |out| on_update(extract(out)));
        let reg = Rc::new(Registration {
            id,
            cb,
            hash: RefCell::new(known.map(str::to_owned)),
            writer: RefCell::new(None),
        });
        self.inner.subs.borrow_mut().insert(sub, reg.clone());
        spawn_stream(self.inner.clone(), sub, reg);
        sub
    }

    /// Stop a subscription: drop the registration (so it isn't replayed on
    /// reconnect) and half-close its stream — the server reads that as a
    /// cancel and releases the gear subscription.
    pub fn cancel(&self, sub: u64) {
        if let Some(reg) = self.inner.subs.borrow_mut().remove(&sub)
            && let Some(writer) = reg.writer.borrow_mut().take()
        {
            spawn_local(async move {
                let _ = JsFuture::from(writer.close()).await;
            });
        }
    }
}

/// The app's single WebTransport client. Subscriptions register locally right
/// away; the supervisor (re)connects in the background and replays them on
/// every fresh session. Connection problems are retried with backoff and
/// logged to the browser console; only a permanent failure (no window,
/// unsupported transport) gives up.
pub(crate) fn connect() -> Rc<WtClient> {
    let inner = Rc::new(Inner {
        transport: RefCell::new(None),
        session_gen: Cell::new(0),
        next_sub: Cell::new(1),
        subs: RefCell::new(HashMap::new()),
    });
    spawn_local(supervise(inner.clone()));
    Rc::new(WtClient { inner })
}

/// Own the session lifecycle: connect, serve until the session closes, sleep,
/// repeat. [`session`] returns `Err` only for conditions retrying can't fix.
async fn supervise(inner: Rc<Inner>) {
    let mut backoff = BACKOFF_MIN_MS;
    loop {
        let opened_at = js_sys::Date::now();
        match session(&inner).await {
            Ok(()) => {}
            Err(e) => {
                leptos::logging::warn!("wt: {e:?}");
                return;
            }
        }
        if js_sys::Date::now() - opened_at >= BACKOFF_RESET_MS {
            backoff = BACKOFF_MIN_MS;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_MS);
    }
}

/// Open one session to the page origin, replay the registry (one stream per
/// subscription), and await the session's end. Setup failures retrying can't
/// fix (no window, unsupported transport, bad URL) are `Err`; everything after
/// — including `ready()` — just ends the session, leaving the reconnect to
/// [`supervise`].
async fn session(inner: &Rc<Inner>) -> Result<(), JsValue> {
    let Some(window) = web_sys::window() else {
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
    // Constructor failure (unsupported transport, unparseable URL) is
    // permanent; network problems surface later, at `ready()`.
    let wt = WebTransport::new_with_options(&url, &options)?;
    if let Err(e) = JsFuture::from(wt.ready()).await {
        leptos::logging::warn!("wt connect: {e:?}");
        return Ok(());
    }

    inner
        .session_gen
        .set(inner.session_gen.get().wrapping_add(1));
    *inner.transport.borrow_mut() = Some(wt.clone());
    // Replay the registry: each subscription gets a fresh stream and echoes
    // its last-held hash, so unchanged content isn't re-sent.
    for (&sub, reg) in inner.subs.borrow().iter() {
        spawn_stream(inner.clone(), sub, reg.clone());
    }

    // Settles (resolve or reject) when the session ends; nobody else needs
    // the close info, and this future observes the rejection.
    let _ = JsFuture::from(wt.closed()).await;
    leptos::logging::warn!("wt session closed");
    *inner.transport.borrow_mut() = None;
    Ok(())
}

/// Log a per-stream failure and force a session rebuild: take (and close)
/// the transport so the supervisor's `closed` await settles and it replays
/// every registration on a fresh session. Broken-while-healthy is rare —
/// usually it means the session just died — so one rebuild covers both.
async fn break_session(inner: &Rc<Inner>, e: JsValue) {
    leptos::logging::warn!("wt stream: {e:?}");
    if let Some(wt) = inner.transport.borrow_mut().take() {
        wt.close();
    }
}

/// Open the stream for one registration and drive its read loop; a no-op if
/// no session is up (the supervisor's replay will handle it). Any stream
/// failure escalates to a session rebuild — the supervisor then replays every
/// registration, this one included.
fn spawn_stream(inner: Rc<Inner>, sub: u64, reg: Rc<Registration>) {
    let Some(wt) = inner.transport.borrow().clone() else {
        return;
    };
    let epoch = inner.session_gen.get();
    spawn_local(async move {
        let bi = match JsFuture::from(wt.create_bidirectional_stream()).await {
            Ok(bi) => bi.dyn_into::<WebTransportBidirectionalStream>().unwrap(),
            Err(e) => return break_session(&inner, e).await,
        };
        let reader = match ReadableStreamDefaultReader::new(&bi.readable()) {
            Ok(r) => r,
            Err(e) => return break_session(&inner, e).await,
        };
        let writer = match WritableStreamDefaultWriter::new(&bi.writable()) {
            Ok(w) => Rc::new(w),
            Err(e) => return break_session(&inner, e).await,
        };
        *reg.writer.borrow_mut() = Some(writer.clone());
        // The stream's single outgoing frame. A rejection means the stream
        // died before the frame landed — treat it like any stream failure.
        let msg = ClientMsg::Subscribe {
            id: reg.id.clone(),
            hash: reg.hash.borrow().clone(),
        };
        let Ok(json) = serde_json::to_string(&msg) else {
            return;
        };
        let chunk = js_sys::Uint8Array::from(format!("{json}\n").as_bytes());
        if let Err(e) = JsFuture::from(writer.write_with_chunk(&chunk)).await {
            return break_session(&inner, e).await;
        }

        // Bytes, not `string`s: a frame's UTF-8 may split across chunks, and
        // a lossy per-chunk decode would corrupt it.
        let mut buf = Vec::<u8>::new();
        loop {
            let read_res = match JsFuture::from(reader.read()).await {
                Ok(r) => r,
                Err(e) => return break_session(&inner, e).await,
            };
            let done = js_sys::Reflect::get(&read_res, &JsValue::from("done"))
                .map(|v| v.as_bool().unwrap_or(true))
                .unwrap_or(true);
            // Server closed the stream: the subscription ended (gear evicted
            // or errored). Drop the registration — unless the session died
            // too (replay incoming), reported by the transport's absence; the
            // generation match rules out a stale task from a dead session
            // waking after the next one is up.
            if done {
                leptos::logging::warn!("wt sub closed");
                reg.writer.borrow_mut().take();
                if inner.transport.borrow().is_some() && inner.session_gen.get() == epoch {
                    inner.subs.borrow_mut().remove(&sub);
                }
                return;
            }
            let Ok(value) = js_sys::Reflect::get(&read_res, &JsValue::from("value")) else {
                continue;
            };
            buf.extend(value.unchecked_into::<js_sys::Uint8Array>().to_vec());
            while let Some(i) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=i).collect();
                if line.len() <= 1 {
                    continue;
                }
                match serde_json::from_slice::<ServerMsg>(&line) {
                    Ok(ServerMsg::Push { out, hash }) => {
                        // The registration may have been canceled meanwhile;
                        // check before invoking, so a canceled subscription's
                        // in-flight pushes go nowhere.
                        let live = inner.subs.borrow().get(&sub).is_some();
                        if live {
                            *reg.hash.borrow_mut() = Some(hash);
                            (reg.cb)(out);
                        }
                    }
                    Err(e) => leptos::logging::warn!("decode: {e}"),
                }
            }
        }
    });
}

/// `setTimeout` as a future.
async fn sleep(ms: i32) {
    let mut schedule = move |resolve: js_sys::Function, _reject: js_sys::Function| {
        let _ = web_sys::window()
            .expect("no window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    };
    let promise = js_sys::Promise::new(&mut schedule);
    let _ = JsFuture::from(promise).await;
}
