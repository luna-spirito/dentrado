use super::*;

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// Per-instance cache for [`repo`]: the [`OutMailbox`] (the worker is
/// spawned lazily on the first run) and the last adopted [`RepoSnapshot`] —
/// the `Rc` returned on a non-tick run, and the stable pointer kept when a
/// tick found the publication unchanged (so dependents aren't re-run for
/// nothing). Wrapped in `Rc<RefCell<…>>` so the cache (which must be
/// `Clone + Debug`) is a cheap refcount bump.
#[derive(Default, Clone)]
pub(crate) struct RepoCache(Rc<RefCell<RepoOracleState>>);

#[derive(Default)]
pub(super) struct RepoOracleState {
    pub(super) mailbox: Option<OutMailbox>,
    pub(super) data: Option<Rc<RepoSnapshot>>,
}

impl fmt::Debug for RepoCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RepoCache").finish()
    }
}

/// Run the [`repo`] oracle. All filesystem work is off-loaded to the worker
/// thread (see [`spawn_worker`]): on a tick it stats the per-site publication
/// files and rebuilds only what drifted (a cold site in full, a warm site
/// per-file — page-level via the `pages.json` row diff); a non-tick run just
/// serves the cached snapshot (building once off-core on a cold start). The
/// worker returns `None` when nothing changed, so the prior `Rc` is handed
/// back unchanged. Each `borrow()` is its own statement — never a `match`/
/// `if let` scrutinee — so no RefCell borrow is held across a re-borrow (the
/// classic scrutinee-temporary foot-gun).
pub(crate) async fn repo(meta: &OutMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoSnapshot> {
    let mailbox = cache.0.borrow().mailbox.clone();
    let mailbox = match mailbox {
        Some(mb) => mb,
        None => {
            let mb = spawn_worker(meta);
            cache.0.borrow_mut().mailbox = Some(mb.clone());
            mb
        }
    };
    // Non-tick: serve the cached snapshot without rescanning.
    if !tick {
        let cached = cache.0.borrow().data.clone();
        if let Some(data) = cached {
            return data;
        }
        // Cold start (first run, before any tick): build once off-core, no rescan.
        let rc = Rc::new(mailbox.snapshot().await);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Tick: rescan + rebuild-if-changed off-core.
    if let Some(data) = mailbox.tick().await {
        let rc = Rc::new(data);
        cache.0.borrow_mut().data = Some(Rc::clone(&rc));
        return rc;
    }
    // Unchanged publication: keep the prior snapshot (empty fallback if the
    // worker's very first scan found no publication at all).
    cache
        .0
        .borrow()
        .data
        .clone()
        .unwrap_or_else(|| Rc::new(RepoSnapshot::default()))
}
