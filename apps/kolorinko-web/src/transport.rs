//! Transport client for kolorinko: WebTransport with a plain-fetch fallback.
//!
//! WebTransport doesn't work everywhere — corporate middleboxes silently drop
//! QUIC, older browsers lack the API — so the client picks its transport per
//! session, on first success, and never revisits the choice until the page
//! reloads:
//!
//! 1. Probe WebTransport under a [`PROBE_MS`] deadline. A session that
//!    reaches `ready()` — the extended CONNECT answered — proves the
//!    transport works end to end: **lock WebTransport in**.
//! 2. Otherwise, one fetch round-trip on [`kolorinko_rt::LEGACY_PATH`] for
//!    any live subscription. A 2xx answer proves plain HTTP reaches the
//!    gears: **lock fetch in**.
//! 3. Neither worked: sleep with backoff, back to 1.
//!
//! The registry — not the connection — is the source of truth regardless of
//! transport: subscriptions register immediately, sit through the probe, and
//! are replayed at lock-in. Whichever transport won then serves each one:
//! over WebTransport, one bidirectional stream per subscription with
//! server-pushed updates for the session's life; over fetch, one round-trip
//! per subscription — no push channel, and deliberately no polling: fallback
//! content is only as fresh as the navigation that asked for it, and only a
//! page reload re-fetches. A lock-in survives any later failure — WebTransport
//! sessions reconnect forever ([`wt_forever`]), a failed fetch round-trip
//! retries until it lands — because a mid-session blip says nothing about
//! which transport works on *this* network; only the probe decides, and only
//! a reload re-probes.
//!
//! # WebTransport
//! The whole site is one origin — `https://<host>:<port>`. The page loads over
//! HTTP/1.1-over-TLS on the first visit, then over HTTP/3 once the browser
//! caches the `Alt-Svc` hint. This module opens a WebTransport session to that
//! **same origin** (`window.location.origin`) and speaks the subscribe/push
//! envelope from [`kolorinko_rt::wire`]: **one bidirectional stream per
//! subscription** — [`Transport::subscribe`] opens a stream, sends the
//! stream's single `Subscribe { id, hash }` frame, and the stream's read loop
//! feeds typed updates to the callback until either side closes it. Closing
//! the stream *is* the cancel/dropped signal; there are no other control
//! messages.
//!
//! # Content hashes
//! The server hashes every payload (SHA-256 of the wire `GearOut` JSON); the
//! client remembers the last hash per subscription and echoes it in
//! `Subscribe` — on (re)connect *and* on SSR hydration (the hashes ride in
//! [`SsrState`]). A matching hash means the server sends nothing until the
//! content actually changes, so unchanged payloads cross the wire zero times
//! — over WebTransport (no push frame) and fetch (a bare `204`) alike.
//!
//! `allowPooling: true` *hints* the browser to run this session on an existing
//! HTTP/3 connection to the origin instead of opening a fresh QUIC connection.
//! It's only a hint — browsers may still use a dedicated connection.
//!
//! # Browser caveat
//! WebTransport rejects self-signed certificates outright (no interactive
//! acceptance like `wss://`). Local testing needs a *trusted* cert (e.g
//! `mkcert localhost`) on the server, *or* the bootstrap-injected cert hash.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use kolorinko_rt::LEGACY_PATH;
use kolorinko_rt::wire::{ClientMsg, GearId, GearOut, GearQuery, ServerMsg};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::{JsFuture, spawn_local};
use web_sys::{
    ReadableStreamDefaultReader, Request, RequestInit, Response, WebTransport,
    WebTransportBidirectionalStream, WebTransportOptions, WritableStreamDefaultWriter,
};
// `WebTransportCloseInfo` feature is enabled for `WebTransport::closed`.

/// First retry delay after a failed probe/round; doubles per attempt, capped.
const BACKOFF_MIN_MS: i32 = 250;
const BACKOFF_MAX_MS: i32 = 5_000;
/// A WebTransport session that stayed up at least this long resets the
/// reconnect backoff.
const BACKOFF_RESET_MS: f64 = 10_000.0;
/// The WebTransport probe deadline. A middlebox that silently drops QUIC (the
/// classic "no WebTransport here" failure) makes `ready()` hang far longer
/// than this — the probe gives up and fetch takes over instead of stalling
/// the page for the browser's own QUIC timeouts.
const PROBE_MS: i32 = 3_000;

/// The session's transport, picked once by the probe and locked for the
/// session's lifetime (see the module docs). `Probing` holds every
/// subscription in the registry untouched — the winner replays them.
#[derive(Clone, Copy)]
enum Mode {
    Probing,
    Wt,
    Fetch,
}

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
/// while its WebTransport stream is up) doubles as the cancel handle —
/// closing it half-closes the stream, which the server reads as a cancel.
struct Registration {
    id: GearId,
    cb: UpdateCb,
    hash: RefCell<Option<String>>,
    writer: RefCell<Option<Rc<WritableStreamDefaultWriter>>>,
}

/// Client state shared by [`Transport`], the supervisor, and per-subscription
/// tasks: the locked transport mode, the live WebTransport session (absent
/// between sessions — also the liveness flag for stream teardown decisions),
/// the session generation counter (so a stale stream task from a dead session
/// can't mistake the next session for its own), the sub-handle counter, and
/// the subscription registry. Because the registry — not the connection — is
/// the source of truth, subscriptions survive probes and reconnects alike.
struct Inner {
    mode: Cell<Mode>,
    transport: RefCell<Option<WebTransport>>,
    session_gen: Cell<u64>,
    next_sub: Cell<u64>,
    subs: RefCell<HashMap<u64, Rc<Registration>>>,
}

/// The app's single transport client (WebTransport, falling back to fetch —
/// see the module docs). One instance per page (see [`connect`]); share it
/// across components via a leptos signal/context. Each subscription is driven
/// by whichever transport the probe locked in.
pub struct Transport {
    inner: Rc<Inner>,
}

impl Transport {
    /// Subscribe to a gear query: register `on_update` under a fresh handle
    /// and hand it to the locked transport (or leave it for the probe's
    /// replay). `known` is the hash of the content the caller already holds
    /// (SSR hydration), if any; the server then skips re-sending it. Returns
    /// the handle for [`cancel`](Self::cancel). Safe while probing or
    /// disconnected: the registration is replayed once a transport is live.
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
        spawn_sub(self.inner.clone(), sub, reg);
        sub
    }

    /// Stop a subscription: drop the registration (so it isn't replayed and
    /// its fetch round-trip, if any, stops retrying) and half-close its
    /// WebTransport stream — the server reads that as a cancel and releases
    /// the gear subscription.
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

/// The app's single transport client. Subscriptions register locally right
/// away; the supervisor probes WebTransport and fetch (see the module docs)
/// and the winner drives the registry from then on. Transport problems are
/// retried with backoff and logged to the browser console.
pub(crate) fn connect() -> Rc<Transport> {
    let inner = Rc::new(Inner {
        mode: Cell::new(Mode::Probing),
        transport: RefCell::new(None),
        session_gen: Cell::new(0),
        next_sub: Cell::new(1),
        subs: RefCell::new(HashMap::new()),
    });
    spawn_local(supervise(inner.clone()));
    Rc::new(Transport { inner })
}

/// Hand one registration to the locked transport: WebTransport opens its
/// stream (if a session is up; else the session replay handles it), fetch
/// fires its one round-trip. While probing, the registration just sits in
/// the registry — the lock-in replays it.
fn spawn_sub(inner: Rc<Inner>, sub: u64, reg: Rc<Registration>) {
    match inner.mode.get() {
        Mode::Wt => spawn_wt_stream(inner, sub, reg),
        Mode::Fetch => spawn_local(fetch_oneshot(inner, sub, reg)),
        Mode::Probing => {}
    }
}

/// Pick the session's transport, then serve it forever:
/// WebTransport → fetch → around again (see the module docs). The lock-in is
/// one-way: whichever transport answers first owns the session until the
/// page reloads, and every later failure is its own reconnect/retry logic.
async fn supervise(inner: Rc<Inner>) {
    let mut backoff = BACKOFF_MIN_MS;
    loop {
        if let Some(wt) = probe_wt().await {
            inner.mode.set(Mode::Wt);
            wt_forever(inner, wt).await;
            return;
        }
        if let Some(done) = fetch_probe(&inner).await {
            inner.mode.set(Mode::Fetch);
            replay_fetch(&inner, done);
            return;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_MS);
    }
}

// ── WebTransport ─────────────────────────────────────────────────────────────

/// Construct a WebTransport toward the page origin (cert-hash options and
/// all — see [`construct_wt`]'s caller docs in the module header). `None`
/// when it can't even be attempted: no window, no WebTransport, unparsable
/// origin. Constructor failure (unsupported transport) is permanent — for
/// the probe that simply means "fetch, then".
fn construct_wt() -> Option<WebTransport> {
    let window = web_sys::window()?;
    let url = window.location().origin().ok()?;
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
    WebTransport::new_with_options(&url, &options).ok()
}

/// One WebTransport attempt under the [`PROBE_MS`] deadline: a session that
/// reaches `ready()` — proof the extended CONNECT went through whatever sits
/// between — wins immediately; anything else (construct failure, `ready()`
/// rejection, or the timer — the classic middlebox that silently drops QUIC)
/// tears the attempt down and answers `None`.
async fn probe_wt() -> Option<WebTransport> {
    let wt = construct_wt()?;
    let window = web_sys::window()?;
    // A timer promise resolving `true` — the value is what tells the two
    // racers apart (`ready()` resolves `undefined`).
    let timer = js_sys::Promise::new(&mut |resolve, _| {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_1(
            &resolve,
            PROBE_MS,
            &JsValue::from_bool(true),
        );
    });
    let ready: JsValue = wt.ready().into();
    let won = js_sys::Promise::race(&js_sys::Array::of2(&ready, &(timer.into())));
    match JsFuture::from(won).await {
        // `ready()` settled first: the session is live — hand it over.
        Ok(v) if v.as_bool() != Some(true) => Some(wt),
        Ok(_) => {
            wt.close();
            None
        }
        Err(e) => {
            leptos::logging::warn!("wt connect: {e:?}");
            None
        }
    }
}

/// Open a session with no deadline — after lock-in the supervisor owns the
/// patience: a failed attempt just feeds the reconnect backoff. `None` when
/// the construct or handshake failed.
async fn open_wt() -> Option<WebTransport> {
    let wt = construct_wt()?;
    if let Err(e) = JsFuture::from(wt.ready()).await {
        leptos::logging::warn!("wt connect: {e:?}");
        return None;
    }
    Some(wt)
}

/// A fresh session goes live: generation bump (stale stream tasks from a dead
/// session can't mistake it for theirs), install the transport, replay every
/// registration on its own fresh stream — each echoing its last-held hash, so
/// unchanged content isn't re-sent.
fn install_session(inner: &Rc<Inner>, wt: &WebTransport) {
    inner
        .session_gen
        .set(inner.session_gen.get().wrapping_add(1));
    *inner.transport.borrow_mut() = Some(wt.clone());
    for (&sub, reg) in inner.subs.borrow().iter() {
        spawn_wt_stream(inner.clone(), sub, reg.clone());
    }
}

/// Serve WebTransport sessions forever (the post-lock-in half of the old
/// supervisor): replay the registry on each fresh session, rebuild on death
/// with backoff that resets once a session stayed up a while. `first` is the
/// probe's already-established session — the loop's own reconnects open
/// fresh ones without a deadline.
async fn wt_forever(inner: Rc<Inner>, first: WebTransport) {
    let mut backoff = BACKOFF_MIN_MS;
    let mut next = Some(first);
    loop {
        let opened_at = js_sys::Date::now();
        let wt = match next.take() {
            Some(wt) => wt,
            None => match open_wt().await {
                Some(wt) => wt,
                None => {
                    sleep(backoff).await;
                    backoff = (backoff * 2).min(BACKOFF_MAX_MS);
                    continue;
                }
            },
        };
        install_session(&inner, &wt);
        // Settles (resolve or reject) when the session ends; nobody else needs
        // the close info, and this future observes the rejection.
        let _ = JsFuture::from(wt.closed()).await;
        leptos::logging::warn!("wt session closed");
        *inner.transport.borrow_mut() = None;
        if js_sys::Date::now() - opened_at >= BACKOFF_RESET_MS {
            backoff = BACKOFF_MIN_MS;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_MS);
    }
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
/// no session is up (the session replay will handle it). Any stream failure
/// escalates to a session rebuild — the supervisor then replays every
/// registration, this one included.
fn spawn_wt_stream(inner: Rc<Inner>, sub: u64, reg: Rc<Registration>) {
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
            // Server closed the stream. The normal case is the cancel ack:
            // our `cancel` removed the registration before half-closing, so
            // an absent registration is silence. A registration still there
            // is a server-initiated end (gear evicted or errored) — the one
            // worth logging — and is dropped: never resubscribed until the
            // route changes. Unless the session died too (replay incoming),
            // reported by the transport's absence; the generation match
            // rules out a stale task from a dead session waking after the
            // next one is up.
            if done {
                reg.writer.borrow_mut().take();
                if inner.transport.borrow().is_some()
                    && inner.session_gen.get() == epoch
                    && inner.subs.borrow_mut().remove(&sub).is_some()
                {
                    leptos::logging::warn!("wt sub closed");
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

// ── fetch fallback ───────────────────────────────────────────────────────────

/// One `POST /-/legacy` round-trip's outcome: a pushed output (with its
/// content hash), "nothing new" (204 — echoed hash current, or a gear that
/// ships nothing over the wire), or a failure (network error, non-2xx,
/// undecodable answer).
enum FetchRes {
    Push { out: GearOut, hash: String },
    Unchanged,
    Fail,
}

/// One `POST /-/legacy` for `reg`'s query, echoing the hash it last held.
async fn fetch_once(reg: &Registration) -> FetchRes {
    let msg = ClientMsg::Subscribe {
        id: reg.id.clone(),
        hash: reg.hash.borrow().clone(),
    };
    let Ok(body) = serde_json::to_string(&msg) else {
        return FetchRes::Fail;
    };
    let init = RequestInit::new();
    init.set_method("POST");
    init.set_body(&JsValue::from_str(&body));
    let Ok(req) = Request::new_with_str_and_init(LEGACY_PATH, &init) else {
        return FetchRes::Fail;
    };
    let Some(window) = web_sys::window() else {
        return FetchRes::Fail;
    };
    let resp = match JsFuture::from(window.fetch_with_request(&req)).await {
        Ok(r) => r,
        Err(_) => return FetchRes::Fail,
    };
    let Ok(resp) = resp.dyn_into::<Response>() else {
        return FetchRes::Fail;
    };
    match resp.status() {
        200 => {
            let text = match resp.text() {
                Ok(p) => p,
                Err(_) => return FetchRes::Fail,
            };
            let text = match JsFuture::from(text).await {
                Ok(t) => t,
                Err(_) => return FetchRes::Fail,
            };
            let Some(text) = text.as_string() else {
                return FetchRes::Fail;
            };
            match serde_json::from_str::<ServerMsg>(&text) {
                Ok(ServerMsg::Push { out, hash }) => FetchRes::Push { out, hash },
                _ => FetchRes::Fail,
            }
        }
        204 => FetchRes::Unchanged,
        _ => FetchRes::Fail,
    }
}

/// Apply one fetch result to its registration: a push fires the callback
/// (hash advanced) if the subscription is still live — a canceled
/// subscription's in-flight answer goes nowhere. `true` when the round trip
/// itself succeeded (push or unchanged), `false` when it failed.
fn deliver(inner: &Inner, sub: u64, reg: &Registration, res: FetchRes) -> bool {
    match res {
        FetchRes::Push { out, hash } => {
            if inner.subs.borrow().contains_key(&sub) {
                *reg.hash.borrow_mut() = Some(hash);
                (reg.cb)(out);
            }
            true
        }
        FetchRes::Unchanged => true,
        FetchRes::Fail => false,
    }
}

/// One subscription's fetch transport: POST once, deliver, done — the
/// fetch fallback is one-shot by design (no push channel, no polling; see
/// the module docs). A failed round-trip retries with backoff while the
/// subscription is live, so a server hiccup can't strand a spinner; the
/// first success ends the task.
async fn fetch_oneshot(inner: Rc<Inner>, sub: u64, reg: Rc<Registration>) {
    let mut backoff = BACKOFF_MIN_MS;
    while inner.subs.borrow().contains_key(&sub) {
        if deliver(&inner, sub, &reg, fetch_once(&reg).await) {
            return;
        }
        sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX_MS);
    }
}

/// The fetch probe: one real round-trip for the first live registration —
/// success means plain HTTP reaches the gears, the answer's content lands
/// in a real subscriber on the way, and the served sub's handle comes back
/// so the lock-in replay doesn't round-trip it twice. No registrations yet:
/// nothing to ask, answer `None` (the probe loop will be back).
async fn fetch_probe(inner: &Rc<Inner>) -> Option<u64> {
    let (sub, reg) = inner
        .subs
        .borrow()
        .iter()
        .next()
        .map(|(s, r)| (*s, r.clone()))?;
    deliver(inner, sub, &reg, fetch_once(&reg).await).then_some(sub)
}

/// Fetch lock-in: every registration gets its one round-trip — except the
/// probe's (`done`), already served by the probe itself.
fn replay_fetch(inner: &Rc<Inner>, done: u64) {
    for (&sub, reg) in inner.subs.borrow().iter() {
        if sub != done {
            spawn_local(fetch_oneshot(inner.clone(), sub, reg.clone()));
        }
    }
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
