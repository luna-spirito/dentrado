use super::*;

// =========================================================================
// Publication worker thread (owns the dataset; serves the async gears)
// =========================================================================

/// A request to the publication worker thread. Each carries its own one-shot
/// reply channel; the oracle core sends one and `.await`s the reply, so it
/// never blocks while the worker decompresses archives on its own thread.
pub(super) enum OutReq {
    /// Rescan the publication and rebuild what drifted. The reply is `Some`
    /// only when the dataset actually changed; `None` lets the caller keep
    /// its cached `Rc`.
    Tick {
        reply: flume::Sender<Option<RepoSnapshot>>,
    },
    /// The current dataset without rescanning (cold start / first non-tick run).
    Snapshot { reply: flume::Sender<RepoSnapshot> },
}

/// Cloneable, `Send` handle to the publication worker thread, held by the
/// [`repo`] oracle's cache so all synchronous filesystem work — manifest
/// parsing, zstd decompression, body materialisation — happens off the
/// async core. Body text crosses this channel only inside the snapshot
/// itself (see [`RepoSnapshot`]).
///
/// [`repo`]: crate::wikidot_page::repo
#[derive(Clone)]
pub(crate) struct OutMailbox(pub(super) flume::Sender<OutReq>);

impl OutMailbox {
    /// Rescan + rebuild what drifted; `Some` only when the dataset changed.
    pub(super) async fn tick(&self) -> Option<RepoSnapshot> {
        let (tx, rx) = flume::bounded(1);
        if self.0.send_async(OutReq::Tick { reply: tx }).await.is_err() {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }

    /// Current dataset without rescanning.
    pub(super) async fn snapshot(&self) -> RepoSnapshot {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(OutReq::Snapshot { reply: tx })
            .await
            .is_err()
        {
            return RepoSnapshot::default();
        }
        rx.recv_async()
            .await
            .unwrap_or_else(|_| RepoSnapshot::default())
    }
}

/// The stamps of the three publication files (`pages.json`, `files.json`,
/// `shell`) — the site-level change gate. A site is re-read only when its
/// stamp triple drifts; the publisher's write-if-changed discipline keeps
/// untouched files' mtimes stable, so an idle daemon costs three `stat`s per
/// site per tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SiteStamps {
    pages: Option<Stamp>,
    files: Option<Stamp>,
    shell: Option<Stamp>,
}

impl SiteStamps {
    pub(super) fn stat(site_dir: &Path) -> Self {
        Self {
            pages: stamp(&site_dir.join("pages.json")),
            files: stamp(&site_dir.join("files.json")),
            shell: stamp(&site_dir.join("shell")),
        }
    }
}

/// The last-adopted per-site state: the change-detection stamps plus the
/// manifest rows the next diff runs against.
pub(super) struct SiteState {
    pub(super) stamps: SiteStamps,
    pub(super) rows: HashMap<u64, PageRow>,
}

/// The publication worker's owned state: the last-adopted sites map, the
/// per-site stamps + rows, and the materialised-body store — the persistent
/// half of every [`RepoSnapshot`], filled once per body at first sight
/// (content addressing: a changed body is a new id) and pruned to the live
/// set as the publication moves. The thread runs [`OutWorker::run`],
/// servicing [`OutReq`]s until every [`OutMailbox`] (and thus every
/// snapshot-holding gear) is gone.
pub(super) struct OutWorker {
    pub(super) dir: &'static Path,
    pub(super) built: bool,
    pub(super) sites: ImHashMap<SafePathComponent, WDWebsite>,
    pub(super) states: HashMap<SafePathComponent, SiteState>,
    pub(super) bodies: ImHashMap<BlobId, Arc<str>>,
}

impl OutWorker {
    pub(super) fn new(dir: &'static Path) -> Self {
        Self {
            dir,
            built: false,
            sites: ImHashMap::new(),
            states: HashMap::new(),
            bodies: ImHashMap::new(),
        }
    }

    /// Service [`OutReq`]s until the mailbox closes. Runs on the worker
    /// thread; `rx.recv()` blocks *this* thread, never the async core.
    pub(super) fn run(mut self, rx: flume::Receiver<OutReq>) {
        while let Ok(req) = rx.recv() {
            match req {
                OutReq::Tick { reply } => {
                    let _ = reply.send(self.tick());
                }
                OutReq::Snapshot { reply } => {
                    self.snapshot();
                    let _ = reply.send(self.data());
                }
            }
        }
    }

    /// Rescan the publication and rebuild what drifted. `Some(data)` when the
    /// dataset changed (the caller adopts a fresh snapshot); `None` when
    /// nothing changed (the caller keeps its prior `Rc`). The first tick
    /// always builds. A site whose publication vanished mid-run is simply
    /// absent from the listing — its entry is dropped, and it re-adopts when
    /// it comes back.
    pub(super) fn tick(&mut self) -> Option<RepoSnapshot> {
        let first = !self.built;
        self.built = true;
        let present = site_dirs(self.dir);
        let mut changed = false;
        // Sites whose publication vanished: drop entry + state together.
        let gone: Vec<_> = self
            .sites
            .keys()
            .filter(|s| !present.contains_key(&***s))
            .cloned()
            .collect();
        if !gone.is_empty() {
            for s in gone {
                self.sites.remove(&s);
                self.states.remove(&s);
            }
            changed = true;
        }
        for (site_name, site_dir) in &present {
            let Some(site) = SafePathComponent::new(site_name.clone()) else {
                continue;
            };
            // Stat *before* reading: a write landing mid-read can only cause
            // one redundant rebuild next tick, never a missed change.
            let stamps = SiteStamps::stat(site_dir);
            if !first && self.states.get(&site).is_some_and(|s| s.stamps == stamps) {
                continue;
            }
            if self.adopt_site(&site, site_dir, stamps) {
                changed = true;
            }
        }
        if first || changed {
            retain_latest(&self.sites, &mut self.bodies);
            Some(self.data())
        } else {
            None
        }
    }

    /// Read and adopt one site's publication. Cold site (no prior state):
    /// full build. Warm site: the three files are patched independently,
    /// each only when its stamp drifted. `false` (nothing adopted, prior
    /// state kept) only when a drifted read failed — the atomic tmp+rename
    /// publication makes that a real I/O error, not a torn read.
    fn adopt_site(
        &mut self,
        site: &SafePathComponent,
        site_dir: &Path,
        stamps: SiteStamps,
    ) -> bool {
        let Some(old) = self.states.get(site) else {
            let Some((rows, w)) = build_site(site_dir, &mut self.bodies) else {
                error!(
                    "Failed to read the publication of {}: {}",
                    &**site,
                    site_dir.display()
                );
                return false;
            };
            self.states.insert(site.clone(), SiteState { stamps, rows });
            self.sites.insert(site.clone(), w);
            return true;
        };
        let mut w = self.sites.get(site).cloned().unwrap_or_default();
        let mut rows = old.rows.clone();
        if old.stamps.pages != stamps.pages {
            let Some(fresh) = read_pages_manifest(site_dir) else {
                error!(
                    "Failed to re-read pages.json of {}: {}",
                    &**site,
                    site_dir.display()
                );
                return false;
            };
            patch_pages(&mut w, site_dir, &old.rows, &fresh, &mut self.bodies);
            rows = fresh;
        }
        if old.stamps.files != stamps.files {
            w.files = read_files_index(site_dir).into_iter().collect();
        }
        if old.stamps.shell != stamps.shell {
            let (title, subtitle, theme_root) = read_shell(site_dir);
            w.title = title;
            w.subtitle = subtitle;
            w.theme_root = theme_root;
        }
        self.states.insert(site.clone(), SiteState { stamps, rows });
        self.sites.insert(site.clone(), w);
        true
    }

    /// Build the dataset once (no rescan) if it hasn't been built yet.
    pub(super) fn snapshot(&mut self) {
        if !self.built {
            self.tick();
        }
    }

    /// A fresh [`RepoSnapshot`] — O(1) structural clones of both halves.
    pub(super) fn data(&self) -> RepoSnapshot {
        RepoSnapshot {
            sites: self.sites.clone(),
            bodies: self.bodies.clone(),
        }
    }
}

/// Spawn the dedicated publication worker for `meta` and return its mailbox.
/// The worker services [`OutReq`]s for the gear's life — keeping all
/// synchronous publication work off the async core.
pub(super) fn spawn_worker(meta: &OutMeta) -> OutMailbox {
    let (tx, rx) = flume::unbounded::<OutReq>();
    let mailbox = OutMailbox(tx);
    let dir = meta.dir();
    std::thread::Builder::new()
        .name("kolorinko-out".into())
        .spawn(move || OutWorker::new(dir).run(rx))
        .expect("spawn publication worker thread");
    mailbox
}
