use super::*;

// =========================================================================
// Internal-link resolution (slug refs → canonical `LinkTarget::Canonical`)
// =========================================================================

/// A link's [`PageRef`] to its dataset slug — the same convention as
/// `include_target` (includes): the parser parks the first
/// `:`-segment in [`PageRef::space`], which for a same-site ref is the
/// *category*, and the last path segment is the page name. `None` for a
/// site-root ref (empty path) or a `/`-bearing segment (a multi-segment
/// route, not a page name) — those stay [`LinkTarget::Page`] (uncolored).
/// A ref that *does* classify but misses the lookup becomes
/// [`LinkTarget::Missing`] (Wikidot's red link) — including cross-site refs
/// (`wikidot:…`), which aren't mirrored here so the page genuinely doesn't
/// exist on this site.
fn link_slug(p: &PageRef) -> Option<Slug> {
    Some((
        p.space
            .as_ref()
            .and_then(|c| SafePathComponent::new(c.clone())),
        SafePathComponent::new(p.path.last()?.clone())?,
    ))
}

/// The id-canonical order of a slug set: sort by the (category, name) string
/// pair — the value's full projection, so sorting by it and deduplicating
/// neighbours is exactly sort+dedup of the set.
fn slug_key(s: &Slug) -> (String, String) {
    (
        s.0.as_ref().map_or_default(|c| (**c).clone()),
        (*s.1).clone(),
    )
}

/// Resolve every internal link in `content` (the whole include/listpages/
/// iftags-expanded tree) against the local snapshot
/// ([`RepoSnapshot::query_pages`] — the page's whole link set in one
/// pass): found targets become [`LinkTarget::Canonical`] (the renderer
/// builds the titled canonical route from them — what the client router
/// intercepts, keeping navigation in the app), classified misses become
/// [`LinkTarget::Missing`] (the renderer's red `newpage` link), and
/// unclassifiable refs (site root, multi-segment routes) stay
/// [`LinkTarget::Page`] untouched.
pub(super) fn resolve_links(
    content: Content,
    site: &SafePathComponent,
    snap: &RepoSnapshot,
) -> Content {
    let mut slugs: Vec<Slug> = Vec::new();
    collect_page_refs(&content, &mut slugs);
    if slugs.is_empty() {
        return content;
    }
    let query = canonical_query(slugs);
    let out = query_pages(snap, site, &query);
    let resolved: HashMap<Slug, (LocalId, String)> = query
        .0
        .iter()
        .zip(out.iter())
        .filter_map(|(slug, hit)| {
            let (local, title) = hit.as_ref()?;
            Some((slug.clone(), (*local, title.clone())))
        })
        .collect();
    substitute_links(content, &resolved)
}

/// Rewrite absolute URLs addressing a *page on this very site* —
/// `http://rpcauthority.wikidot.com/rpc-002`, `www.rpc-wiki.net/contest:x` —
/// into [`LinkTarget::Page`] refs, so [`resolve_links`] and the renderer give
/// them the same canonical internal treatment a `[[[rpc-002]]]` gets (the
/// mirror's link-rewriting divergence). A URL with a query, a fragment, a
/// multi-segment path (`forum/t-…`, `local--files/…` — attachments are the
/// resource pass's business, and forums are not mirrored), or a foreign host
/// passes through unchanged; the bare site root becomes the root page ref.
pub(super) fn rewrite_own_page_links(content: Content, site: &SafePathComponent) -> Content {
    let mut walk = |c: Content| rewrite_own_page_links(c, site);
    content
        .into_iter()
        .map(|node| match node {
            Node::Link {
                target,
                text,
                class,
                new_tab,
            } => Node::Link {
                new_tab,
                class,
                text: walk(text),
                target: match target {
                    LinkTarget::Url(u) => own_page_ref(site, &u)
                        .map(LinkTarget::Page)
                        .unwrap_or(LinkTarget::Url(u)),
                    other => other,
                },
            },
            other => other.map_node(&mut walk),
        })
        .collect()
}

/// The [`PageRef`] an own-host URL addresses, or `None` when it addresses no
/// page (foreign host, query, multi-segment path, unusable slug). The path
/// segment is percent-decoded; `SafePathComponent` rejects anything that is
/// not a single normal path component (a `forum/…` multi-segment path).
fn own_page_ref(site: &SafePathComponent, url: &str) -> Option<PageRef> {
    let tail = http_tail(url, None)?;
    if tail.contains('?') {
        return None;
    }
    let (host, rest) = tail.split_once('/').unwrap_or((&tail, ""));
    if !own_file_host(site, host) {
        return None;
    }
    // The bare site root (or a trailing `/`): the `Page` ref with an empty
    // path renders as the space root.
    if rest.is_empty() {
        return Some(PageRef {
            space: None,
            path: Vec::new(),
        });
    }
    let decoded = percent_decode(rest);
    let (cat, name) = match decoded.split_once(':') {
        Some((cat, name)) if !cat.is_empty() => (Some(cat), name),
        // No (usable) colon: the whole segment is the page name.
        _ => (None, decoded.as_str()),
    };
    // Reject anything that is not a single normal path component (a
    // `forum/…` multi-segment path) up front, before building the ref.
    SafePathComponent::new(name.to_owned())?;
    Some(PageRef {
        space: cat.map(str::to_string),
        path: vec![name.to_string()],
    })
}

/// Canonical id form of a collected slug set: sorted by the (category, name)
/// string pair and deduplicated, so the gear id is a pure function of the
/// *set* of links — an edit that only reshuffles them reuses the instance.
pub(super) fn canonical_query(mut slugs: Vec<Slug>) -> PageQuery {
    slugs.sort_by_key(slug_key);
    slugs.dedup();
    PageQuery(slugs)
}

/// Walk `content` and collect every [`LinkTarget::Page`] target's slug, in
/// first-appearance order.
pub(super) fn collect_page_refs(content: &Content, out: &mut Vec<Slug>) {
    for node in content {
        if let Node::Link {
            target: LinkTarget::Page(p),
            ..
        } = node
            && let Some(slug) = link_slug(p)
            && !out.contains(&slug)
        {
            out.push(slug);
        }
        node.visit_node(&mut |c| collect_page_refs(c, out));
    }
}

/// Rewrite every classifiable [`LinkTarget::Page`]: hits become
/// [`LinkTarget::Canonical`], classified misses [`LinkTarget::Missing`]
/// (red link), and everything unclassifiable (site root, multi-segment
/// routes, URLs, unresolved targets) passes through unchanged.
pub(super) fn substitute_links(
    content: Content,
    resolved: &HashMap<Slug, (LocalId, String)>,
) -> Content {
    let mut walk = |c: Content| substitute_links(c, resolved);
    content
        .into_iter()
        .map(|node| match node {
            Node::Link {
                target,
                text,
                class,
                new_tab,
            } => Node::Link {
                new_tab,
                target: match target {
                    LinkTarget::Page(p) => match &link_slug(&p) {
                        // Classified as a page slug and found: canonical.
                        Some(slug) if let Some((local, title)) = resolved.get(slug) => {
                            LinkTarget::Canonical {
                                page_id: local.page_id().to_string(),
                                title: title.clone(),
                            }
                        }
                        // Classified but absent: the red link.
                        Some(_) => LinkTarget::Missing(p),
                        // Unclassifiable (site root, multi-segment): verbatim.
                        None => LinkTarget::Page(p),
                    },
                    other => other,
                },
                text: walk(text),
                class,
            },
            other => other.map_node(&mut walk),
        })
        .collect()
}
