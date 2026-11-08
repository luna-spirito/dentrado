use super::*;

// =========================================================================
// `repo` metadata gear
// =========================================================================

/// Decode one site's current publication generation. A timer tick re-reads
/// `HEAD`; a non-tick run (the first activation can be either) serves the
/// previous output unchanged. Revision frontmatter for unchanged archive
/// hashes is reused from the previous output, so an idle tick only stats the
/// `HEAD` file and re-reads the three small section objects.
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
    if !tick && let Some(previous) = previous {
        return previous;
    }
    let site_dir = crate::globals::repo().site_dir(&site);
    repo_step(&site, &site_dir, previous.as_ref())
}

/// [`repo`] without the process-local context — the reader over one site's
/// publication directory, split out so it is testable without a full core.
pub(crate) fn repo_step(
    site: &SafePathComponent,
    site_dir: &Path,
    previous: Option<&RepoMeta>,
) -> RepoMeta {
    read_repo_meta(site, site_dir, previous)
        .or_else(|| previous.cloned())
        .unwrap_or_default()
}
