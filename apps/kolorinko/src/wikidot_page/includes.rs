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

/// Walk `content` and record every `[[include]]` target not already fetched
/// (in `raws`) and not already batched in `out`, so [`resolve_include`] can
/// fetch the whole cone at once. Sync recursion over the tree — no awaits.
pub(super) fn collect_include_targets(
    content: &Content,
    current_site: &SafePathComponent,
    raws: &HashMap<Key, Content>,
    out: &mut Vec<(Key, Slug)>,
) {
    for node in content {
        match node {
            Node::Include(inc) => {
                if let Some((inc_site, inc_slug)) = include_target(&inc.source, current_site)
                    && let key = (inc_site.clone(), inc_slug.0.clone(), inc_slug.1.clone())
                    && !raws.contains_key(&key)
                    && !out.iter().any(|(k, _)| *k == key)
                {
                    out.push((key, inc_slug));
                }
            }
            other => {
                other.visit_node(&mut |c| collect_include_targets(c, current_site, raws, out));
            }
        }
    }
}

/// Assemble the body of the page at the head of `path` in one recursive
/// pass: every `[[include]]` whose target's body is in `raws` and is not
/// already on the recursion `path` (a data-level cycle) is replaced by that
/// body with the directive's vars applied — which resolves the values of
/// directives nested inside it too, so vars cascade top-down through the
/// chain — and the spliced body is assembled in turn with the target pushed
/// onto the path. One closing pass then erases what could not resolve (a
/// back-edge directive, a `{$var}` outside any include — in bare text or an
/// attribute value) to its defaults.
pub(super) fn substitute_includes(
    content: Content,
    current_site: &SafePathComponent,
    raws: &HashMap<Key, Content>,
    path: &[Key],
) -> Content {
    apply_include_vars(assemble_includes(content, current_site, raws, path), &[])
}

/// The structural half: splice each include's body (recursing with the
/// target on the path); directives that cannot be spliced — a cycle, or a
/// target with no fetched body — stay verbatim.
fn assemble_includes(
    content: Content,
    current_site: &SafePathComponent,
    raws: &HashMap<Key, Content>,
    path: &[Key],
) -> Content {
    let mut walk = |c: Content| assemble_includes(c, current_site, raws, path);
    let mut out: Content = Vec::with_capacity(content.len());
    for node in content {
        match node {
            Node::Include(inc) => {
                let key = include_target(&inc.source, current_site)
                    .map(|(site, slug)| (site, slug.0, slug.1));
                if let Some(k) = key.as_ref().filter(|k| !path.contains(k))
                    && let Some(raw) = raws.get(k)
                {
                    let spliced = apply_include_vars(raw.to_vec(), &inc.vars);
                    let mut deeper = path.to_vec();
                    deeper.push(k.clone());
                    out.extend(assemble_includes(spliced, current_site, raws, &deeper));
                } else {
                    out.push(Node::Include(inc));
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
