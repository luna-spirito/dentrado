use super::*;

// =========================================================================
// `repo_snap` — the one cross-core bridge
// =========================================================================

/// Trivial lens cache: [`repo_snap`] is an O(1) structural clone of the
/// followed snapshot, recomputed on every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoSnapCache;

/// The `repo` core's snapshot as a cross-core `shared` value: an O(1)
/// clone of the followed `Rc`, so every consumer core holds the whole
/// corpus by reference and reads pages, slugs, selections, and bodies
/// locally — the single bridge that replaces the whole `repo_l_*` lens
/// family (one cross-core read per consuming gear *run*, none per hop).
pub(crate) fn repo_snap(data: &RepoSnapshot) -> RepoSnapshot {
    data.clone()
}

// =========================================================================
// `shell` gear
// =========================================================================

/// Per-instance cache for [`shell`]: a pure aggregation re-derived each run
/// from its `article_latest` dependencies and the live [`RepoSnapshot`], so it
/// carries no state between runs.
#[derive(Default, Clone, Debug)]
pub(crate) struct ShellCache;

/// The space's whole chrome in one shot: the fully include-resolved `nav:top`
/// and `nav:side` pages, the theme-root URLs, and the landing page's
/// canonical address (`root`). Keyed on the canonical
/// `space` (the URL/subscription identity); the dataset site is resolved
/// through the global registry, and each nav page is fetched by its
/// `local` id (resolved from the followed [`RepoSnapshot`]) via an
/// [`article_latest`](crate::runtime::article_latest)
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// (so an edit to either re-runs this gear); the theme roots are projected
/// straight out of the followed snapshot. The client fetches the entire site
/// frame under one subscription that survives page navigation within the
/// space.
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    data: &RepoSnapshot,
    space: SpaceId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> SiteShell {
    let Some(site) = crate::globals::site_of(&space) else {
        return SiteShell::default();
    };
    let local_of = |slug: (Option<SafePathComponent>, SafePathComponent)| {
        article(data, site, &slug).and_then(|a| LocalId::from_page_id(&a.meta.page_id))
    };
    let nav = |name: &'static str| local_of(nav_slug(name));
    let nav_top = match nav("top") {
        Some(local) => {
            let view = crate::runtime::article_latest(space, local)
                .secondary_get(ctx)
                .await;
            (*view).clone()
        }
        None => ArticleView::default(),
    };
    let nav_side = match nav("side") {
        Some(local) => {
            let view = crate::runtime::article_latest(space, local)
                .secondary_get(ctx)
                .await;
            (*view).clone()
        }
        None => ArticleView::default(),
    };
    let (title, subtitle, theme_root) = data
        .sites
        .get(site)
        .map(|w| {
            let theme_root = w
                .theme_root
                .as_ref()
                .and_then(|p| w.files.get(p))
                .map(|ca| ca_url(site, ca));
            (w.title.clone(), w.subtitle.clone(), theme_root)
        })
        .unwrap_or_default();
    // The landing page's canonical address — the same resolution the server's
    // bare-root 301 performs (the registry's landing slug through the
    // dataset), title included — so the client's site link skips the
    // round-trip and lands on the same titled route the 301 would.
    let root = crate::globals::reg_of(&space)
        .map(|reg| (None, reg.landing.clone()))
        .and_then(|slug| article(data, site, &slug))
        .and_then(|a| {
            LocalId::from_page_id(&a.meta.page_id).map(|local| (space, local, a.meta.title.clone()))
        });
    SiteShell {
        title,
        subtitle,
        site: Some(site.to_string()),
        root,
        theme_root,
        nav_top,
        nav_side,
    }
}

/// `(nav, name)` slug for one of the per-site navigation pages (`nav:top`,
/// `nav:side`).
pub(super) fn nav_slug(name: &str) -> Slug {
    let category = SafePathComponent::new("nav".to_string()).unwrap();
    let page = SafePathComponent::new(name.to_string()).unwrap();
    (Some(category), page)
}
