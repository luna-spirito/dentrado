use super::*;
use crate::repo_state::RepoState;

// =========================================================================
// `repo` oracle gear
// =========================================================================

/// The gear-view key of the persisted worker image.
const STATE_KEY: &[u8] = b"repo";

/// Run the [`repo`] oracle. All filesystem work is off-loaded to the worker
/// thread (see [`spawn_worker`]): on a tick it stats the per-site publication
/// files and rebuilds only what drifted (a cold site in full, a warm site
/// per-file — page-level via the `pages.json` row diff); a non-tick run just
/// serves the cached snapshot. The worker returns the prior dataset unchanged
/// when nothing drifted, so dependents aren't re-run for nothing.
///
/// The worker's data survives a restart through the gear view: a cold start
/// restores the last committed [`PersistedWorker`] image into the fresh
/// worker, which then only re-stats the publication (and, when it drifted,
/// re-reads the changed files) instead of rebuilding the corpus.
pub(crate) async fn repo<S: IsStorage<KolorinkoRT>>(
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
    tick: bool,
) -> RepoState {
    let previous = match ctx.previous_output() {
        Some(GearResult::Local(GearOutLocal::RepoOut(state))) => Some(state),
        _ => None,
    };
    repo_step(crate::globals::repo(), previous, view, tick).await
}

/// [`repo`] without the process-local context — the state machine over one
/// view, split out so it is testable without a full core.
pub(crate) async fn repo_step<V: GearView>(
    meta: &OutMeta,
    previous: Option<RepoState>,
    view: &mut V,
    tick: bool,
) -> RepoState {
    // A non-tick run with a snapshot in hand never touches the worker.
    if !tick && let Some(state) = &previous {
        return state.clone();
    }
    let (mailbox, previous_data) = match previous {
        Some(state) => (state.mailbox, Some(state.data)),
        None => {
            let image = chunked::load(view, STATE_KEY).await;
            (spawn_worker(meta, image), None)
        }
    };
    let reply = if tick {
        mailbox.tick().await
    } else {
        mailbox.snapshot().await
    };
    if let Some(image) = &reply.image {
        chunked::store(view, STATE_KEY, image).await;
    }
    // An unchanged publication keeps the prior `Rc`, so `repo_snap` sees no
    // output change and dependents aren't re-run. A cold start (restored
    // state, no prior `Rc`) adopts the worker's snapshot.
    let data = match (reply.changed, previous_data) {
        (false, Some(prev)) => prev,
        _ => Rc::new(reply.data),
    };
    RepoState { mailbox, data }
}
