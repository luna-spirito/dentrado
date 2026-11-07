use super::*;

// =========================================================================
// `repo_snap` — the one cross-core bridge
// =========================================================================

/// The `repo` core's snapshot as a cross-core `shared` value: an O(1)
/// clone of the followed `Rc`, so every consumer core holds the whole
/// corpus by reference and reads pages, slugs, selections, and bodies
/// locally — the single bridge that replaces the whole `repo_l_*` lens
/// family (one cross-core read per consuming gear *run*, none per hop).
pub(crate) fn repo_snap(state: &RepoState) -> RepoSnapshot {
    state.snapshot().clone()
}

// =========================================================================
// `shell` gear
// =========================================================================

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
pub(crate) async fn shell<S: IsStorage<KolorinkoRT>>(
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
            // Through the dataset resolver, not a bare index get: the shell
            // names the theme as a raw URL whose canonical key may differ
            // from the index's (a custom-domain spelling).
            let theme_root = w
                .theme_root
                .as_ref()
                .and_then(|p| resource_url(data, site, p));
            (w.title.clone(), w.subtitle.clone(), theme_root)
        })
        .unwrap_or_default();
    // The landing page's canonical address — the same resolution the server's
    // bare-root 301 performs (the shell's landing slug through the dataset),
    // title included — so the client's site link skips the round-trip and
    // lands on the same titled route the 301 would.
    let root = data
        .sites
        .get(site)
        .and_then(|w| article(data, site, &w.landing))
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
