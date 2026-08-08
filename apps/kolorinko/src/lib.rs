use clap::Parser;
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
    sync::Arc,
    thread::available_parallelism,
};

use crate::runtime::KolorinkoRT;
mod assets;
mod render_cli;
mod repo;
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

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    commands: Option<Command>,
    config_path: PathBuf,
}

#[derive(clap::Subcommand)]
enum Command {
    Render {
        #[arg(long)]
        inject: bool,
        page: String,
    },
}

pub fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    match cli.commands {
        None => run_server(load_config(&cli.config_path)?),
        Some(Command::Render { inject, page }) => {
            render_cli::run_cli(cli.config_path, page, inject)
        }
    }
}

fn load_config(config_path: &Path) -> anyhow::Result<Config> {
    toml::from_str(&std::fs::read_to_string(config_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_path.display()))
}

/// `kolorinko <config.toml>` — run the H3 + WebTransport server.
fn run_server(config: Config) -> anyhow::Result<()> {
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

    let repo_meta = make_repo_meta(&config.repo);
    let bind = config.server.bind.clone();
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

    // Load the frontend once for the whole process (blocking `std::fs`: a
    // one-time read of a few small files), then share the single map across
    // every core as one `Arc`. Each core holds a refcount bump, never its own
    // copy of the bytes. The WT cert hash (hash-pinning mode) is injected into
    // `/index.html` here, before compression, so it ships in the cached bytes.
    let wt_hash = if inject_wt_hash {
        Some(tls::wt_cert_hash())
    } else {
        None
    };
    let assets = Arc::new(assets::load_assets(
        &PathBuf::from(&config.server.web_dist),
        wt_hash,
    ));

    let worker = move |core: Rc<Core<KolorinkoRT, InMemoryStorage<KolorinkoRT>>>| {
        let bind = bind.clone();
        let meta = repo_meta.clone();
        let assets = assets.clone();
        async move {
            let core_h3 = core.clone();
            let meta_h3 = meta.clone();
            let bind_h3 = bind.clone();
            let assets_h3 = assets.clone();
            compio::runtime::spawn(async move {
                if let Err(e) =
                    server::serve(core_h3, &bind_h3, assets_h3, meta_h3, inject_wt_hash).await
                {
                    error!("h3 server exited: {e}");
                }
            })
            .detach();
            if let Err(e) = web::serve(&bind, assets, meta, core, inject_wt_hash).await {
                error!("https bootstrap exited: {e}");
            }
        }
    };

    // Keep the `Db` alive for the life of the process: its `Drop` impl sends
    // `Shutdown` to every core and joins the worker threads.
    let _db = Db::start_with_worker(db_config(cores), worker)?;

    loop {
        std::thread::park();
    }
}

/// Build a single-node [`DbConfig`] for `cores` cores. Shared by the server
/// ([`run_server`]) and the one-shot render CLI ([`render_cli`]).
fn db_config(cores: NonZero<u32>) -> DbConfig<KolorinkoRT, InMemoryStorage<KolorinkoRT>> {
    DbConfig {
        num_cores: cores,
        node_id: NodeId(0),
        module: std::sync::Arc::new(()),
        peers: HashMap::new(),
        doorbells: iter::repeat_with(Doorbell::new)
            .take(cores.get() as usize)
            .collect(),
        make_storage: std::sync::Arc::new(InMemoryStorage::<KolorinkoRT>::default),
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
