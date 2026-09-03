// Evaluating `Send` for the publication worker's spawn closure recurses
// through flume's `Hook<T>` → `Option<T>` → the carried `RepoSnapshot` (deep
// imbl/wikitext nesting) and brushes the default limit; harmless to raise.
#![recursion_limit = "256"]

use clap::Parser;
use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
        storage::InMemoryStorage,
    },
    types::NodeId,
};
use indexmap::IndexMap;
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
mod globals;
mod metrics;
mod render_cli;
mod repo;
mod respond;
mod runtime;
mod server;
mod ssr;
mod tls;
mod web;
mod wikidot_page;
pub mod wikidot_parser;
/// Process configuration, loaded once from a `.toml` file whose path is passed
/// as the sole CLI argument. Every setting that used to live in env vars or
/// `const`s (publication source/interval, bind address, frontend dist,
/// WebTransport trust mode) is gathered here.
#[derive(Debug, Deserialize)]
struct Config {
    evakuilo: EvakuiloCfg,
    server: ServerCfg,
    /// Wikidot-export sites to register as content spaces, in serving order
    /// (the first entry's landing page is what the bare `/` serves): site →
    /// [`globals::SiteCfg`] (`landing`, `domains`). The canonical space id is
    /// derived as `SHA-256("wikidot-evakuilo/v1/<site>")[0..16]` — see
    /// [`globals::evakuilo_space_id`]. An `IndexMap` keeps the TOML document
    /// order, which defines `/`'s serving space.
    #[serde(rename = "ensure-evakuilo-sites", default)]
    ensure_evakuilo_sites: IndexMap<String, globals::SiteCfg>,
}

#[derive(Debug, Deserialize)]
struct EvakuiloCfg {
    /// The evakuilo publication root — the daemon's `out/` directory, the one
    /// holding the `<site>/` publications kolorinko serves.
    dir: String,
    /// Seconds between publication rescans (mtime checks per site; a scan
    /// of an unchanged corpus is a few `stat`s).
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
    /// One-shot SSR render, no servers. `<site>/<page>` renders that page to
    /// stdout; a bare `<site>` mass-renders every page of the site into a
    /// directory of standalone `.html` files (default
    /// `.kolorinko/render/<site>`).
    Render {
        #[arg(long)]
        inject: bool,
        /// Whole-site render only: output directory
        /// (default `.kolorinko/render/<site>`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Whole-site render only: replace the output directory if it exists.
        #[arg(long)]
        force: bool,
        /// `<site>/<page>`/`<site>/<category>/<page>` for one page, bare
        /// `<site>` for the whole site.
        target: String,
    },
}

pub fn main() -> anyhow::Result<()> {
    env_logger::init();

    let cli = Cli::parse();
    match cli.commands {
        None => run_server(load_config(&cli.config_path)?),
        Some(Command::Render {
            inject,
            out,
            force,
            target,
        }) => render_cli::run_cli(cli.config_path, &target, inject, out, force),
    }
}

fn load_config(config_path: &Path) -> anyhow::Result<Config> {
    toml::from_str(&std::fs::read_to_string(config_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {}: {e}", config_path.display()))
}

/// Initialize the process-global config from the parsed file — the single
/// entry point shared by the server ([`run_server`]) and the render CLI
/// ([`render_cli`]), so both derive the same space registry.
fn init_globals(config: &Config) -> anyhow::Result<()> {
    globals::init(
        &config.evakuilo.dir,
        config.evakuilo.interval,
        &config.ensure_evakuilo_sites,
    )
}

/// `kolorinko <config.toml>` — run the H3 + WebTransport server.
fn run_server(config: Config) -> anyhow::Result<()> {
    let cores = core_count();
    metrics::init(cores);

    init_globals(&config)?;
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
        let assets = assets.clone();
        async move {
            metrics::register(core.core_id(), core.stats());
            // One aggregator for the whole process; core 0 is the convention.
            if core.core_id() == 0 {
                compio::runtime::spawn(metrics::log_loop()).detach();
            }
            let core_h3 = core.clone();
            let bind_h3 = bind.clone();
            let assets_h3 = assets.clone();
            compio::runtime::spawn(async move {
                if let Err(e) = server::serve(core_h3, &bind_h3, assets_h3, inject_wt_hash).await {
                    error!("h3 server exited: {e}");
                }
            })
            .detach();
            if let Err(e) = web::serve(&bind, assets, core, inject_wt_hash).await {
                error!("https bootstrap exited: {e}");
            }
        }
    };

    // The servers live on the cores' runtimes: a core panic cascades — every
    // core dies, the tasks are cancelled — and `park` returns, letting the
    // process exit so the supervisor can restart it. Without this the process
    // would linger as a zombie serving nothing.
    let mut db = Db::start_with_worker(db_config(cores), worker)?;
    db.park();
    anyhow::bail!("all core threads exited, the Db died (core panic)");
}

/// Core count for multi-core runs: the `NUM_CORES` env override if set (and
/// parseable), else OS-reported parallelism (fallback 4). Shared by the
/// server ([`run_server`]) and the whole-site render ([`render_cli`]).
pub(crate) fn core_count() -> NonZero<u32> {
    NonZero::new(
        std::env::var("NUM_CORES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| {
                available_parallelism()
                    .map(|x| u32::try_from(x.get()).unwrap())
                    .unwrap_or(4)
            }),
    )
    .unwrap()
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
