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
        reply: flume::Sender<Option<RepoSnapshot>>,
    },
    /// The current dataset without pulling (cold start / first non-tick run).
    Snapshot { reply: flume::Sender<RepoSnapshot> },
}

/// Cloneable, `Send` handle to the git worker thread, held by the [`repo`]
/// oracle's cache so it can pull and build off-core (libgit2 is synchronous
/// and `git2::Repository` is `!Send`, so no gear ever touches it directly).
/// Body text never crosses this channel — it is materialised into the
/// snapshot itself (see [`RepoSnapshot`]).
///
/// [`repo`]: crate::wikidot_page::repo
#[derive(Clone)]
pub(crate) struct GitMailbox(flume::Sender<GitReq>);

impl GitMailbox {
    /// Fetch + rebuild if the tip moved; `Some` only when the dataset changed.
    pub(super) async fn tick(&self) -> Option<RepoSnapshot> {
        let (tx, rx) = flume::bounded(1);
        if self.0.send_async(GitReq::Tick { reply: tx }).await.is_err() {
            return None;
        }
        rx.recv_async().await.unwrap_or(None)
    }

    /// Current dataset without pulling.
    pub(super) async fn snapshot(&self) -> RepoSnapshot {
        let (tx, rx) = flume::bounded(1);
        if self
            .0
            .send_async(GitReq::Snapshot { reply: tx })
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

/// The git worker thread's owned state. Everything `!Send` or git-bound
/// lives here: the [`Repository`] (created *on* this thread and never
/// moved), the reverse [`Index`], the last tip, the current sites map, and
/// the materialised-body store — the persistent half of every
/// [`RepoSnapshot`], filled once per body at first sight (content
/// addressing: a changed body is a new oid) and pruned to the live set as
/// the tip moves. The thread runs [`GitWorker::run`], servicing
/// [`GitReq`]s until every [`GitMailbox`] (and thus every snapshot-holding
/// gear) is gone.
pub(super) struct GitWorker {
    pub(super) repo: Option<Repository>,
    pub(super) url: &'static str,
    pub(super) root: &'static Path,
    pub(super) mailbox: GitMailbox,
    pub(super) last_tip: Option<Oid>,
    pub(super) sites: ImHashMap<SafePathComponent, WDWebsite>,
    pub(super) index: Index,
    /// Materialised latest bodies by blob id — see [`GitWorker`].
    pub(super) bodies: ImHashMap<BlobId, Arc<str>>,
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
            bodies: ImHashMap::new(),
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
            }
        }
    }

    /// Fetch + rebuild if the tip moved. `Some(data)` when the dataset changed
    /// (the caller adopts a fresh snapshot); `None` when nothing changed (the
    /// caller keeps its prior `Rc`). The first tick always pulls + builds.
    pub(super) fn tick(&mut self) -> Option<RepoSnapshot> {
        self.ensure_repo();
        let repo = self.repo.as_ref()?;
        let outcome = pull_for_diff(repo);
        if self.last_tip.is_none() {
            let tip = current_tip(repo);
            if let Some(t) = tip {
                let (sites, index) = build_from_tree(repo, t, self.root, &mut self.bodies);
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
                                &mut self.bodies,
                            );
                            Some((sites, index))
                        }
                        None => Some(build_from_tree(repo, new_tip, self.root, &mut self.bodies)),
                    }
                }
                _ => Some(build_from_tree(repo, new_tip, self.root, &mut self.bodies)),
            };
        self.last_tip = Some(new_tip);
        if let Some((sites, index)) = rebuilt {
            self.sites = sites;
            self.index = index;
            retain_latest(&self.sites, &mut self.bodies);
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
            let (sites, index) = build_from_tree(repo, t, self.root, &mut self.bodies);
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

    /// A fresh [`RepoSnapshot`] — O(1) structural clones of both halves.
    pub(super) fn data(&self) -> RepoSnapshot {
        RepoSnapshot {
            sites: self.sites.clone(),
            bodies: self.bodies.clone(),
        }
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
