use super::*;

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// Reverse index kept by [`GitWorker`]: each `_meta` file path → its nested-map
/// [`Key`]. The path encodes the *physical* location `(site, p1, p2, id)`, but
/// `Key = (site, cat, name)` holds the *logical* location whose `cat`/`name`
/// come from the **slug inside the `_meta` file** — not from the path — so the
/// key is genuinely non-derivable from the path. The index exists so an
/// incremental update can O(1)-remove a page's old entry when its slug changed
/// (rename) or it was deleted; without it you'd have to re-read the old tip's
/// blob or scan the dataset by page id.
pub(super) type Index = HashMap<PathBuf, Key>;

/// Per-instance cache for [`repo`]: the [`GitMailbox`] (the worker is spawned
/// lazily on the first run) and the last adopted [`RepoSnapshot`] — the
/// `Rc` returned on a non-tick run, and the stable pointer kept when a tick
/// found the tip unchanged (so dependents aren't re-run for nothing). Wrapped
/// in `Rc<RefCell<…>>` so the cache (which must be `Clone + Debug`) is a cheap
/// refcount bump.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoOracleState>>);

#[derive(Default)]
pub(super) struct RepoOracleState {
    pub(super) mailbox: Option<GitMailbox>,
    pub(super) data: Option<Rc<RepoSnapshot>>,
}

impl fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the [`repo`] oracle. All git work is off-loaded to the worker thread
/// (see [`spawn_worker`]): on a tick it fetches and incrementally rebuilds
/// only the pages the `old_tip → new_tip` diff touched (falling back to a full
/// [`build_from_tree`] on the first build or when the diff is uncomputable, e.g.
/// a force-push GC'd the old tip); a non-tick run just serves the cached
/// snapshot. The worker returns `None` when the tip didn't move, so the prior
/// `Rc` is handed back unchanged. Each `borrow()` is its own statement — never
/// a `match`/`if let` scrutinee — so no RefCell borrow is held across a
/// re-borrow (the classic scrutinee-temporary foot-gun).
pub(crate) async fn repo(meta: &RepoMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoSnapshot> {
    let mailbox = cache.0.borrow().mailbox.clone();
    let mailbox = match mailbox {
        Some(mb) => mb,
        None => {
            let mb = spawn_worker(meta);
            cache.0.borrow_mut().mailbox = Some(mb.clone());
            mb
        }
    };
    // Non-tick: serve the cached snapshot without pulling.
    if !tick {
        let cached = cache.0.borrow().data.clone();
        if let Some(data) = cached {
            return data;
        }
        // Cold start (first run, before any tick): build once off-core, no pull.
        let rc = Rc::new(mailbox.snapshot().await);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Tick: pull + rebuild-if-changed off-core.
    if let Some(data) = mailbox.tick().await {
        let rc = Rc::new(data);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Unchanged tip: keep the prior snapshot (empty fallback if the worker had
    // no repository to clone on its very first, failed tick).
    cache
        .0
        .borrow()
        .data
        .clone()
        .unwrap_or_else(|| Rc::new(RepoSnapshot::default()))
}

/// The outcome of a `git fetch` + hard-reset: the tip either moved or not.
pub(super) enum PullOutcome {
    SameTip,
    Updated { new_tip: Oid },
}

/// Fetch from `origin` (force-updating local branches) and hard-reset the
/// working tree. Returns the new tip classified against the previous one so the
/// caller can skip a rebuild when nothing changed. `None` on fetch failure
/// (logged); the caller keeps serving the last good dataset.
pub(super) fn pull_for_diff(repo: &Repository) -> Option<PullOutcome> {
    let old_tip = current_tip(repo);
    match try_pull(repo) {
        Ok(new_tip) => Some(if Some(new_tip) == old_tip {
            PullOutcome::SameTip
        } else {
            PullOutcome::Updated { new_tip }
        }),
        Err(e) => {
            error!("Failed to pull the repository: {e}");
            None
        }
    }
}

pub(super) fn current_tip(repo: &Repository) -> Option<Oid> {
    repo.head().ok().and_then(|r| r.target())
}

pub(super) fn try_pull(repo: &Repository) -> Result<Oid, git2::Error> {
    let mut remote = repo.find_remote("origin")?;
    remote.fetch(&["+refs/heads/*:refs/heads/*"], None, None)?;
    let fetched = repo.revparse_single("FETCH_HEAD")?;
    let new_tip = fetched.id();
    repo.reset(&fetched, git2::ResetType::Hard, None)?;
    Ok(new_tip)
}

pub(super) fn open_or_clone(url: &str, path: &Path) -> Option<Repository> {
    match Repository::open(path) {
        Ok(r) => Some(r),
        Err(_) => match Repository::clone(url, path) {
            Ok(r) => Some(r),
            Err(e) => {
                error!("Failed to clone {url}: {e}");
                None
            }
        },
    }
}
