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

/// Trivial lens cache: a pure projection of `repo`, recomputed on every
/// `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLLocalIdCache;

/// Trivial lens cache for [`repo_l_query_pages`]: a pure projection of
/// `repo`, recomputed on every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLQueryPagesCache;

/// Per-instance cache for [`shell`]: a pure aggregation re-derived each run
/// from its `article_latest` dependencies and the live [`RepoData`], so it
/// carries no state between runs.
#[derive(Default, Clone, Debug)]
pub(crate) struct ShellCache;

/// Project one page out of `repo`'s dataset into a shippable [`ArticleLatest`],
/// materialising the latest body blob via the worker thread (off-core). The
/// page is addressed canonically: the lens resolves `(space, local)` to the
/// page's current slug through the rename-stable page id
/// ([`super::page_slug`]) — this is the cross-core bridge every off-`repo`
/// gear reads the dataset through. An unknown address (or an unopenable
/// repository) yields an empty [`ArticleLatest`] (blank render).
pub(crate) async fn repo_l_article_latest(
    data: &RepoData,
    space: SpaceId,
    local: LocalId,
) -> ArticleLatest {
    let Some((site, slug)) = super::page_slug(data, space, local) else {
        return ArticleLatest::default();
    };
    let Some(a) = data.article(&site, &slug) else {
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

/// The slug-family → canonical bridge: the `(local id, title)` of the page a
/// legacy `(site, slug)` address names — the id off the rename-stable page
/// id, the title for a redirect's decorative segment. `None` when the site
/// has no such page. Everything slug-addressed that needs the canonical
/// identity (HTTP slug redirects, the `/code/N` endpoint, the render CLI,
/// and the include cone inside [`article_latest`]) crosses here.
pub(crate) fn repo_l_local_id(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<(LocalId, String)> {
    let a = data.article(site, slug)?;
    Some((
        LocalId::from_page_id(&a.meta.page_id)?,
        a.meta.title.clone(),
    ))
}

/// The batched [`repo_l_local_id`]: the `(local id, title)` answer for a
/// whole (sorted, deduplicated) slug set in one positional read — one gear
/// instance and one dependency per page, instead of one per link, which is
/// what keeps thousand-link index pages viable. An unknown site answers
/// `None` for every slug (every link renders `newpage`).
pub(crate) fn repo_l_query_pages(
    data: &RepoData,
    site: &SafePathComponent,
    query: &PageQuery,
) -> PageQueryResult {
    query
        .0
        .iter()
        .map(|slug| repo_l_local_id(data, site, slug))
        .collect()
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
/// and `nav:side` pages, the theme-root URLs, and the landing page's
/// canonical address (`root`). Keyed on the canonical
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
    let local_of = |slug: (Option<SafePathComponent>, SafePathComponent)| {
        data.article(site, &slug)
            .and_then(|a| LocalId::from_page_id(&a.meta.page_id))
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
        .and_then(|slug| data.article(site, &slug))
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
