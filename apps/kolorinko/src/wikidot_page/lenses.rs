use super::*;

// =========================================================================
// `repo_l_article_latest` lens
// =========================================================================

/// Trivial lens cache: the lens is a pure projection of `repo`, recomputed on
/// every `follow` kick.
#[derive(Default, Clone, Debug)]
pub(crate) struct RepoLArticleCache;

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

/// The site's whole chrome in one shot: the fully include-resolved `nav:top`
/// and `nav:side` pages plus the theme-root URLs. Each nav page is declared as
/// an [`article_latest`](crate::runtime::article_latest)
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// (so an edit to either re-runs this gear); the theme roots are projected
/// straight out of the followed [`RepoData`]. Keyed on `site` alone, so the
/// client fetches the entire site frame under one subscription that survives
/// page navigation within the site.
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    data: &RepoData,
    site: SafePathComponent,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> SiteShell {
    let nav_top = crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_slug("top"))
        .secondary_get(ctx)
        .await;
    let nav_side =
        crate::runtime::article_latest(repo_meta.clone(), site.clone(), nav_slug("side"))
            .secondary_get(ctx)
            .await;
    let (title, subtitle, theme_root) = data
        .sites
        .get(&site)
        .map(|w| {
            let theme_root = w
                .theme_root
                .as_ref()
                .and_then(|p| w.files.get(p))
                .map(|ca| ca_url(&site, ca));
            (w.title.clone(), w.subtitle.clone(), theme_root)
        })
        .unwrap_or_default();
    SiteShell {
        title,
        subtitle,
        theme_root,
        nav_top: (*nav_top).clone(),
        nav_side: (*nav_side).clone(),
    }
}

/// `(nav, name)` slug for one of the per-site navigation pages (`nav:top`,
/// `nav:side`).
pub(super) fn nav_slug(name: &str) -> Slug {
    let category = SafePathComponent::new("nav".to_string()).unwrap();
    let page = SafePathComponent::new(name.to_string()).unwrap();
    (Some(category), page)
}
