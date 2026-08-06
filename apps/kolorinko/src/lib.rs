use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
        storage::InMemoryStorage,
    },
    types::NodeId,
};
use log::{error, info};
use serde::Deserialize;
use std::{
    collections::HashMap,
    iter,
    num::NonZero,
    path::{Path, PathBuf},
    rc::Rc,
    thread::available_parallelism,
};

use crate::runtime::KolorinkoRT;
mod assets;
mod runtime;
mod server;
mod tls;
mod web;
mod wikidot_page;
pub mod wikidot_parser;

/// Process configuration, loaded once from a `.toml` file whose path is passed
/// as the sole CLI argument. Every setting that used to live in env vars or
/// `const`s (repo source/interval, bind address, frontend dist, WebTransport
/// trust mode) is gathered here.
#[derive(Debug, Deserialize)]
struct Config {
    repo: RepoCfg,
    server: ServerCfg,
}

#[derive(Debug, Deserialize)]
struct RepoCfg {
    url: String,
    dir: String,
    /// Seconds between forced `git pull`s.
    interval: u32,
}

#[derive(Debug, Deserialize)]
struct ServerCfg {
    bind: String,
    /// Path to the built frontend (`kolorinko-web/dist`).
    web_dist: String,
    /// WebTransport trust mode: `true` → short-lived self-signed cert pinned by
    /// SHA-256 (`serverCertificateHashes`); `false` → CA-trusted cert with
    /// HTTP/3 upgrade via `Alt-Svc` and `allowPooling`.
    inject_wt_hash: bool,
    /// Optional browser-trusted (e.g. mkcert) cert+key for the TCP (HTTPS)
    /// bootstrap and, in CA-trust mode, the H3 endpoint. Omit to auto-discover
    /// `.certs/{localhost.pem,localhost-key.pem}` (then a self-signed cert as a
    /// last resort, which browsers refuse).
    cert_file: Option<String>,
    key_file: Option<String>,
}

pub fn main() -> anyhow::Result<()> {
    env_logger::init();

    let config_path = std::env::args()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: kolorinko <config.toml>"))?;
    let config: Config = toml::from_str(&std::fs::read_to_string(&config_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {config_path}: {e}"))?;

    let cores = NonZero::new(
        std::env::var("NUM_CORES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                available_parallelism()
                    .map(|x| u32::try_from(x.get()).unwrap())
                    .unwrap_or(4)
            }),
    )
    .unwrap();

    let db_config = DbConfig::<KolorinkoRT, InMemoryStorage<KolorinkoRT>> {
        num_cores: cores,
        node_id: NodeId(0),
        module: std::sync::Arc::new(()),
        peers: HashMap::new(),
        doorbells: iter::repeat_with(Doorbell::new)
            .take(cores.get() as usize)
            .collect(),
        make_storage: std::sync::Arc::new(|| InMemoryStorage::<KolorinkoRT>::default()),
    };

    let repo_meta = make_repo_meta(&config.repo);
    let bind = config.server.bind.clone();
    let web_dist = PathBuf::from(&config.server.web_dist);
    let inject_wt_hash = config.server.inject_wt_hash;
    tls::set_cert_paths(
        config.server.cert_file.map(PathBuf::from),
        config.server.key_file.map(PathBuf::from),
    );
    if inject_wt_hash {
        info!("inject_wt_hash: WebTransport will use serverCertificateHashes");
    } else {
        info!("no inject_wt_hash: WebTransport will use allowPooling + Alt-Svc upgrade");
    }

    // compio-quic can't SO_REUSEPORT across cores, so the QUIC/UDP listener is
    // bound by one core; gear work still routes to all cores via
    // `db_run_gear`. The TCP bootstrap, by contrast, is bound by every core
    // with SO_REUSEPORT (kernel-hashed per connection).
    let bind_claim = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let bind = bind.clone();
        let dist = web_dist.clone();
        let meta = repo_meta.clone();
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
    // `Shutdown` to every core and joins the worker threads.
    let _db = Db::start_with_worker(db_config, worker)?;

    loop {
        std::thread::park();
    }
}

/// Build the [`RepoMeta`] from the config. `RepoMeta` holds `&'static` fields
/// (it is part of a gear identity), so the runtime-configured strings are leaked
/// once at startup — they live for the whole process anyway.
fn make_repo_meta(cfg: &RepoCfg) -> wikidot_page::RepoMeta {
    let url: &'static str = Box::leak(cfg.url.clone().into_boxed_str());
    let path: &'static Path = Box::leak(PathBuf::from(&cfg.dir).into_boxed_path());
    wikidot_page::RepoMeta::new(url, path, cfg.interval)
}
