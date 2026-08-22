//! HTTPS (HTTP/1.1 over TLS) bootstrap server: one TCP listener per core.
//!
//! This serves the **page** (HTTPS / HTTP/1.1) so it loads cleanly in a browser
//! with a real lock icon (mkcert). It binds the **same** `host:port` as the
//! QUIC endpoint — TCP and UDP are independent socket namespaces, so they
//! coexist on `:4433`. Each core binds with `SO_REUSEPORT`; the kernel hashes
//! each connection to one core.
//!
//! This bootstrap also drives the **HTTP/1.1 → HTTP/3 upgrade**. In **pooling
//! mode** (no `--inject-wt-hash`) the QUIC endpoint presents the same
//! CA-trusted cert used here, so every response carries
//! `Alt-Svc: h3="<port>"` and the browser upgrades subsequent fetches to
//! HTTP/3 over QUIC. In **hash-pinning mode** (`--inject-wt-hash`) the QUIC
//! endpoint presents a self-signed cert the browser can't validate for a fetch,
//! so we emit **no `Alt-Svc`** — advertising one would only make the browser
//! try, fail, and mark the origin's QUIC broken (which would also break
//! WebTransport). The page stays on TCP in that mode; WebTransport owns QUIC.
//!
//! Static assets and mirrored `/repo/` assets are served as zstd when the
//! client advertises `Accept-Encoding: zstd` (the stored [`Body`] is already
//! compressed; otherwise it's decompressed server-side). The `/repo/` path goes
//! through the [`RepoAsset`] gear, so its bytes are cached shared across cores.
//!
//! [`RepoAsset`]: kolorinko_rt gear

use std::{collections::HashMap, io, rc::Rc, sync::Arc};

use compio::{
    buf::BufResult,
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpSocket},
    runtime,
    tls::TlsAcceptor,
};
use log::{info, warn};

use dentrado::core::{core_ctx::Core, storage::InMemoryStorage};
use kolorinko_rt::Body;

use crate::respond;
use crate::runtime::KolorinkoRT;
use crate::tls::https_server_config;

/// Bind `addr` with `SO_REUSEPORT`, wrap each connection in TLS, and serve the
/// HTTP/1.1 page. (WebTransport is separate — see [`crate::server`].)
///
/// `assets` is the single shared asset map (loaded once in [`crate::lib`]);
/// `core` routes `/repo/` asset reads through the [`RepoAsset`] gear.
///
/// [`RepoAsset`]: kolorinko_rt gear
///
/// Runs forever; only returns on a fatal listener error.
pub(crate) async fn serve(
    addr: &str,
    assets: Arc<HashMap<String, Body>>,
    core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    inject_wt_hash: bool,
) -> io::Result<()> {
    let acceptor = TlsAcceptor::from(https_server_config()?);
    let listener = bind_reuseport(addr).await?;
    let local = listener.local_addr()?;
    info!("kolorinko https bootstrap listening on {local} (reuse_port)");

    // `Alt-Svc` advertises the HTTP/3 upgrade. Only in pooling mode: there the
    // QUIC endpoint presents the CA-trusted cert, so the browser's upgrade
    // handshake actually validates. In hash-pinning mode the QUIC cert is
    // self-signed; advertising an upgrade would make the browser try, fail, and
    // mark the origin's QUIC broken (breaking WebTransport too), so we stay
    // silent. TCP and UDP share the same `host:port`, so the advertised port is
    // this listener's port.
    let alt_svc = if inject_wt_hash {
        None
    } else {
        Some(format!("h3=\":{}\"; ma=86400", local.port()))
    };

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let assets = assets.clone();
                let acceptor = acceptor.clone();
                let alt_svc = alt_svc.clone();
                let core = core.clone();
                runtime::spawn(async move {
                    let mut stream = match acceptor.accept(stream).await {
                        Ok(s) => s,
                        Err(e) => {
                            warn!("tls handshake {peer}: {e}");
                            return;
                        }
                    };
                    if let Err(e) = handle_conn(&mut stream, &assets, &core, alt_svc.as_deref()).await
                        && !is_disconnect(&e)
                    {
                        warn!("conn {peer}: {e}");
                    }
                })
                .detach();
            }
            Err(e) => warn!("accept: {e}"),
        }
    }
}

/// Create a `TcpListener` on `addr` with `SO_REUSEADDR` and `SO_REUSEPORT` set
/// before binding, so every core can bind the same port.
async fn bind_reuseport(addr: &str) -> io::Result<TcpListener> {
    use std::net::SocketAddr;
    let sa: SocketAddr = addr
        .parse()
        .map_err(|e| io::Error::other(format!("invalid bind addr {addr:?}: {e}")))?;
    let sock = if sa.is_ipv4() {
        TcpSocket::new_v4().await?
    } else {
        TcpSocket::new_v6().await?
    };
    sock.set_reuseaddr(true)?;
    sock.set_reuseport(true)?;
    sock.bind(sa).await?;
    sock.listen(128).await
}

/// Handle one TLS connection: resolve the request through [`crate::respond`]
/// (static assets, `/repo/` assets, SSR'd pages — all stored/served as a
/// [`Body`], zstd to clients that accept it). The WT cert hash (if in
/// hash-pinning mode) was already injected into `/index.html` once at load
/// time by [`crate::assets::load_assets`].
async fn handle_conn<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    assets: &Arc<HashMap<String, Body>>,
    core: &Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>,
    alt_svc: Option<&str>,
) -> io::Result<()> {
    let head = read_request_head(stream).await?;
    let Some((method, path)) = parse_request_line(&head) else {
        write_http(
            stream,
            400,
            "text/plain",
            None,
            b"bad request\n",
            "no-store",
            None,
            alt_svc,
        )
        .await?;
        return Ok(());
    };

    if method != "GET" && method != "HEAD" {
        write_http(
            stream,
            405,
            "text/plain",
            None,
            b"method not allowed\n",
            "no-store",
            None,
            alt_svc,
        )
        .await?;
        return Ok(());
    }

    let reply = respond::resolve(path, accepts_zstd(&head), assets, core, host_of(&head)).await;
    write_http(
        stream,
        reply.status,
        reply.mime,
        reply.served.encoding,
        &reply.served.bytes,
        reply.cache_control,
        reply.location.as_deref(),
        alt_svc,
    )
    .await?;
    let _ = stream.shutdown().await; // send close_notify for a clean TLS close
    Ok(())
}

/// Whether the request head advertises `zstd` in `Accept-Encoding` (header name
/// is case-insensitive). Scans the raw head string instead of a parsed header
/// map: the HTTP/1.1 bootstrap reads only the request line + head bytes.
fn accepts_zstd(head: &str) -> bool {
    head.lines().any(|l| {
        let mut parts = l.splitn(2, ':');
        matches!(parts.next().map(str::trim), Some(name) if name.eq_ignore_ascii_case("accept-encoding"))
            && parts.next().is_some_and(|v| v.contains("zstd"))
    })
}

/// The request's `Host` header value — the page's own origin, with which the
/// SSR document absolutizes its OpenGraph URLs. Same raw-head scan as
/// [`accepts_zstd`].
fn host_of(head: &str) -> Option<&str> {
    head.lines().find_map(|l| {
        let mut parts = l.splitn(2, ':');
        match parts.next().map(str::trim) {
            Some(name) if name.eq_ignore_ascii_case("host") => parts.next().map(str::trim),
            _ => None,
        }
    })
}

/// Read bytes until the end of the HTTP request head (`\r\n\r\n`).
async fn read_request_head<S: AsyncRead + Unpin>(stream: &mut S) -> io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    loop {
        let chunk: Vec<u8> = vec![0u8; 2048];
        let BufResult(res, chunk) = AsyncRead::read(stream, chunk).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        buf.extend_from_slice(&chunk[..n]);
        if find_double_crlf(&buf).is_some() {
            break;
        }
        if buf.len() > 1 << 16 {
            return Err(io::Error::other("HTTP request head too large"));
        }
    }
    String::from_utf8(buf).map_err(|_| io::Error::other("non-utf8 request head"))
}

/// Write a complete HTTP/1.1 response (head + body) and flush. `encoding`, when
/// present, is sent as `Content-Encoding` (zstd for a compressed [`Body`]).
/// `alt_svc`, when present, advertises the HTTP/3 upgrade.
async fn write_http<S: AsyncWrite + Unpin>(
    stream: &mut S,
    status: u16,
    mime: &str,
    encoding: Option<&str>,
    body: &[u8],
    cache_control: &str,
    location: Option<&str>,
    alt_svc: Option<&str>,
) -> io::Result<()> {
    let status_text = match status {
        200 => "OK",
        301 => "Moved Permanently",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let mut head = format!(
        "HTTP/1.1 {status} {status_text}\r\nContent-Type: {mime}\r\nContent-Length: {len}\r\nConnection: close\r\nCache-Control: {cache_control}\r\nVary: Accept-Encoding\r\n",
        len = body.len(),
    );
    if let Some(enc) = encoding {
        head.push_str("Content-Encoding: ");
        head.push_str(enc);
        head.push_str("\r\n");
    }
    if let Some(loc) = location {
        head.push_str("Location: ");
        head.push_str(loc);
        head.push_str("\r\n");
    }
    if let Some(alt) = alt_svc {
        head.push_str("Alt-Svc: ");
        head.push_str(alt);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut full = Vec::with_capacity(head.len() + body.len());
    full.extend_from_slice(head.as_bytes());
    full.extend_from_slice(body);
    let BufResult(res, _) = stream.write_all(full).await;
    res?;
    // TLS encrypts via rustls, which buffers ciphertext; flush pushes it to the
    // socket before the stream drops (otherwise the peer sees a truncated record).
    stream.flush().await?;
    Ok(())
}

/// Parse the request line into `(method, raw_path)` — `raw_path` includes any
/// query string; callers strip it as needed.
fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

/// Index of the `\r\n\r\n` terminator, if present.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

fn is_disconnect(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}
