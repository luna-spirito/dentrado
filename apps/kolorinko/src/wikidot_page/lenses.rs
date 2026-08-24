use super::*;

// =========================================================================
// `repo_l_article_latest` lens
// =========================================================================

/// Trivial lens cache: the lens is a pure projection of `repo`, recomputed on
/// every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLArticleCache;

/// Trivial lens cache for [`repo_l_list_pages`]: a pure projection of `repo`,
/// recomputed on every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLListPagesCache;

/// Per-instance cache for [`shell`]: a pure aggregation re-derived each run
/// from its `article_latest` dependencies and the live [`RepoData`], so it
/// carries no state between runs.
#[derive(Default, Clone, Debug)]
pub(crate) struct ShellCache;

/// Project one page out of `repo`'s dataset into a shippable [`ArticleLatest`],
/// materialising the latest body blob via the worker thread (off-core). A
/// missing page (or an unopenable repository) yields an empty [`ArticleLatest`]
/// (blank render).
pub(crate) async fn repo_l_article_latest(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> ArticleLatest {
    let Some(a) = data.article(site, slug) else {
        return ArticleLatest::default();
    };
    let meta = a.meta.clone();
    let revisions = a.revisions.clone();
    match data.mailbox.blob(a.latest_body).await {
        Some(body) => ArticleLatest {
            meta,
            body,
            revisions,
        },
        None => ArticleLatest::default(),
    }
}

/// Project one ListPages selection out of `repo`'s dataset: the site's pages
/// matching the (context-resolved) module parameters, ordered and truncated
/// to the first pagination page. An unknown site yields an empty selection.
pub(crate) fn repo_l_list_pages(
    data: &RepoData,
    site: &SafePathComponent,
    query: &ListPagesQuery,
) -> ListPagesResult {
    match data.sites.get(site) {
        Some(w) => select(w, &query.0),
        None => ListPagesResult {
            pages: Vec::new(),
            total: 0,
        },
    }
}

/// The space's whole chrome in one shot: the fully include-resolved `nav:top`
/// and `nav:side` pages plus the theme-root URLs. Keyed on the canonical
/// `space` (the URL/subscription identity); the dataset site is resolved
/// through the global registry, and each nav page is fetched by its
/// `local` id (resolved from the followed [`RepoData`]) via an
/// [`article_latest`](crate::runtime::article_latest)
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// (so an edit to either re-runs this gear); the theme roots are projected
/// straight out of the followed snapshot. The client fetches the entire site
/// frame under one subscription that survives page navigation within the
/// space.
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    data: &RepoData,
    space: SpaceId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> SiteShell {
    let Some(site) = crate::globals::site_of(&space) else {
        return SiteShell::default();
    };
    let nav = |name: &'static str| {
        let slug = nav_slug(name);
        data.article(site, &slug)
            .and_then(|a| LocalId::from_page_id(&a.meta.page_id))
    };
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
    SiteShell {
        title,
        subtitle,
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
