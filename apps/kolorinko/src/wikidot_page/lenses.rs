use super::*;

// =========================================================================
// `shell` gear
// =========================================================================

/// The space's whole chrome in one shot: the fully include-resolved `nav:top`
/// and `nav:side` pages, the theme-root URL, and the landing page's canonical
/// address (`root`). Keyed on the canonical `space` (the URL/subscription
/// identity); the dataset site is resolved through the global registry, its
/// metadata comes from the per-site [`repo`] gear, and each nav page is
/// fetched by its `local` id via an
/// [`article_latest`](crate::runtime::article_latest)
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependency (so an edit to either re-runs this gear). The client fetches the
/// entire site frame under one subscription that survives page navigation
/// within the space.
pub(crate) async fn shell<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> SiteShell {
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return SiteShell::default();
    };
    let meta = crate::runtime::repo(site.clone()).secondary_get(ctx).await;
    let local_of = |slug: Slug| {
        article(&meta, &slug).and_then(|page| LocalId::from_page_id(&page.id.to_string()))
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
    let theme_root = meta
        .theme_root
        .as_ref()
        .and_then(|p| resource_url(&meta, &site, p));
    // The landing page's canonical address — the same resolution the server's
    // bare-root 301 performs (the shell's landing slug through the dataset),
    // title included — so the client's site link skips the round-trip and
    // lands on the same titled route the 301 would.
    let root = article(&meta, &meta.landing).and_then(|page| {
        LocalId::from_page_id(&page.id.to_string()).map(|local| (space, local, page.title.clone()))
    });
    SiteShell {
        title: meta.title.clone(),
        subtitle: meta.subtitle.clone(),
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
