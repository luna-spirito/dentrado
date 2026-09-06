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

/// Resolve one mirrored attachment — the canonical `host/path` key
/// ([`canon_file_key`]) of an in-article URL or the shell's `theme_root` —
/// to its content-addressed [`CaRef`]. Three lookups, in order: the key as
/// named (the index preserves custom hosts verbatim); the canonical
/// `<site>.wikidot.com` spelling when the host is one of the site's alias
/// domains (the two hosts name one file space, and the publisher collapses
/// only the wikidot spellings); and, for a `local--resized-images/…`
/// variant (which the export never saved), the original `local--files/…`
/// file under the same host. `None` when the URL is not mirrored (a
/// hotlink).
pub(crate) fn resource(
    snap: &RepoSnapshot,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<CaRef> {
    let files = &snap.sites.get(site)?.files;
    keyed(files, site, path).or_else(|| {
        let (host, rest) = path.as_str().split_once("local--resized-images/")?;
        let (orig, _variant) = rest.rsplit_once('/')?;
        keyed(
            files,
            site,
            &RepoAssetPath::new(format!("{host}local--files/{orig}"))?,
        )
    })
}

/// One key against the index: the direct lookup, then the alias retry — a
/// configured custom domain (± `www.` / `files.`, matched by
/// [`same_file_host`]) names the same files `<site>.wikidot.com` does, and
/// the publisher collapses only the wikidot spellings.
fn keyed(
    files: &ImHashMap<RepoAssetPath, CaRef>,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<CaRef> {
    files.get(path).cloned().or_else(|| {
        let (host, rel) = path.as_str().split_once('/')?;
        if !crate::globals::domains_of_site(site)
            .iter()
            .any(|d| same_file_host(d, host))
        {
            return None;
        }
        files
            .get(&RepoAssetPath::new(format!(
                "{}.wikidot.com/{rel}",
                &**site
            ))?)
            .cloned()
    })
}

/// Does `host` name this site — the canonical wikidot pair, or one of the
/// configured alias domains (± the `www.` / `files.` spellings the corpus
/// and the configs mix; hosts are DNS names: compared case-insensitively)?
/// Decides which absolute URLs address this site's own pages
/// ([`own_page_ref`]) and files ([`keyed`]'s retry).
///
/// [`own_page_ref`]: crate::wikidot_page::links
pub(super) fn own_host(site: &SafePathComponent, host: &str) -> bool {
    let s: &str = site;
    host.eq_ignore_ascii_case(format!("{s}.wikidot.com").as_str())
        || host.eq_ignore_ascii_case(format!("{s}.wdfiles.com").as_str())
        || crate::globals::domains_of_site(site)
            .iter()
            .any(|d| same_file_host(d, host))
}

/// Do a configured domain and a URL host name the same file space? A custom
/// domain serves the site's files under itself and its `files.`/`www.`
/// spellings (hosts are DNS names: compared case-insensitively).
fn same_file_host(domain: &str, host: &str) -> bool {
    fn base(s: &str) -> &str {
        let s = s.strip_prefix("files.").unwrap_or(s);
        s.strip_prefix("www.").unwrap_or(s)
    }
    base(domain).eq_ignore_ascii_case(base(host))
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
