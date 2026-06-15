use dentrado::{
    core::{
        core_ctx::Core,
        db::{Db, DbConfig, Doorbell},
    },
    types::NodeId,
};
use log::{error, warn};
use std::{
    collections::HashMap,
    env::{VarError, var},
    iter,
    num::NonZero,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    thread::available_parallelism,
    time::{SystemTime, UNIX_EPOCH},
};

use crate::runtime::KolorinkoRT;
mod runtime;
mod safe_path;
mod web;
mod wikidot_page;
pub mod wikidot_parser;

#[derive(Clone, Copy)]
pub struct Timestamp(pub u32);

pub fn impure_now() -> Timestamp {
    Timestamp(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as u32,
    )
}

/// The export repo served by [`runtime::GearId::Repo`].
const REPO_URL: &str = "https://github.com/luna-spirito/wikidot-kolorinko-export.git";
/// Default seconds between forced `git pull`s of the repo.
const DEFAULT_REPO_INTERVAL: u32 = 300;

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

    let config = DbConfig::<KolorinkoRT> {
        num_cores: cores,
        node_id: NodeId(0),
        module: Arc::new(()),
        peers: HashMap::new(),
        doorbells: iter::repeat_with(Doorbell::new)
            .take(cores.get() as usize)
            .collect(),
    };

    let repo_meta = make_repo_meta();
    let bind_addr = var("KOLORINKO_BIND").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let web_dist =
        PathBuf::from(var("KOLORINKO_WEB_DIST").unwrap_or_else(|_| "../kolorinko-web/dist".into()));

    // One web worker per core. Each binds the same `(addr, port)` with
    // SO_REUSEPORT; the kernel hashes each incoming connection to a single
    // core, preserving the thread-per-core, shared-nothing model.
    let worker = move |core: Rc<Core<KolorinkoRT>>| {
        let addr = bind_addr.clone();
        let dist = web_dist.clone();
        let meta = repo_meta.clone();
        async move {
            if let Err(e) = web::serve(core, &addr, dist, meta).await {
                error!("web worker exited: {e}");
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
    let dir = var("REPO_DIR").unwrap_or_else(|_| "kolorinko-repo".to_string());
    let repo_dir: &'static Path = Box::leak(PathBuf::from(dir).into_boxed_path());
    let interval = var("REPO_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REPO_INTERVAL);
    wikidot_page::RepoMeta::new(REPO_URL, repo_dir, interval)
}
