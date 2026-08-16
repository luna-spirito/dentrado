//! The kolorinko server: HTTP/3 + WebTransport over a single QUIC listener.
//!
//! One QUIC endpoint (ALPN `h3`) serves two kinds of traffic on the same port:
//! - **HTTP/3 requests** (`GET /`, `GET /<asset>`) → static assets (the built
//!   frontend), served from an in-memory map loaded at startup.
//! - **WebTransport** (`CONNECT :protocol = webtransport`) → a [`run_session`]
//!   loop that round-trips the dentrado gear protocol (NDJSON over one bidi
//!   stream), with server push from per-page subscriptions.
//!
//! # Every core binds the same QUIC port (SO_REUSEPORT)
//! compio-quic's `Endpoint::server`/`ServerBuilder::bind` call `UdpSocket::bind`
//! directly, which never sets `SO_REUSEPORT`. [`build_endpoint`] instead builds
//! the UDP socket via `socket2` and sets `SO_REUSEPORT` before binding, so every
//! core owns a listener on the same port. The kernel spreads incoming QUIC
//! datagrams across cores by client 4-tuple, so each connection lives entirely
//! on the core whose socket received its Initial — mirroring how the TCP
//! bootstrap already reuses the port. Gear work for a session still routes to
//! the owning core by [`Core::db_run_gear`]'s inter-core routing.
//!
//! The cost is connection migration: a client that changes its source address
//! (NAT rebinding, Wi-Fi → cellular) is re-hashed to a different core, where
//! its connection state doesn't exist, so the connection drops.
//! eBPF CID steering desired in long term.
//!
//! # TLS — two modes, switched by `--inject-wt-hash`
//! - **Pooling mode** (flag unset, the default): the QUIC endpoint presents the
//!   **CA-trusted** cert (mkcert) — the same one the TCP bootstrap uses — so
//!   the browser's HTTP/3 upgrade (`Alt-Svc`) validates and WebTransport pools
//!   with the fetch connection pool (`allowPooling` on the client). No cert
//!   hash is involved.
//! - **Hash-pinning mode** (`--inject-wt-hash`): the endpoint presents a
//!   **short-lived self-signed** cert, pinned by SHA-256 in the browser via
//!   `serverCertificateHashes`. Browsers won't honor a local CA for a
//!   hash-pinned WT handshake, so this needs no CA setup; the cert must be
//!   short-lived (≤ ~14 days), so it's regenerated on each start, and its hash
//!   is injected into the page once at startup (see [`crate::assets`]).
//!
//! [`Core::db_run_gear`]: dentrado::core::core_ctx::Core::db_run_gear

use std::{collections::HashMap, io, rc::Rc, sync::Arc};

use bytes::Bytes;
use compio::runtime;
use futures::{
    channel::mpsc,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
    stream::StreamExt,
};
use log::{error, info, warn};

use dentrado::core::{
    core_ctx::{Core, Subscription},
    gear::GearResult,
    storage::InMemoryStorage,
};
use kolorinko_rt::{
    Body,
    wire::{self, ClientMsg, ServerMsg},
};

use crate::respond;
use crate::runtime::{
    GearOutShared, KolorinkoRT, article_latest, article_latest_parsed, asset,
    repo_l_article_latest, repo_resource, shell,
};
use crate::wikidot_page::RepoMeta;

/// Max concurrent WebTransport sessions advertised per HTTP/3 connection
/// (`SETTINGS_H3_WEBTRANSPORT_MAX_SESSIONS`). Default-0 means "none", which
/// makes browsers refuse the WT handshake.
const MAX_WT_SESSIONS: u64 = 16;

/// Bind `bind` with TLS and run the QUIC accept loop. Runs forever; only
/// returns on a fatal endpoint error.
pub(crate) async fn serve(
    core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    bind: &str,
    assets: Arc<HashMap<String, Body>>,
    repo_meta: RepoMeta,
    inject_wt_hash: bool,
) -> io::Result<()> {
    let endpoint = build_endpoint(bind, inject_wt_hash)?;
    info!(
        "kolorinko H3 server listening on {bind} ({})",
        if inject_wt_hash {
            "hash-pinning"
        } else {
            "pooling"
        }
    );

    loop {
        match endpoint.wait_incoming().await {
            Some(incoming) => {
                let core = core.clone();
                let assets = assets.clone();
                let repo_meta = repo_meta.clone();
                runtime::spawn(async move {
                    if let Err(e) = handle_conn(incoming, &core, &assets, &repo_meta).await {
                        warn!("conn closed: {e}");
                    }
                })
                .detach();
            }
            None => {
                error!("endpoint on {bind} stopped accepting");
                return Ok(());
            }
        }
    }
}

/// Build the QUIC `Endpoint` with a TLS cert chosen by the mode:
/// - hash-pinning mode (`inject_wt_hash`) → the short-lived self-signed cert
///   the browser pins via `serverCertificateHashes`;
/// - pooling mode → the CA-trusted (mkcert) cert, so the HTTP/3 upgrade
///   advertised by [`crate::web`] validates and WebTransport can pool with the
///   fetch connection pool.
fn build_endpoint(bind: &str, inject_wt_hash: bool) -> io::Result<compio_quic::Endpoint> {
    let (certs, key) = if inject_wt_hash {
        crate::tls::wt_cert()
    } else {
        crate::tls::load_cert_key()?
    };
    let server_config = compio_quic::ServerBuilder::new_with_single_cert(certs, key)
        .map_err(|e| io::Error::other(format!("server cert: {e}")))?
        .with_alpn_protocols(&["h3"])
        .build();
    // Build the UDP socket ourselves so we can set SO_REUSEPORT (compio-quic's
    // `bind` doesn't). With it every core binds the same port; the kernel hashes
    // incoming QUIC datagrams by 4-tuple to one core, so each connection lives
    // entirely on the endpoint that received its Initial.
    let socket = reuseport_udp_socket(bind)?;
    compio_quic::Endpoint::new(
        socket,
        compio_quic::EndpointConfig::default(),
        Some(server_config),
        None,
    )
}

/// Create a UDP socket bound to `bind` with `SO_REUSEADDR` + `SO_REUSEPORT`, so
/// every core can share the QUIC listener port and the kernel spreads incoming
/// datagrams across them by client 4-tuple. Built via `socket2` (the only way
/// to set `SO_REUSEPORT` before `bind`), then handed to compio through the safe
/// `UdpSocket::from_std`, which attaches it to the current thread's runtime.
/// There is no connection-migration support: a client that changes its source
/// address is re-hashed to another core where its state is absent — fine for
/// localhost/LAN, where no NAT sits between client and server.
fn reuseport_udp_socket(bind: &str) -> io::Result<compio::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|e| io::Error::other(format!("bind addr: {e}")))?;
    let sock = Socket::new(Domain::for_address(addr), Type::DGRAM, Some(Protocol::UDP))?;
    // SO_REUSEPORT must be set before `bind` to join the per-port socket group.
    sock.set_reuse_address(true)?;
    sock.set_reuse_port(true)?;
    sock.bind(&addr.into())?;
    compio::net::UdpSocket::from_std(sock.into())
}

/// Handle one QUIC connection: serve HTTP/3 requests (static assets) and, if a
/// WebTransport CONNECT arrives, hand the connection to a session.
async fn handle_conn(
    incoming: compio_quic::Incoming,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    assets: &Arc<HashMap<String, Body>>,
    repo_meta: &RepoMeta,
) -> io::Result<()> {
    let conn = incoming
        .await
        .map_err(|e| io::Error::other(format!("quic handshake: {e}")))?;
    let remote = conn.remote_address();

    let mut h3_conn = h3::server::builder()
        // WebTransport needs all four: `enable_webtransport` advertises
        // SETTINGS_H3_ENABLE_WEBTRANSPORT, `max_webtransport_sessions`
        // advertises SETTINGS_H3_WEBTRANSPORT_MAX_SESSIONS (default 0 = "no WT
        // sessions" — browsers refuse the handshake), and the other two carry
        // the rest of the WT-over-H3 requirements.
        .enable_webtransport(true)
        .enable_extended_connect(true)
        .enable_datagram(true)
        .max_webtransport_sessions(MAX_WT_SESSIONS)
        .build::<_, Bytes>(conn)
        .await
        .map_err(|e| io::Error::other(format!("h3 build: {e}")))?;

    loop {
        let resolver = match h3_conn.accept().await {
            Ok(Some(r)) => r,
            Ok(None) => return Ok(()), // connection closed
            Err(e) => {
                warn!("h3 accept {remote}: {e}");
                return Ok(());
            }
        };
        let (req, stream) = match resolver.resolve_request().await {
            Ok(v) => v,
            Err(e) => {
                warn!("h3 resolve {remote}: {e}");
                continue;
            }
        };
        let is_wt = req.method() == http::Method::CONNECT
            && req
                .extensions()
                .get::<h3::ext::Protocol>()
                .is_some_and(|p| p == &h3::ext::Protocol::WEB_TRANSPORT);
        if is_wt {
            // WebTransport: the session takes ownership of the H3 connection,
            // so this connection accepts no further requests.
            let session =
                h3_webtransport::server::WebTransportSession::accept(req, stream, h3_conn)
                    .await
                    .map_err(|e| io::Error::other(format!("wt accept: {e}")))?;
            info!("wt session established from {remote}");
            let core = core.clone();
            let repo_meta = repo_meta.clone();
            runtime::spawn(async move {
                run_session(session, &core, &repo_meta).await;
            })
            .detach();
            return Ok(()); // h3_conn consumed by the session
        }
        // HTTP/3 request — resolved through [`crate::respond`] (static assets,
        // `/repo/` assets, SSR'd pages) on this stream; the connection stays
        // open for further multiplexed requests.
        let assets = assets.clone();
        let core = core.clone();
        let repo_meta = repo_meta.clone();
        runtime::spawn(async move {
            let mut stream = stream;
            let full = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| req.uri().path().to_string());
            let accept_zstd = req
                .headers()
                .get(http::header::ACCEPT_ENCODING)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.contains("zstd"));
            let reply = respond::resolve(&full, accept_zstd, &assets, repo_meta, &core).await;
            let mut b = http::Response::builder()
                .status(reply.status)
                .header("content-type", reply.mime)
                .header("content-length", reply.served.bytes.len().to_string());
            if let Some(enc) = reply.served.encoding {
                b = b.header("content-encoding", enc);
            }
            let resp = b.body(()).unwrap();
            if let Err(e) = stream.send_response(resp).await {
                warn!("h3 send_response: {e}");
                return;
            }
            if let Err(e) = stream.send_data(reply.served.bytes).await {
                warn!("h3 send_data: {e}");
                return;
            }
            // Drain the request body to its FIN so the bidi stream's receive
            // half closes cleanly. compio-quic's `RecvStream::drop` otherwise
            // sends `STOP_SENDING(0)` (because `all_data_read` is false after
            // reading only the request HEADERS) — clients see that as a stream
            // reset right after the complete response, which is enough for
            // browsers to mark HTTP/3 broken and refuse the Alt-Svc upgrade
            // (page stuck on HTTP/1.1).
            while stream.recv_data().await.is_ok_and(|d| d.is_some()) {}
            // Finalize the response: send the QUIC FIN on the send half so the
            // client sees a clean end-of-stream.
            if let Err(e) = stream.finish().await {
                warn!("h3 finish: {e}");
            }
        })
        .detach();
    }
}

/// Drive one WebTransport session: wait for the client's bidi stream, then
/// duplex request/reply + push.
async fn run_session(
    session: h3_webtransport::server::WebTransportSession<compio_quic::Connection, Bytes>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    repo_meta: &RepoMeta,
) {
    let bidi = loop {
        match session.accept_bi().await {
            Ok(Some(h3_webtransport::server::AcceptedBi::BidiStream(_id, b))) => break b,
            Ok(Some(h3_webtransport::server::AcceptedBi::Request(..))) => continue,
            Ok(None) => return, // session closed
            Err(e) => {
                warn!("wt accept_bi: {e}");
                return;
            }
        }
    };

    // Split into read/write halves: a reader task pulls requests while the main
    // loop pushes replies. `AsyncReadExt::split`'s BiLock serializes the two,
    // which our request/reply cadence never needs to overlap.
    let (reader, writer) = bidi.split();

    let (tx_cmd, mut rx_cmd) = mpsc::unbounded::<ClientMsg>();
    let (tx_msg, mut rx_msg) = mpsc::unbounded::<ServerMsg>();
    // One push task per active `sub` handle; dropping the `JoinHandle` (on
    // `Cancel` or at disconnect) cancels the task and releases its
    // `Subscription`, so a disconnected client's gears lose interest
    // automatically.
    let mut subs: HashMap<u64, runtime::JoinHandle<()>> = HashMap::new();

    let reader_task = runtime::spawn(async move {
        let mut r = reader;
        let mut line = Vec::<u8>::new();
        loop {
            match read_frame(&mut r, &mut line).await {
                Ok(Some(msg)) => {
                    if tx_cmd.unbounded_send(msg).is_err() {
                        return; // main loop gone
                    }
                }
                Ok(None) => return, // clean half-close
                Err(e) => {
                    warn!("wt read: {e}");
                    return;
                }
            }
        }
    });
    reader_task.detach();

    let mut writer = writer;
    loop {
        let close = select! {
            cmd = rx_cmd.next() => match cmd {
                Some(ClientMsg::Subscribe { sub, id }) => {
                    // Replace any existing subscription under this handle.
                    drop(subs.remove(&sub));
                    let core = core.clone();
                    let repo_meta = repo_meta.clone();
                    let tx = tx_msg.clone();
                    let handle = runtime::spawn(async move {
                        push(sub, id, repo_meta, &core, tx).await;
                    });
                    subs.insert(sub, handle);
                    false
                }
                Some(ClientMsg::Cancel { sub }) => {
                    subs.remove(&sub); // drop → cancel task + release subscription
                    false
                }
                None => true, // reader ended → client gone
            },
            msg = rx_msg.next() => match msg {
                Some(m) => match write_frame(&mut writer, &m).await {
                    Ok(()) => false,
                    Err(e) => { warn!("wt write: {e}"); true }
                },
                None => true, // tx_msg dropped → unreachable (we hold one)
            },
        };
        if close {
            break;
        }
    }
    // `subs` drops here: every push task is cancelled, every `Subscription`
    // released — disconnected clients stop subscribing automatically.
}

// ---- wire dispatch ----------------------------------------------------------
//
// Wire `GearId` (no `repo_meta`) → runtime `GearId` (with the server's
// configured `repo_meta`) → `Subscription`. The runtime builders construct the
// full runtime `GearId` internally; the server only injects `repo_meta` and
// forwards the client-supplied id fields. Runtime `GearOut` → wire `GearOut` is
// a plain variant-by-variant relabel (same payloads, same variant names).

async fn subscribe_wire(
    id: wire::GearId,
    repo_meta: RepoMeta,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Subscription<KolorinkoRT, InMemoryStorage<KolorinkoRT>> {
    match id {
        wire::GearId::ArticleLatest { site, slug } => {
            article_latest(repo_meta, site, slug).subscribe(core).await
        }
        wire::GearId::ArticleLatestParsed { site, slug } => {
            article_latest_parsed(repo_meta, site, slug)
                .subscribe(core)
                .await
        }
        wire::GearId::RepoLArticleLatest { site, slug } => {
            repo_l_article_latest(repo_meta, site, slug)
                .subscribe(core)
                .await
        }
        wire::GearId::Shell(site) => shell(repo_meta, site).subscribe(core).await,
        // Assets are HTTP-only — never shipped over WebTransport. The match is
        // exhaustive on the generated wire enum, but `to_wire_out` drops these.
        wire::GearId::Asset { site, hash, ext } => {
            asset(repo_meta, site, hash, ext).subscribe(core).await
        }
        // Server-internal resolution dependency of `article_latest`; the client
        // never subscribes to it, but the match must be exhaustive.
        wire::GearId::RepoResource { site, path } => {
            repo_resource(repo_meta, site, path).subscribe(core).await
        }
    }
}

// TODO: Annihilate. Also, no copies.
fn to_wire_out(res: GearResult<KolorinkoRT>) -> Option<wire::GearOut> {
    match res {
        // Shippable gears carry their payload directly.
        GearResult::Ship(_) => None,
        // Shared gears are shared *across cores* by reference; to the client
        // they serialize the same way, so clone the payload out of the handle.
        GearResult::Shared(s) => match &*s {
            GearOutShared::ShellOut(a) => Some(wire::GearOut::ShellOut(a.clone())),
            GearOutShared::ArticleLatestOut(a) => Some(wire::GearOut::ArticleLatestOut(a.clone())),
            GearOutShared::ArticleLatestParsedOut(a) => {
                Some(wire::GearOut::ArticleLatestParsedOut(a.clone()))
            }
            GearOutShared::RepoLArticleLatestOut(a) => {
                Some(wire::GearOut::RepoLArticleLatestOut(a.clone()))
            }
            // Assets are served over plain HTTP, never the WebTransport wire —
            // the browser fetches them via `<img>`/`<link>`/`url()`. Dropping
            // here keeps their bytes out of the subscription channel.
            GearOutShared::AssetOut(_) => None,
            // Server-internal: never shipped to the client.
            GearOutShared::RepoResourceOut(_) => None,
        },
        GearResult::Local(_) => None,
    }
}

/// Drive one subscription: ship the current output, then every subsequent
/// update, then a final `Dropped`.
async fn push(
    sub: u64,
    id: wire::GearId,
    repo_meta: RepoMeta,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    tx: mpsc::UnboundedSender<ServerMsg>,
) {
    let s = subscribe_wire(id, repo_meta, core).await;
    if let Some(out) = to_wire_out(s.current()) {
        let _ = tx.unbounded_send(ServerMsg::Update { sub, out });
    }
    while let Some(out) = to_wire_out(s.next().await) {
        let _ = tx.unbounded_send(ServerMsg::Update { sub, out });
    }
    let _ = tx.unbounded_send(ServerMsg::Dropped { sub });
}

// ---- newline-delimited JSON (NDJSON) framing ---------------------------------
//
// One compact JSON object per line. NDJSON (not a binary length prefix) so the
// browser client parses it with `TextDecoder` + `indexOf('\n')`. serde_json
// never emits a literal `\n` inside a compact object, so `\n` is unambiguous.

async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    line: &mut Vec<u8>,
) -> io::Result<Option<ClientMsg>> {
    loop {
        if let Some(nl) = line.iter().position(|&b| b == b'\n') {
            let req_bytes: Vec<u8> = line.drain(..=nl).collect();
            let body = &req_bytes[..req_bytes.len() - 1]; // strip the '\n'
            if !body.is_empty() {
                match serde_json::from_slice::<ClientMsg>(body) {
                    Ok(msg) => return Ok(Some(msg)),
                    // Skip a malformed line (e.g. a path component that failed
                    // validation) without tearing down the whole session.
                    Err(e) => warn!("wt bad frame: {e}"),
                }
            }
            continue; // blank line, or skipped frame
        }
        let mut chunk = [0u8; 4096];
        match r.read(&mut chunk).await {
            Ok(0) => return Ok(None), // clean half-close
            Ok(n) => line.extend_from_slice(&chunk[..n]),
            Err(e) => return Err(e),
        }
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, msg: &ServerMsg) -> io::Result<()> {
    let json =
        serde_json::to_vec(msg).map_err(|e| io::Error::other(format!("encode reply: {e}")))?;
    w.write_all(&json).await?;
    w.write_all(b"\n").await?;
    w.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn reuseport_shared_bind() {
        // The invariant the whole multi-core QUIC design rests on: two sockets
        // bind the same port. Catches a silent regression if set_reuse_port
        // were reordered after bind (it would no-op, second bind → EADDRINUSE).
        compio::runtime::Runtime::new().unwrap().block_on(async {
            let a = super::reuseport_udp_socket("127.0.0.1:0").unwrap();
            let port = a.local_addr().unwrap().port();
            let _b = super::reuseport_udp_socket(&format!("127.0.0.1:{port}")).unwrap();
        });
    }
}
