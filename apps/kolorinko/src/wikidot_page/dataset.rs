use super::assets_gear::repo_alias;
use super::*;

// =========================================================================
// Dataset projections (the snapshot's data live in `kolorinko_rt`)
// =========================================================================

/// `(site, Option<category>, name)` — the full address of a page within the
/// dataset. Used both as the include-resolution visited key and as the
/// incremental-update reverse-index value (`_meta` path → its nested-map key).
pub(super) type Key = (
    SafePathComponent,
    Option<SafePathComponent>,
    SafePathComponent,
);

/// One page's latest projection, borrowed straight out of a [`RepoSnapshot`].
pub(crate) struct PageLatest<'a> {
    pub(crate) meta: &'a ArticleMeta,
    pub(crate) revisions: &'a [RevMeta],
    pub(crate) body: &'a Arc<str>,
}

/// Look up one page by `(site, slug)` — the nested-map core of every other
/// projection below.
pub(crate) fn article<'a>(
    snap: &'a RepoSnapshot,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<&'a Article> {
    find_article(&snap.sites, site, slug)
}

/// Project one page out of the snapshot by canonical address — the body of
/// the old `repo_l_article_latest` lens, now a local read. `None` when the
/// space is unregistered, the address names no page, or the body failed to
/// materialise (the old failed-RPC blank-page convention).
pub(crate) fn latest<'a>(
    snap: &'a RepoSnapshot,
    space: SpaceId,
    local: LocalId,
) -> Option<PageLatest<'a>> {
    let (site, slug) = super::page_slug(snap, space, local)?;
    let a = article(snap, &site, &slug)?;
    Some(PageLatest {
        meta: &a.meta,
        revisions: &a.revisions,
        body: snap.bodies.get(&a.latest_body)?,
    })
}

/// The slug-family → canonical bridge — the body of the old `repo_l_local_id`
/// lens: the `(local id, title)` a legacy `(site, slug)` address names (HTTP
/// slug redirects, the `/code/N` endpoint, the render CLI, and the include
/// cone).
pub(crate) fn local_id(
    snap: &RepoSnapshot,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<(LocalId, String)> {
    let a = article(snap, site, slug)?;
    Some((
        LocalId::from_page_id(&a.meta.page_id)?,
        a.meta.title.clone(),
    ))
}

/// The batched [`local_id`]: a page's whole (sorted, deduplicated) link set
/// answered in one pass.
pub(crate) fn query_pages(
    snap: &RepoSnapshot,
    site: &SafePathComponent,
    query: &PageQuery,
) -> PageQueryResult {
    query
        .0
        .iter()
        .map(|slug| local_id(snap, site, slug))
        .collect()
}

/// Project one ListPages selection over one site — the body of the old
/// `repo_l_list_pages` lens. An unknown site yields an empty selection.
pub(crate) fn list_pages(
    snap: &RepoSnapshot,
    site: &SafePathComponent,
    query: &ListPagesQuery,
) -> ListPagesResult {
    match snap.sites.get(site) {
        Some(w) => select(w, &query.0),
        None => ListPagesResult {
            pages: Vec::new(),
            total: 0,
        },
    }
}

/// Resolve one `files/<host>/<path>` attachment to its content-addressed
/// [`CaRef`] — the body of the old `repo_resource` gear. Three lookups, in
/// order: the tail as named (absolute `files.json` rows keep their real
/// external host); the bare site-relative key when the host is one of this
/// site's own (the publication keys on-site files without a host — the DB's
/// `local--…` path form; see [`own_file_host`]); and the CDN/alias-domain
/// retry ([`repo_alias`]). `None` when the URL is not mirrored (a hotlink).
pub(crate) fn resource(
    snap: &RepoSnapshot,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<CaRef> {
    let files = &snap.sites.get(site)?.files;
    files
        .get(path)
        .cloned()
        .or_else(|| {
            let (host, rel) = path.as_str().split_once('/')?;
            if !own_file_host(site, host) {
                return None;
            }
            files.get(&RepoAssetPath::new(rel.to_owned())?).cloned()
        })
        .or_else(|| {
            crate::globals::space_of(site)
                .and_then(|space| crate::globals::domains_of(&space))
                .and_then(|domains| repo_alias(site, domains, path))
                .and_then(|alt| files.get(&alt).cloned())
        })
}

/// Does `host` name this site's own file space — one of the hosts a same-site
/// file can be fetched or linked under? The canonical Wikidot pair plus each
/// configured alias domain, bare and `files.`-prefixed (hosts are DNS names:
/// compared case-insensitively). Decides when a missed `host/path` lookup
/// retries the bare site-relative key ([`resource`]).
fn own_file_host(site: &SafePathComponent, host: &str) -> bool {
    let s: &str = site;
    host.eq_ignore_ascii_case(format!("{s}.wikidot.com").as_str())
        || host.eq_ignore_ascii_case(format!("{s}.wdfiles.com").as_str())
        || crate::globals::domains_of_site(site).iter().any(|d| {
            d.eq_ignore_ascii_case(host) || format!("files.{d}").eq_ignore_ascii_case(host)
        })
}

/// The nested-map lookup underlying [`article`], factored out so the
/// build/incremental tests can resolve a page from a bare sites map.
pub(super) fn find_article<'a>(
    sites: &'a ImHashMap<SafePathComponent, WDWebsite>,
    site: &SafePathComponent,
    slug: &Slug,
) -> Option<&'a Article> {
    sites.get(site)?.articles.get(&slug.0)?.get(&slug.1)
}

/// Drop materialised bodies no longer referenced as any page's latest —
/// bounds [`RepoSnapshot::bodies`] to the corpus's live set however long the
/// process runs and however the tip moves (a force-push re-mirror included).
pub(super) fn retain_latest(
    sites: &ImHashMap<SafePathComponent, WDWebsite>,
    bodies: &mut ImHashMap<BlobId, Arc<str>>,
) {
    let live: HashSet<BlobId> = sites
        .values()
        .flat_map(|w| w.articles.values())
        .flat_map(|by_name| by_name.values())
        .map(|a| a.latest_body)
        .collect();
    bodies.retain(|oid, _| live.contains(oid));
}
