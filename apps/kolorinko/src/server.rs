//! The kolorinko server: HTTP/3 + WebTransport over a single QUIC listener.
//!
//! One QUIC endpoint (ALPN `h3`) serves two kinds of traffic on the same port:
//! - **HTTP/3 requests** (`GET /`, `GET /<asset>`) → static assets (the built
//!   frontend), served from an in-memory map loaded at startup.
//! - **WebTransport** (`CONNECT :protocol = webtransport`) → a [`run_session`]
//!   loop accepting one bidirectional stream per subscription (NDJSON frames
//!   with content-hash skip; see [`subscription_stream`]).
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
//! its connection state doesn't exist, so the connection drops. To make that
//! drop fast instead of silent, every endpoint shares one stateless reset key
//! (see [`stateless_reset_key`]): the foreign core answers the unknown DCID
//! with a valid Stateless Reset, and the client tears down promptly rather
//! than waiting out its retransmission budget. eBPF CID steering desired in
//! long term.
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

use std::sync::{Arc, OnceLock};
use std::{collections::HashMap, io, rc::Rc};

use bytes::{Buf, Bytes};
use compio::runtime;
use futures::{
    future::FutureExt,
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    select,
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
    GearOutShared, KolorinkoRT, article_latest, article_latest_parsed, asset, code_block,
    repo_l_article_latest, repo_l_list_pages, repo_l_local_id, repo_l_query_pages, repo_resource,
    shell,
};

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
                runtime::spawn(async move {
                    if let Err(e) = handle_conn(incoming, &core, &assets).await {
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
        compio_quic::EndpointConfig::new(stateless_reset_key()),
        Some(server_config),
        None,
    )
}

/// The process-wide QUIC stateless reset key, shared by every core's
/// endpoint (see the module docs): a datagram whose DCID matches no connection
/// — arriving at the wrong core after a 4-tuple re-hash, or at any core after
/// this process restarted — is answered with a Stateless Reset (RFC 9000
/// §10.3) the client can validate, instead of being silently dropped. Random
/// per process, not persisted: tokens of the previous process's connections
/// die with its key, and clients reconnect through their supervisor.
fn stateless_reset_key() -> Arc<dyn compio_quic::crypto::HmacKey> {
    static KEY: OnceLock<Arc<dyn compio_quic::crypto::HmacKey>> = OnceLock::new();
    KEY.get_or_init(|| {
        use ring::rand::SecureRandom;
        let mut bytes = [0u8; 64];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .expect("system randomness");
        Arc::new(ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &bytes))
    })
    .clone()
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
            runtime::spawn(async move {
                run_session(session, &core).await;
            })
            .detach();
            return Ok(()); // h3_conn consumed by the session
        }
        // HTTP/3 request — resolved through [`crate::respond`] (static
        // assets, `/repo/` assets, SSR'd pages) on this stream; the
        // connection stays open for further multiplexed requests.
        let assets = assets.clone();
        let core = core.clone();
        runtime::spawn(async move {
            let mut stream = stream;
            let full = req
                .uri()
                .path_and_query()
                .map(|p| p.as_str().to_string())
                .unwrap_or_else(|| req.uri().path().to_string());
            // HTTP/3 carries the origin as `:authority` (h3 folds any `Host`
            // header into the URI's authority); the SSR document absolutizes
            // its OpenGraph URLs with it.
            let host = req.uri().authority().map(|a| a.as_str().to_owned());
            // The one POST the server speaks: the fetch-fallback gear
            // endpoint. Its body is read to the end *before* answering;
            // GET/HEAD have none — their receive half is drained after the
            // response instead.
            let post = req.method() == http::Method::POST;
            let reply = if post {
                let path = full.split('?').next().unwrap_or("");
                if path == kolorinko_rt::LEGACY_PATH {
                    match read_h3_body(&mut stream).await {
                        Some(body) => respond::legacy(&body, &core).await,
                        None => respond::Reply::bad_request(),
                    }
                } else {
                    respond::Reply::method_not_allowed()
                }
            } else {
                let accept_zstd = req
                    .headers()
                    .get(http::header::ACCEPT_ENCODING)
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v.contains("zstd"));
                let inm = req
                    .headers()
                    .get(http::header::IF_NONE_MATCH)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned);
                respond::resolve(&full, accept_zstd, &assets, &core, host.as_deref())
                    .await
                    .revalidated(inm.as_deref())
            };
            let mut b = http::Response::builder()
                .status(reply.status)
                .header("content-type", reply.mime)
                .header("cache-control", reply.cache_control)
                .header("vary", "Accept-Encoding");
            // 204 carries no body, so no `Content-Length` either (RFC 9110).
            if reply.status != 204 {
                b = b.header("content-length", reply.served.bytes.len().to_string());
            }
            if let Some(etag) = &reply.etag {
                b = b.header("etag", etag);
            }
            if let Some(loc) = &reply.location {
                b = b.header("location", loc);
            }
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
            if post {
                // The body was fully read before answering: just finalize
                // with FIN.
                if let Err(e) = stream.finish().await {
                    warn!("h3 finish: {e}");
                }
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

/// Read a POST body to its end, bounded by [`respond::LEGACY_MAX`]; `None`
/// on an oversize or errored body.
async fn read_h3_body<S>(stream: &mut h3::server::RequestStream<S, Bytes>) -> Option<Vec<u8>>
where
    S: h3::quic::RecvStream,
{
    let mut body = Vec::new();
    loop {
        match stream.recv_data().await {
            Ok(Some(chunk)) => {
                body.extend_from_slice(chunk.chunk());
                if body.len() > respond::LEGACY_MAX {
                    return None;
                }
            }
            Ok(None) => return Some(body), // fully read
            Err(_) => return None,
        }
    }
}

/// Drive one WebTransport session: accept bidirectional streams until the
/// session closes. Each stream is one subscription ([`subscription_stream`]).
async fn run_session(
    session: h3_webtransport::server::WebTransportSession<compio_quic::Connection, Bytes>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) {
    while let Ok(Some(accepted)) = session.accept_bi().await {
        match accepted {
            h3_webtransport::server::AcceptedBi::BidiStream(_id, stream) => {
                runtime::spawn(subscription_stream(stream, core.clone())).detach();
            }
            // Plain HTTP/3 requests can still arrive through an established
            // session; we serve none after the connect.
            h3_webtransport::server::AcceptedBi::Request(..) => {}
        }
    }
}

/// Drive one subscription stream: read the single `Subscribe`, then push the
/// gear's output whenever its hash differs from what the client last held,
/// until the subscription ends or the stream closes in either direction —
/// a client half-close is a cancel, this side closing (task end) is a
/// `Dropped`. One detached task per stream.
async fn subscription_stream<S>(
    bidi: h3_webtransport::stream::BidiStream<S, Bytes>,
    core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) where
    h3_webtransport::stream::BidiStream<S, Bytes>: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = bidi.split();
    let mut line = Vec::new();
    let ClientMsg::Subscribe { id, hash } = match read_frame(&mut reader, &mut line).await {
        Ok(Some(msg)) => msg,
        _ => return,
    };
    let s = subscribe_wire(id, &core).await;
    let mut last = hash;
    if let Some(out) = to_wire_out(s.current())
        && push_if_changed(&mut writer, &out, &mut last).await.is_err()
    {
        return;
    }
    loop {
        select! {
            // The client never writes another frame; only the half-close
            // (cancel) or a stream error matters.
            frame = read_frame(&mut reader, &mut line).fuse() => match frame {
                Ok(Some(_)) => continue,
                _ => return,
            },
            res = s.next().fuse() => match to_wire_out(res) {
                Some(out) => {
                    if push_if_changed(&mut writer, &out, &mut last).await.is_err() {
                        return;
                    }
                }
                None => return, // subscription ended → closing = Dropped
            },
        }
    }
}

// ---- wire dispatch ----------------------------------------------------------
//
// Wire `GearId` → runtime `Subscription`. Since the globals refactor the wire
// and runtime ids are the same shape (no injected server-config fields): the
// builder forwards the client-supplied id fields as-is. Runtime `GearOut` →
// wire `GearOut` is a plain variant-by-variant relabel (same payloads, same
// variant names).

pub(crate) async fn subscribe_wire(
    id: wire::GearId,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
) -> Subscription<KolorinkoRT, InMemoryStorage<KolorinkoRT>> {
    match id {
        // The client-facing gears are keyed by the canonical URL identity
        // (`space`/`local`): what a URL names is what a subscription names.
        wire::GearId::ArticleLatest { space, local } => {
            article_latest(space, local).subscribe_raw(core).await
        }
        wire::GearId::Shell(space) => shell(space).subscribe_raw(core).await,
        // The canonical resolution cone (server-internal; exhaustive match).
        wire::GearId::ArticleLatestParsed { space, local } => {
            article_latest_parsed(space, local)
                .subscribe_raw(core)
                .await
        }
        wire::GearId::RepoLArticleLatest { space, local } => {
            repo_l_article_latest(space, local)
                .subscribe_raw(core)
                .await
        }
        wire::GearId::RepoLLocalId { site, slug } => {
            repo_l_local_id(site, slug).subscribe_raw(core).await
        }
        wire::GearId::RepoLQueryPages { site, query } => {
            repo_l_query_pages(site, query).subscribe_raw(core).await
        }
        wire::GearId::RepoLListPages { site, query } => {
            repo_l_list_pages(site, query).subscribe_raw(core).await
        }
        // Assets are HTTP-only — never shipped over WebTransport. The match is
        // exhaustive on the generated wire enum, but `to_wire_out` drops these.
        wire::GearId::Asset { site, hash, ext } => asset(site, hash, ext).subscribe_raw(core).await,
        // Code blocks likewise: HTTP-only.
        wire::GearId::CodeBlock { space, local, n } => {
            code_block(space, local, n).subscribe_raw(core).await
        }
        // Server-internal resolution dependency of `article_latest`; the client
        // never subscribes to it, but the match must be exhaustive.
        wire::GearId::RepoResource { site, path } => {
            repo_resource(site, path).subscribe_raw(core).await
        }
    }
}

// TODO: Annihilate. Also, no copies.
pub(crate) fn to_wire_out(res: GearResult<KolorinkoRT>) -> Option<wire::GearOut> {
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
            // Server-internal resolution dependencies; the client never
            // subscribes to them.
            GearOutShared::RepoLLocalIdOut(_) => None,
            GearOutShared::RepoLQueryPagesOut(_) => None,
            GearOutShared::RepoLListPagesOut(_) => None,
            // Assets are served over plain HTTP, never the WebTransport wire —
            // the browser fetches them via `<img>`/`<link>`/`url()`. Dropping
            // here keeps their bytes out of the subscription channel.
            GearOutShared::AssetOut(_) => None,
            // Code blocks: same HTTP-only treatment as assets.
            GearOutShared::CodeBlockOut(_) => None,
            // Server-internal: never shipped to the client.
            GearOutShared::RepoResourceOut(_) => None,
        },
        GearResult::Local(_) => None,
    }
}

/// Push `out` on `w` unless its hash equals `last` (the client already holds
/// it); on push, advance `last` to the new hash.
async fn push_if_changed<W: AsyncWrite + Unpin>(
    w: &mut W,
    out: &wire::GearOut,
    last: &mut Option<String>,
) -> io::Result<()> {
    let hash = out_hash(out);
    if last.as_deref() != Some(&hash) {
        write_frame(
            w,
            &ServerMsg::Push {
                out: out.clone(),
                hash: hash.clone(),
            },
        )
        .await?;
        *last = Some(hash);
    }
    Ok(())
}

/// SHA-256 (hex) of a wire `GearOut`'s JSON encoding — the content hash the
/// protocol skips unchanged pushes by (see [`kolorinko_rt::wire`]). Shared
/// with the SSR state embedder ([`crate::ssr`]) so a hydrated page hashes
/// exactly like a pushed one.
pub(crate) fn out_hash(out: &wire::GearOut) -> String {
    let json = serde_json::to_vec(out).expect("GearOut serializes");
    ring::digest::digest(&ring::digest::SHA256, &json)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
    use std::{io, thread, time::Duration};

    /// A compio runtime, retrying transient `ENOMEM`: io_uring ring allocation
    /// fails under system memory pressure (many test processes at once) and
    /// passes on a retry — see `dentrado::core::db::build_core_runtime`.
    fn new_runtime() -> compio::runtime::Runtime {
        for attempt in 0..5u32 {
            match compio::runtime::Runtime::new() {
                Ok(rt) => return rt,
                Err(e) if e.kind() == io::ErrorKind::OutOfMemory && attempt < 4 => {
                    thread::sleep(Duration::from_millis(10 << attempt));
                }
                Err(e) => panic!("compio runtime build failed: {e}"),
            }
        }
        unreachable!()
    }

    #[test]
    fn reuseport_shared_bind() {
        // The invariant the whole multi-core QUIC design rests on: two sockets
        // bind the same port. Catches a silent regression if set_reuse_port
        // were reordered after bind (it would no-op, second bind → EADDRINUSE).
        new_runtime().block_on(async {
            let a = super::reuseport_udp_socket("127.0.0.1:0").unwrap();
            let port = a.local_addr().unwrap().port();
            let _b = super::reuseport_udp_socket(&format!("127.0.0.1:{port}")).unwrap();
        });
    }
}
