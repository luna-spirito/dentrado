//! HTTP/1.1 + WebSocket server, one listener per dentrado core.
//!
//! Each core calls [`serve`], which binds the same `(addr, port)` with
//! `SO_REUSEPORT`. The kernel hashes each incoming connection to exactly one
//! core, giving a true thread-per-core, shared-nothing accept model.
//!
//! Request routing (per connection):
//! - `GET /`        → `index.html` (the Leptos CSR entry point)
//! - `GET /<asset>` → other files from the built frontend dir
//! - `GET /ws`      → WebSocket upgrade → [`ws_loop`]
//!
//! The WebSocket loop round-trips a [`dentrado`] gear: the client sends a
//! command, the owning core runs it via [`Core::db_run_gear`] (routing to the
//! correct core if this one does not own it), and replies with the result.

use std::{
    collections::HashMap,
    io,
    path::{Path, PathBuf},
    rc::Rc,
};

use compio::{
    buf::BufResult,
    io::{AsyncRead, AsyncWriteExt},
    net::{TcpListener, TcpSocket, TcpStream},
    runtime,
    ws::{Config, WebSocketStream, tungstenite},
};
use dentrado::{core::core_ctx::Core, wire::WireLocCtx};
use kolorinko_wikitext::Content;
use log::{error, info, warn};
use serde::{Deserialize, Serialize};

use crate::{
    runtime::{GearId, GearOut, KolorinkoRT},
    safe_path::SafePathComponent,
    wikidot_page::RepoMeta,
};

/// Minimal placeholder served when no built frontend is present, so the server
/// is usable before `trunk build` has run.
const PLACEHOLDER_INDEX: &str = "<!doctype html>\
<html><head><meta charset=\"utf-8\"><title>kolorinko</title></head>\
<body><h1>kolorinko</h1>\
<p>No built frontend found. Build it with \
<code>trunk build</code> in <code>apps/kolorinko-web</code>.</p></body></html>";

/// Bind `addr` with `SO_REUSEPORT` and run the accept loop for this core.
///
/// Runs forever; only returns on a fatal listener error. Each core binds the
/// same `(addr, port)`; the kernel hashes each incoming connection to exactly
/// one core, preserving the thread-per-core, shared-nothing model.
pub(crate) async fn serve(
    core: Rc<Core<KolorinkoRT>>,
    addr: &str,
    assets_dir: PathBuf,
    repo_meta: RepoMeta,
) -> io::Result<()> {
    let assets = Rc::new(load_assets(&assets_dir));
    let listener = bind_reuseport(addr).await?;
    let local = listener.local_addr()?;
    info!("kolorinko worker listening on {local} (reuse_port)");

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let core = core.clone();
                let assets = assets.clone();
                let repo_meta = repo_meta.clone();
                runtime::spawn(async move {
                    if let Err(e) = handle_conn(stream, &core, &assets, repo_meta).await
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
///
/// `addr` must parse to a single `SocketAddr` (e.g. `"0.0.0.0:8080"`).
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

/// Handle a single TCP connection: route by request line.
async fn handle_conn(
    mut stream: TcpStream,
    core: &Rc<Core<KolorinkoRT>>,
    assets: &HashMap<String, Vec<u8>>,
    repo_meta: RepoMeta,
) -> io::Result<()> {
    let head = read_request_head(&mut stream).await?;
    let Some((method, path)) = parse_request_line(&head) else {
        write_http(&mut stream, 400, "text/plain", b"bad request\n").await?;
        return Ok(());
    };

    if method != "GET" && method != "HEAD" {
        write_http(&mut stream, 405, "text/plain", b"method not allowed\n").await?;
        return Ok(());
    }

    if path == "/ws" {
        upgrade_ws(&mut stream, &head).await?;
        let mut ws = WebSocketStream::from_raw_socket(
            stream,
            tungstenite::protocol::Role::Server,
            Config::default(),
        )
        .await;
        return ws_loop(&mut ws, core, repo_meta).await;
    }

    let key = if path == "/" { "/index.html" } else { path };
    match assets.get(key) {
        Some(bytes) => write_http(&mut stream, 200, mime_for(key), bytes).await,
        None => write_http(&mut stream, 404, "text/plain", b"not found\n").await,
    }
}

/// Read bytes until the end of the HTTP request head (`\r\n\r\n`).
///
/// Uses compio's owned-buffer [`AsyncRead`] directly so we can stop as soon as
/// the header terminator appears without consuming any body bytes.
async fn read_request_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    loop {
        let chunk: Vec<u8> = vec![0u8; 2048];
        let BufResult(res, chunk) = AsyncRead::read(stream, chunk).await;
        let n = res?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        // compio resizes `chunk` to exactly the `n` bytes read.
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

/// Write the `101 Switching Protocols` WebSocket handshake response and flush.
///
/// No leftover request bytes are passed on: a WebSocket `GET` upgrade carries
/// no body, so once `\r\n\r\n` is consumed the socket is clean for framing.
async fn upgrade_ws(stream: &mut TcpStream, head: &str) -> io::Result<()> {
    let key = get_header(head, "sec-websocket-key")
        .ok_or_else(|| io::Error::other("missing Sec-WebSocket-Key"))?;
    let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
    let resp = format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    );
    let BufResult(res, _) = stream.write_all(resp.into_bytes()).await;
    res
}

/// A client request over the WebSocket.
#[derive(Deserialize)]
#[serde(tag = "t")]
enum Request {
    /// Load a parsed page: runs `GearId::Load` on the owning core.
    #[serde(rename = "load")]
    Load {
        site: String,
        category: Option<String>,
        page: String,
    },
    /// Diagnostic: run `GearId::Repo` and report the resolved path.
    #[serde(rename = "repo")]
    Repo,
}

/// A server reply over the WebSocket.
#[derive(Serialize)]
#[serde(tag = "t")]
enum Reply {
    /// A page was loaded; `content` is the parsed Wikidot AST.
    #[serde(rename = "page")]
    Page { content: Content },
    /// `GearId::Repo` resolved to this filesystem path.
    #[serde(rename = "repo")]
    Repo { path: String },
    #[serde(rename = "error")]
    Error { error: String },
}

impl Reply {
    fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{\"t\":\"error\"}".into())
    }
}

/// WebSocket message loop: parse a JSON [`Request`], run the matching gear via
/// [`Core::db_run_gear`] (which routes to the owning core), and reply.
async fn ws_loop(
    ws: &mut WebSocketStream<TcpStream>,
    core: &Rc<Core<KolorinkoRT>>,
    repo_meta: RepoMeta,
) -> io::Result<()> {
    loop {
        let msg = match ws.read().await {
            Ok(m) => m,
            Err(e) => {
                warn!("ws read: {e}");
                return Ok(());
            }
        };
        let text = match msg {
            tungstenite::Message::Text(t) => t,
            tungstenite::Message::Close(_) | tungstenite::Message::Ping(_) => return Ok(()),
            _ => continue,
        };
        let req: Request = match serde_json::from_str(text.as_str()) {
            Ok(r) => r,
            Err(e) => {
                send(
                    ws,
                    Reply::Error {
                        error: format!("bad request: {e}"),
                    },
                )
                .await;
                continue;
            }
        };
        let reply = handle_request(core, &repo_meta, req).await;
        send(ws, reply).await;
    }
}

/// Dispatch a [`Request`] to a gear and turn the [`GearOut`] into a [`Reply`].
async fn handle_request(core: &Rc<Core<KolorinkoRT>>, repo_meta: &RepoMeta, req: Request) -> Reply {
    let ctx = WireLocCtx::default();
    match req {
        Request::Repo => match core.db_run_gear(GearId::Repo(repo_meta.clone()), ctx).await {
            Ok(GearOut::RepoOut(path)) => Reply::Repo {
                path: path.as_path().display().to_string(),
            },
            Ok(_) => Reply::Error {
                error: "unexpected gear output".into(),
            },
            Err(e) => Reply::Error {
                error: format!("gear error: {e:?}"),
            },
        },
        Request::Load {
            site,
            category,
            page,
        } => {
            let Some(site) = SafePathComponent::new(site) else {
                return Reply::Error {
                    error: "invalid site".into(),
                };
            };
            let Some(page) = SafePathComponent::new(page) else {
                return Reply::Error {
                    error: "invalid page".into(),
                };
            };
            let category = match category {
                None => None,
                Some(c) => match SafePathComponent::new(c) {
                    Some(sp) => Some(sp),
                    None => {
                        return Reply::Error {
                            error: "invalid category".into(),
                        };
                    }
                },
            };
            let gear = GearId::Load {
                repo: repo_meta.clone(),
                site,
                slug: (category, page),
            };
            match core.db_run_gear(gear, ctx).await {
                Ok(GearOut::LoadOut(content)) => Reply::Page {
                    content: (*content).clone(),
                },
                Ok(_) => Reply::Error {
                    error: "unexpected gear output".into(),
                },
                Err(e) => Reply::Error {
                    error: format!("gear error: {e:?}"),
                },
            }
        }
    }
}

/// Serialize a [`Reply`] and send it, logging (not propagating) send errors.
async fn send(ws: &mut WebSocketStream<TcpStream>, reply: Reply) {
    if let Err(e) = ws
        .send(tungstenite::Message::Text(reply.to_json().into()))
        .await
    {
        warn!("ws send: {e}");
    }
}

/// Write a complete HTTP/1.1 response (head + body) and flush.
async fn write_http(
    stream: &mut TcpStream,
    status: u16,
    mime: &str,
    body: &[u8],
) -> io::Result<()> {
    let status_text = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "OK",
    };
    let head = format!(
        "HTTP/1.1 {status} {status_text}\r\n\
         Content-Type: {mime}\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n",
        len = body.len()
    );
    let mut full = Vec::with_capacity(head.len() + body.len());
    full.extend_from_slice(head.as_bytes());
    full.extend_from_slice(body);
    let BufResult(res, _) = stream.write_all(full).await;
    res
}

// ---- small HTTP parsing helpers ------------------------------------------------

fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let path = parts.next()?;
    Some((method, path))
}

/// Case-insensitive header lookup over the raw request head.
fn get_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    for line in head.lines().skip(1) {
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case(name)
        {
            return Some(v.trim());
        }
    }
    None
}

/// Index of the `\r\n\r\n` terminator, if present.
fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Map a path to a MIME type by extension.
fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") | Some("htm") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("mjs") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        _ => "application/octet-stream",
    }
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

// ---- frontend asset loading ---------------------------------------------------

/// Load the built frontend into a `path → bytes` map, keyed by request path
/// (e.g. `/index.html`, `/pkg/kolorinko_web.js`).
///
/// Done once per worker at startup. Uses blocking `std::fs` because it is a
/// one-time read of (typically) a few small files, well under a millisecond.
fn load_assets(dir: &Path) -> HashMap<String, Vec<u8>> {
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
