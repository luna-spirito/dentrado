use super::*;

// =========================================================================
// Git worker thread (owns the !Send `Repository`; serves the async gears)
// =========================================================================

/// A request to the git worker thread. Each carries its own one-shot reply
/// channel; the oracle core sends one and `.await`s the reply, so it never
/// blocks while libgit2 works on the worker thread.
pub(super) enum GitReq {
    /// Fetch + rebuild if the tip moved. The reply is `Some` only when the
    /// dataset actually changed; `None` lets the caller keep its cached `Rc`.
    Tick {
        reply: flume::Sender<Option<RepoData>>,
    },
    /// The current dataset without pulling (cold start / first non-tick run).
    Snapshot { reply: flume::Sender<RepoData> },
    /// Read one body blob out of the object database (frontmatter stripped).
    Blob {
        oid: Oid,
        reply: flume::Sender<Option<String>>,
    },
}

/// Cloneable, `Send` handle to the git worker thread, embedded in [`RepoData`]
/// so the [`repo_l_article_latest`] lens — co-located on the oracle's core —
/// can materialise body blobs off-core. Cheap to clone (one `Arc` bump inside
/// the `flume` sender).
#[derive(Clone)]
pub(crate) struct GitMailbox(flume::Sender<GitReq>);

impl GitMailbox {
    /// Fetch + rebuild if the tip moved; `Some` only when the dataset changed.
    pub(super) async fn tick(&self) -> Option<RepoData> {
        let (tx, rx) = flume::bounded(1);
        if self.0.send_async(GitReq::Tick { reply: tx }).await.is_err() {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }

    /// Current dataset without pulling.
    pub(super) async fn snapshot(&self) -> RepoData {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(GitReq::Snapshot { reply: tx })
            .await
            .is_err()
        {
            return RepoData::empty(self.clone());
        }
        rx.recv_async()
            .await
            .unwrap_or_else(|_| RepoData::empty(self.clone()))
    }

    /// Read one body blob off the worker thread. `None` if missing/bad UTF-8.
    pub(super) async fn blob(&self, oid: Oid) -> Option<String> {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(GitReq::Blob { oid, reply: tx })
            .await
            .is_err()
        {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }
}

/// The git worker thread's owned state. Everything `!Send` or git-bound lives
/// here: the [`Repository`] (created *on* this thread and never moved), the
/// reverse [`Index`], the last tip, and the current sites snapshot. The thread
/// runs [`GitWorker::run`], servicing [`GitReq`]s until every [`GitMailbox`]
/// (and thus every [`RepoData`]) is gone. Body blobs are read **uncached** —
/// git odb lookups are cheap (content-addressed) and run only on the oracle's
/// timer tick; the expensive work (parsing) is dedup'd downstream by
/// [`ParsedCache`](crate::wikidot_page::ParsedCache), so a worker-side blob
/// cache would only duplicate that for no gain.
pub(super) struct GitWorker {
    pub(super) repo: Option<Repository>,
    pub(super) url: &'static str,
    pub(super) root: &'static Path,
    pub(super) mailbox: GitMailbox,
    pub(super) last_tip: Option<Oid>,
    pub(super) sites: ImHashMap<SafePathComponent, WDWebsite>,
    pub(super) index: Index,
}

impl GitWorker {
    pub(super) fn new(url: &'static str, root: &'static Path, mailbox: GitMailbox) -> Self {
        Self {
            repo: None,
            url,
            root,
            mailbox,
            last_tip: None,
            sites: ImHashMap::new(),
            index: HashMap::new(),
        }
    }

    /// Service [`GitReq`]s until the mailbox closes. Runs on the worker thread;
    /// `rx.recv()` blocks *this* thread, never the async core.
    pub(super) fn run(mut self, rx: flume::Receiver<GitReq>) {
        while let Ok(req) = rx.recv() {
            match req {
                GitReq::Tick { reply } => {
                    let _ = reply.send(self.tick());
                }
                GitReq::Snapshot { reply } => {
                    self.snapshot();
                    let _ = reply.send(self.data());
                }
                GitReq::Blob { oid, reply } => {
                    let _ = reply.send(self.blob(oid));
                }
            }
        }
    }

    /// Fetch + rebuild if the tip moved. `Some(data)` when the dataset changed
    /// (the caller adopts a fresh snapshot); `None` when nothing changed (the
    /// caller keeps its prior `Rc`). The first tick always pulls + builds.
    pub(super) fn tick(&mut self) -> Option<RepoData> {
        self.ensure_repo();
        let repo = self.repo.as_ref()?;
        let outcome = pull_for_diff(repo);
        if self.last_tip.is_none() {
            let tip = current_tip(repo);
            if let Some(t) = tip {
                let (sites, index) = build_from_tree(repo, t, self.root);
                self.sites = sites;
                self.index = index;
            }
            self.last_tip = tip;
            return Some(self.data());
        }
        let new_tip = match outcome {
            Some(PullOutcome::SameTip) => return None,
            Some(PullOutcome::Updated { new_tip }) => new_tip,
            None => return None,
        };
        let old_tip = self.last_tip.expect("Some on the non-first path above");
        // `files/` touches force a full rebuild (the symlink→hash index lives
        // in `build_from_tree`); a pure-page change takes the incremental path.
        let rebuilt: Option<(ImHashMap<SafePathComponent, WDWebsite>, Index)> =
            match diff_changes(repo, old_tip, new_tip, self.root) {
                Some((affected, false)) if affected.is_empty() => None,
                Some((affected, false)) => {
                    let mut index = self.index.clone();
                    let new_tree = repo
                        .find_commit(new_tip)
                        .ok()
                        .and_then(|c| repo.find_tree(c.tree_id()).ok());
                    match new_tree.as_ref() {
                        Some(tree) => {
                            let sites = incremental_update(
                                repo,
                                tree,
                                self.root,
                                &self.sites,
                                &mut index,
                                affected,
                            );
                            Some((sites, index))
                        }
                        None => Some(build_from_tree(repo, new_tip, self.root)),
                    }
                }
                _ => Some(build_from_tree(repo, new_tip, self.root)),
            };
        self.last_tip = Some(new_tip);
        if let Some((sites, index)) = rebuilt {
            self.sites = sites;
            self.index = index;
            Some(self.data())
        } else {
            None
        }
    }

    /// Build the dataset once (no pull) if it hasn't been built yet.
    pub(super) fn snapshot(&mut self) {
        self.ensure_repo();
        if self.last_tip.is_none() {
            self.build();
        }
    }

    pub(super) fn build(&mut self) {
        let Some(repo) = self.repo.as_ref() else {
            return;
        };
        let tip = current_tip(repo);
        if let Some(t) = tip {
            let (sites, index) = build_from_tree(repo, t, self.root);
            self.sites = sites;
            self.index = index;
        }
        self.last_tip = tip;
    }

    /// Lazily open/clone the `Repository` on this thread, retrying on each tick
    /// until it succeeds (a transient clone failure shouldn't doom the gear).
    pub(super) fn ensure_repo(&mut self) {
        if self.repo.is_none() {
            self.repo = open_or_clone(self.url, self.root);
        }
    }

    /// A fresh [`RepoData`] snapshot (structurally shared `sites` + our mailbox).
    pub(super) fn data(&self) -> RepoData {
        RepoData {
            sites: self.sites.clone(),
            mailbox: self.mailbox.clone(),
        }
    }

    /// Read body blob `oid` straight from the odb (frontmatter stripped).
    /// Uncached — see [`GitWorker`]: odb lookups are cheap and infrequent, and
    /// the parse layer dedup's the expensive work. `None` if missing/bad UTF-8.
    pub(super) fn blob(&self, oid: Oid) -> Option<String> {
        read_body(self.repo.as_ref()?, oid)
    }
}

/// Spawn the dedicated git worker thread for `meta` and return its mailbox. The
/// worker opens/clones the [`Repository`] *on its own thread* (it is `!Send` and
/// must never cross a thread), then services [`GitReq`]s for the gear's life —
/// keeping all synchronous libgit2 work off the async core.
pub(super) fn spawn_worker(meta: &RepoMeta) -> GitMailbox {
    let (tx, rx) = flume::unbounded::<GitReq>();
    let mailbox = GitMailbox(tx);
    let worker_mailbox = mailbox.clone();
    let url = meta.url();
    let root = meta.path();
    std::thread::Builder::new()
        .name("kolorinko-git".into())
        .spawn(move || GitWorker::new(url, root, worker_mailbox).run(rx))
        .expect("spawn git worker thread");
    mailbox
}
