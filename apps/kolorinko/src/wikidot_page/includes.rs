use super::*;

// =========================================================================
// `[[include]]` resolution — target collection, splicing, dependency tree
// =========================================================================

/// Resolve an include's [`PageRef`] to `(site, slug)` on the current site.
/// The parser parks the first `:`-segment of the source in [`PageRef::space`];
/// for same-site page refs that segment is the category, so `space` → category
/// and the trailing path → name. Cross-site includes (`space` = another site)
/// are not yet supported. Unresolvable targets (bad path component) return
/// `None` and the directive is left in place.
fn include_target(
    src: &PageRef,
    current_site: &SafePathComponent,
) -> Option<(SafePathComponent, Slug)> {
    let name = SafePathComponent::new(src.path.last()?.clone())?;
    let category = match &src.space {
        Some(cat) => Some(SafePathComponent::new(cat.clone())?),
        None => None,
    };
    Some((current_site.clone(), (category, name)))
}

/// Walk `content` and record every `[[include]]` target not already in
/// `visited` (and not already batched in `out`), so [`resolve`](super::article_latest::resolve)
/// can fetch the whole batch at once. Sync recursion over the tree — no awaits.
pub(super) fn collect_include_targets(
    content: &Content,
    current_site: &SafePathComponent,
    visited: &HashSet<Key>,
    out: &mut Vec<(Key, SafePathComponent, Slug)>,
) {
    for node in content {
        match node {
            Node::Include(inc) => {
                if let Some((inc_site, inc_slug)) = include_target(&inc.source, current_site)
                    && let key = (inc_site.clone(), inc_slug.0.clone(), inc_slug.1.clone())
                    && !visited.contains(&key)
                    && !out.iter().any(|(k, _, _)| *k == key)
                {
                    out.push((key, inc_site, inc_slug));
                }
            }
            other => {
                other.visit_node(&mut |c| collect_include_targets(c, current_site, visited, out));
            }
        }
    }
}

/// Return `content` with every `[[include]]` whose target is among `fetched`
/// (already resolved) replaced by that target's content (spliced inline);
/// directives that couldn't be resolved (unknown target, or a data cycle) are
/// left verbatim.
pub(super) fn substitute_includes(
    content: Content,
    current_site: &SafePathComponent,
    fetched: &HashMap<Key, Content>,
) -> Content {
    let mut walk = |c: Content| substitute_includes(c, current_site, fetched);
    let mut out: Content = Vec::with_capacity(content.len());
    for node in content {
        match node {
            Node::Include(inc) => {
                let resolved = include_target(&inc.source, current_site)
                    .and_then(|(s, slug)| fetched.get(&(s, slug.0, slug.1)).map(Content::as_slice));
                match resolved {
                    Some(nodes) => out.extend(apply_include_vars(nodes.to_vec(), &inc.vars)),
                    None => out.push(Node::Include(inc)),
                }
            }
            other => out.push(other.map_node(&mut walk)),
        }
    }
    out
}

/// Fold the discovery-order `(includer, included)` edges into the page's
/// dependency tree: one [`PageDep`] per fetched page, nested under the page
/// whose body first included it; `root`'s direct includes form the top level.
pub(super) fn dep_tree(root: &Key, edges: Vec<(Key, Key)>) -> Vec<PageDep> {
    let mut children: HashMap<Key, Vec<Key>> = HashMap::new();
    for (includer, included) in edges {
        children.entry(includer).or_default().push(included);
    }
    dep_children(&children, children.get(root).map_or(&[], Vec::as_slice))
}

fn dep_children(children: &HashMap<Key, Vec<Key>>, keys: &[Key]) -> Vec<PageDep> {
    keys.iter()
        .map(|key| {
            page_dep(
                key,
                dep_children(children, children.get(key).map_or(&[], Vec::as_slice)),
            )
        })
        .collect()
}

/// One fetched page as a [`PageDep`]: its `(site, category, page)` address,
/// with its nested deps.
pub(super) fn page_dep(key: &Key, deps: Vec<PageDep>) -> PageDep {
    PageDep {
        site: (*key.0).clone(),
        category: key.1.as_ref().map(|c| (**c).clone()),
        page: (*key.2).clone(),
        deps,
    }
}
