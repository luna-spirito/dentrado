use super::*;

// =========================================================================
// `repo` metadata gear
// =========================================================================

/// Decode one site's current publication generation. A timer tick compares
/// `HEAD` against the previous output and returns it unchanged when the
/// generation did not move; a non-tick run (the first activation can be
/// either) serves the previous output unchanged. Revision frontmatter for
/// unchanged archive hashes is reused from the previous output, so an idle
/// tick only reads the small `HEAD` file.
pub(crate) async fn repo<S: IsStorage<KolorinkoRT>>(
    site: SafePathComponent,
    tick: bool,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _view: &mut S::View<'_>,
) -> RepoMeta {
    let previous = match ctx.previous_output() {
        Some(GearResult::Shared(shared)) => match &*shared {
            GearOutShared::RepoOut(meta) => Some(meta.clone()),
            _ => None,
        },
        _ => None,
    };
    if !tick && let Some(previous) = &previous {
        return previous.clone();
    }
    let site_dir = crate::globals::repo().site_dir(&site);
    if tick
        && let Some(previous) = &previous
        && let Ok(head) = read_head(&site_dir).await
        && head.as_deref() == previous.head.as_deref()
    {
        return previous.clone();
    }
    repo_step(&site, &site_dir, previous.as_ref()).await
}

/// [`repo`] without the process-local context — the reader over one site's
/// publication directory, split out so it is testable without a full core.
pub(crate) async fn repo_step(
    site: &SafePathComponent,
    site_dir: &Path,
    previous: Option<&RepoMeta>,
) -> RepoMeta {
    read_repo_meta(site, site_dir, previous)
        .await
        .or_else(|| previous.cloned())
        .unwrap_or_default()
}
