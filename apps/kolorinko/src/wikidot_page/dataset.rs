use super::*;

// =========================================================================
// Dataset projections over one site's `RepoMeta`
// =========================================================================

/// `(site, Option<category>, name)` — the full address of a page within the
/// dataset. Used as the include-resolution visited key.
pub(super) type Key = (
    SafePathComponent,
    Option<SafePathComponent>,
    SafePathComponent,
);

/// Look up one page by its slug.
pub(crate) fn article<'a>(meta: &'a RepoMeta, slug: &Slug) -> Option<&'a PageMeta> {
    meta.by_slug.get(slug).and_then(|id| meta.pages.get(id))
}

/// Look up one page by its canonical numeric id.
pub(crate) fn latest<'a>(meta: &'a RepoMeta, local: LocalId) -> Option<&'a PageMeta> {
    meta.pages.get(&local.page_id())
}

/// The slug-family → canonical bridge: the `(local id, title)` a legacy
/// `(site, slug)` address names (HTTP slug redirects, the `/code/N` endpoint,
/// the render CLI, and the include cone).
pub(crate) fn local_id(meta: &RepoMeta, slug: &Slug) -> Option<(LocalId, String)> {
    let page = article(meta, slug)?;
    Some((
        LocalId::from_page_id(&page.id.to_string())?,
        page.title.clone(),
    ))
}

/// The batched [`local_id`]: a page's whole (sorted, deduplicated) link set
/// answered in one pass.
pub(crate) fn query_pages(meta: &RepoMeta, query: &PageQuery) -> PageQueryResult {
    query.0.iter().map(|slug| local_id(meta, slug)).collect()
}

/// Project one ListPages selection over one site.
pub(crate) fn list_pages(meta: &RepoMeta, query: &ListPagesQuery) -> ListPagesResult {
    select(meta, &query.0)
}

/// Resolve one mirrored attachment — the canonical `host/path` key
/// ([`canon_file_key`]) of an in-article URL or the shell's `theme_root` —
/// to its CA hash and reverse-index entry (the recorded `content_type` plus
/// the original URL). Three lookups, in order: the key as named (the index
/// preserves custom hosts verbatim); the canonical `<site>.wikidot.com`
/// spelling when the host is one of the site's alias domains; and, for a
/// `local--resized-images/…` variant (which the export never saved), the
/// original `local--files/…` file under the same host. `None` when the URL is
/// not mirrored (a hotlink).
pub(crate) fn resource<'a>(
    meta: &'a RepoMeta,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<(&'a str, &'a CaFile)> {
    ca_of(meta, site, path).or_else(|| {
        let (host, rest) = path.as_str().split_once("local--resized-images/")?;
        let (orig, _variant) = rest.rsplit_once('/')?;
        ca_of(
            meta,
            site,
            &RepoAssetPath::new(format!("{host}local--files/{orig}"))?,
        )
    })
}

/// The served CA URL of one mirrored attachment — [`resource`] serialized
/// through [`ca_url`]. The one lookup the page pipeline's resource
/// substitution and the served CSS blobs' ref rewriting share.
pub(crate) fn resource_url(
    meta: &RepoMeta,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<String> {
    resource(meta, site, path).map(|(hash, file)| ca_url(site, hash, file))
}

/// The reverse-index entry a forward-index hit names (every forward value
/// keys the reverse map by construction).
fn ca_of<'a>(
    meta: &'a RepoMeta,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<(&'a str, &'a CaFile)> {
    keyed(&meta.files, site, path)
        .and_then(|hash| meta.files_ca.get(hash).map(|file| (hash.as_str(), file)))
}

/// One key against the forward index: the direct lookup, then the alias
/// retry — a configured custom domain (± `www.` / `files.`, matched by
/// [`same_file_host`]) names the same files `<site>.wikidot.com` does.
fn keyed<'a>(
    files: &'a ImHashMap<RepoAssetPath, String>,
    site: &SafePathComponent,
    path: &RepoAssetPath,
) -> Option<&'a String> {
    files.get(path).or_else(|| {
        let (host, rel) = path.as_str().split_once('/')?;
        if !crate::globals::domains_of_site(site)
            .iter()
            .any(|d| same_file_host(d, host))
        {
            return None;
        }
        files.get(&RepoAssetPath::new(format!(
            "{}.wikidot.com/{rel}",
            &**site
        ))?)
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
