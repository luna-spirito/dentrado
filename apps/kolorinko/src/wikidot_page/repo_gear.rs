use super::*;

// =========================================================================
// `repo` metadata gear
// =========================================================================

/// The gear-view key of the persisted generation image. Versioned so a cache
/// written by an older `RepoMeta` layout cold-starts instead of misdecoding.
const META_KEY: &[u8] = b"meta-v2";

/// Load the persisted generation from the gear view; an absent or unreadable
/// image is a cold start (the metadata is recomputable, never authoritative).
pub(super) async fn load_meta<V: GearView>(view: &mut V) -> Option<RepoMeta> {
    let bytes = chunked::load(view, META_KEY).await?;
    match postcard::from_bytes::<RepoMeta>(&bytes) {
        Ok(meta) => Some(meta),
        Err(e) => {
            error!("discarding an unreadable repo cache: {e}");
            None
        }
    }
}

/// Stage the generation as a chunked image in the view.
pub(super) async fn store_meta<V: GearView>(view: &mut V, meta: &RepoMeta) {
    let bytes = postcard::to_stdvec(meta).expect("a repo metadata image serializes");
    chunked::store(view, META_KEY, &bytes).await;
}

/// Decode one site's current publication generation. A timer tick compares
/// `HEAD` against the previous output and returns it unchanged when the
/// generation did not move; a non-tick run (the first activation can be
/// either) serves the previous output unchanged.
///
/// Across process restarts the gear view holds the last generation: a `HEAD`
/// match reuses it without touching any archive. A moved `HEAD` decodes the
/// new generation; the prior one remains as the fallback if that read fails.
pub(crate) async fn repo<S: IsStorage<KolorinkoRT>>(
    site: SafePathComponent,
    tick: bool,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
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
    let head = read_head(&site_dir).await;
    if tick
        && let Some(previous) = &previous
        && let Ok(head) = &head
        && head.as_deref() == previous.head.as_deref()
    {
        return previous.clone();
    }
    let cached = load_meta(view).await;
    if let (Ok(Some(head)), Some(cached)) = (&head, &cached)
        && cached.head.as_deref() == Some(head.as_str())
    {
        return cached.clone();
    }
    let meta = repo_step(&site, &site_dir, previous.as_ref().or(cached.as_ref())).await;
    store_meta(view, &meta).await;
    meta
}

/// [`repo`] without the process-local context — the reader over one site's
/// publication directory, split out so it is testable without a full core.
pub(crate) async fn repo_step(
    site: &SafePathComponent,
    site_dir: &Path,
    fallback: Option<&RepoMeta>,
) -> RepoMeta {
    read_repo_meta(site, site_dir)
        .await
        .or_else(|| fallback.cloned())
        .unwrap_or_default()
}
