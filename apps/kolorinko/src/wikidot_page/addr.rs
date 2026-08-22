use super::*;

// =========================================================================
// Canonical addressing gears — `page_addr` / `legacy_page_id`
// =========================================================================

/// No carry-over state: [`page_addr`] is a pure lookup in the followed
/// [`RepoData`] snapshot (space registry + the site's `by_page_id` index).
#[derive(Default, Clone, Debug)]
pub(crate) struct PageAddrCache;

/// No carry-over state: [`legacy_page_id`] is a pure lookup in the followed
/// [`RepoData`] snapshot (slug-keyed article → its `page_id`).
#[derive(Default, Clone, Debug)]
pub(crate) struct LegacyPageIdCache;

/// Resolve a canonical route `(space, local)` to the dataset address that
/// serves it: the registered site for the space, plus the page's current slug
/// from its (rename-stable) wikidot page id. `None` when the space is not
/// registered in the global config, or the site has no page with that id.
pub(crate) fn page_addr(data: &RepoData, space: SpaceId, local: LocalId) -> Option<PageAddr> {
    let site = crate::globals::site_of(&space)?;
    let slug = data
        .sites
        .get(site)?
        .by_page_id
        .get(&local.as_u64())?
        .clone();
    Some(PageAddr {
        site: site.clone(),
        slug,
    })
}

/// Resolve a legacy `/site/cat/name` address to the canonical local id of the
/// page it names — the redirect target for alias paths. `None` when the site
/// has no such page (or its page id, oddly, isn't numeric).
pub(crate) fn legacy_page_id(
    data: &RepoData,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<LocalId> {
    let article = data.article(site, slug)?;
    LocalId::from_page_id(&article.meta.page_id)
}
