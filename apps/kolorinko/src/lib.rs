use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
        storage::InMemoryStorage,
    },
    types::NodeId,
};
use log::{error, info, warn};
use std::{
    collections::HashMap,
    env::{VarError, var},
    iter,
    num::NonZero,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    thread::available_parallelism,
};

use crate::runtime::KolorinkoRT;
mod assets;
mod runtime;
mod safe_path;
mod server;
mod tls;
mod web;
mod wikidot_page;
pub mod wikidot_parser;

/// The export repo mirrored by the local `repo` oracle gear (read through the
/// `repo_l_article` lens).
const REPO_URL: &str = "https://github.com/luna-spirito/wikidot-kolorinko-export.git";
/// Default seconds between forced `git pull`s of the repo (15 minutes).
const DEFAULT_REPO_INTERVAL: u32 = 900;

pub fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cores = match var("NUM_CORES").map(|x| x.parse()) {
        Ok(Ok(x)) => x,
        e => {
            if !matches!(e, Err(VarError::NotPresent)) {
                warn!("NUM_CORES ignored: couldn't parse as number")
            }
            NonZero::new(
                available_parallelism()
                    .map(|x| u32::try_from(x.get()).unwrap())
                    .unwrap_or(4),
            )
            .unwrap()
        }
    };

    let config = DbConfig::<KolorinkoRT, InMemoryStorage<KolorinkoRT>> {
        num_cores: cores,
        node_id: NodeId(0),
        module: Arc::new(()),
        peers: HashMap::new(),
        doorbells: iter::repeat_with(Doorbell::new)
            .take(cores.get() as usize)
            .collect(),
        make_storage: Arc::new(|| InMemoryStorage::<KolorinkoRT>::default()),
    };

    let repo_meta = make_repo_meta();
    // One bind address for everything. The HTTPS bootstrap binds TCP on it; the
    // HTTP/3 + WebTransport server binds QUIC (UDP) on the same `host:port` —
    // TCP and UDP coexist, so the site is a single `https://<host>:<port>`
    // origin and `Alt-Svc` advertises that same port for the H3 upgrade.
    let bind = var("KOLORINKO_BIND").unwrap_or_else(|_| "[::1]:4433".to_string());
    let web_dist = PathBuf::from(var("KOLORINKO_WEB_DIST").unwrap_or_else(|_| {
        // Resolve against the crate directory so the default is correct no
        // matter which CWD the binary is launched from.
        format!("{}/../kolorinko-web/dist", env!("CARGO_MANIFEST_DIR"))
    }));

    // compio-quic can't SO_REUSEPORT across cores, so the QUIC/UDP listener is
    // bound by one core; gear work still routes to all cores via
    // `db_run_gear`. The TCP bootstrap, by contrast, is bound by every core
    // with SO_REUSEPORT (kernel-hashed per connection).
    let bind_claim = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    // `--inject-wt-hash` selects the WebTransport trust policy for the whole
    // process:
    // - absent (default) → **pooling**: the QUIC endpoint presents the CA-trusted
    //   cert, no hash is injected, the HTTP/1.1 bootstrap advertises `Alt-Svc`
    //   (so the browser upgrades to HTTP/3), and the client opens WebTransport
    //   with `allowPooling: true` (sharing the HTTP/3 connection pool).
    // - present → **hash-pinning**: the QUIC endpoint presents a short-lived
    //   self-signed cert, its SHA-256 is injected once into the page, no
    //   `Alt-Svc` is sent, and the client pins it via `serverCertificateHashes`.
    let inject_wt_hash = std::env::args().any(|a| a == "--inject-wt-hash");
    if inject_wt_hash {
        info!("--inject-wt-hash: WebTransport will use serverCertificateHashes");
    } else {
        info!("no --inject-wt-hash: WebTransport will use allowPooling + Alt-Svc upgrade");
    }

    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let bind = bind.clone();
        let dist = web_dist.clone();
        let meta = repo_meta.clone();
        // TODO: Just pass core_num to the worker, and use that. This is hack.
        // Clone the Arc so the `async move` block below doesn't move it out of
        // the closure on each call (which would make the closure `FnOnce`).
        let claim = bind_claim.clone();
        async move {
            // One core also drives the QUIC/H3+WT accept loop (UDP) as a
            // background task before joining the others on the TCP bootstrap.
            if claim
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_ok()
            {
                let core_h3 = core.clone();
                let dist_h3 = dist.clone();
                let meta_h3 = meta.clone();
                let bind_h3 = bind.clone();
                compio::runtime::spawn(async move {
                    if let Err(e) =
                        server::serve(core_h3, &bind_h3, dist_h3, meta_h3, inject_wt_hash).await
                    {
                        error!("h3 server exited: {e}");
                    }
                })
                .detach();
            }
            // Every core serves the HTTPS bootstrap over TCP.
            if let Err(e) = web::serve(&bind, dist, inject_wt_hash).await {
                error!("https bootstrap exited: {e}");
            }
        }
    };

    // Keep the `Db` alive for the life of the process: its `Drop` impl sends
    // `Shutdown` to every core and joins the worker threads, so dropping a
    // temporary here would tear the whole server down before it serves a
    // single request.
    let _db = Db::start_with_worker(config, worker)?;

    // The worker futures live on the per-core threads spawned by
    // `start_with_worker`; the main thread just has to stick around so `_db`
    // isn't dropped. Park forever; the process is stopped by a signal.
    loop {
        std::thread::park();
    }
}

/// Build the [`RepoMeta`] from `REPO_DIR` / `REPO_INTERVAL` env vars.
///
/// `RepoMeta` holds `&'static` fields, so a runtime-configured path is leaked
/// once at startup (it lives for the whole process anyway).
fn make_repo_meta() -> wikidot_page::RepoMeta {
    // Default to a project-local clone (gitignored via `.*`); the server
    // auto-clones it from `REPO_URL` on first use if absent.
    let dir = var("REPO_DIR").unwrap_or_else(|_| ".kolorinko/repo".to_string());
    let repo_dir: &'static Path = Box::leak(PathBuf::from(dir).into_boxed_path());
    let interval = var("REPO_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REPO_INTERVAL);
    wikidot_page::RepoMeta::new(REPO_URL, repo_dir, interval)
}
