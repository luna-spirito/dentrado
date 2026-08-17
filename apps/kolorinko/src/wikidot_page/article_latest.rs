use super::*;
use std::collections::VecDeque;

// =========================================================================
// `article_latest_parsed` gear
// =========================================================================

/// Cache for [`article_latest_parsed`]: the last body string and the
/// [`ArticleView`] parsed from it. Because the lens hands back a fresh `String`
/// each kick, an unchanged page is recognised by body equality and its cached
/// parse reused — only a genuinely-changed page is re-parsed.
#[derive(Default, Clone, Debug)]
pub(crate) struct ParsedCache {
    pub(super) body: Option<String>,
    pub(super) view: Option<ArticleView>,
}

/// Parse a page's latest body into an [`ArticleView`] **without** resolving
/// `[[include]]` directives. Depends only on the [`repo_l_article_latest`] lens
/// (never on another parse gear), so the parse layer is acyclic.
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    meta: &RepoMeta,
    site: &SafePathComponent,
    slug: &Slug,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    let latest = crate::runtime::repo_l_article_latest(meta.clone(), site.clone(), slug.clone())
        .secondary_get(ctx)
        .await;
    if cache.body.as_deref() == Some(latest.body.as_str())
        && let Some(view) = &cache.view
    {
        return view.clone();
    }
    let view = ArticleView {
        meta: latest.meta.clone(),
        revisions: latest.revisions.clone(),
        content: parse(&latest.body),
        deps: Vec::new(),
    };
    *cache = ParsedCache {
        body: Some(latest.body.clone()),
        view: Some(view.clone()),
    };
    view
}

// =========================================================================
// `article_latest` gear — include resolution
// =========================================================================

/// No carry-over state: the result is fully re-derived each run from the parse
/// gears it depends on (which the framework re-runs on any change).
#[derive(Default, Clone, Debug)]
pub(crate) struct LatestCache;

/// Render a page's final [`ArticleView`] by resolving every `[[include]]`:
/// each included page's [`article_latest_parsed`] output is fetched and spliced
/// in place of the directive, recursively, with a visited-set to break
/// data-level cycles (A includes B includes A). Declaring each include as a
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// makes the whole result reactive — an edit anywhere in the include cone
/// re-runs this gear. The tree of every page fetched along the way rides
/// along as the view's `deps`.
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    meta: &RepoMeta,
    site: SafePathComponent,
    slug: Slug,
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut LatestCache,
) -> ArticleView {
    let ArticleView {
        meta: page_meta,
        revisions,
        content,
        ..
    } = parsed.clone();
    let root = (site, slug.0, slug.1);
    let (content, deps) = resolve(content, &root, meta, ctx).await;
    let content = resolve_resources(content, &root.0, meta, ctx).await;
    let content = apply_include_vars(content, &[]);
    ArticleView {
        meta: page_meta,
        revisions,
        content,
        deps,
    }
}

/// Resolve every `[[include]]` directive anywhere inside the root page's
/// content, splicing each included page's (recursively resolved) content in
/// place of the directive, and return the resolved content together with the
/// dependency tree — every page fetched, nested under the page whose body
/// included it.
///
/// Fetching is breadth-first: each page's raw body is walked as it arrives,
/// its include targets declared as [`article_latest_parsed`]
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependencies (so the whole result is reactive to edits anywhere in the
/// transitive include cone) and fetched, and the `(includer, target)` edge
/// recorded; a `visited` set breaks data-level cycles (A includes B includes
/// A). Assembly then runs in passes over the pre-fetched bodies — each pass
/// substitutes one level of directives, so include vars cascade top-down
/// (an includer's vars resolve a nested directive's `{$passthrough}` values
/// before that directive itself is spliced) — until a pass finds nothing new.
pub(super) async fn resolve<S: Storage<KolorinkoRT>>(
    content: Content,
    root: &Key,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    let mut visited: HashSet<Key> = HashSet::from([root.clone()]);
    let mut queue: VecDeque<(Key, Content)> = VecDeque::from([(root.clone(), content)]);
    let mut raws: HashMap<Key, Content> = HashMap::new();
    let mut edges: Vec<(Key, Key)> = Vec::new();
    while let Some((origin, body)) = queue.pop_front() {
        let mut targets: Vec<(Key, SafePathComponent, Slug)> = Vec::new();
        collect_include_targets(&body, &root.0, &visited, &mut targets);
        for (key, inc_site, inc_slug) in targets {
            visited.insert(key.clone());
            let parsed = crate::runtime::article_latest_parsed(
                meta.clone(),
                inc_site.clone(),
                inc_slug.clone(),
            )
            .secondary_get(ctx)
            .await;
            edges.push((origin.clone(), key.clone()));
            raws.insert(key.clone(), parsed.content.clone());
            queue.push_back((key, parsed.content.clone()));
        }
        raws.insert(origin, body);
    }
    let mut content = raws.remove(root).unwrap_or_default();
    let mut substituted: HashSet<Key> = HashSet::from([root.clone()]);
    loop {
        let mut targets: Vec<(Key, SafePathComponent, Slug)> = Vec::new();
        collect_include_targets(&content, &root.0, &substituted, &mut targets);
        if targets.is_empty() {
            break;
        }
        let fetched: HashMap<Key, Content> = targets
            .iter()
            .map(|(key, _, _)| (key.clone(), raws.remove(key).unwrap_or_default()))
            .collect();
        substituted.extend(fetched.keys().cloned());
        content = substitute_includes(content, &root.0, &fetched);
    }
    (content, dep_tree(root, edges))
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
        .map(|key| PageDep {
            site: (*key.0).clone(),
            category: key.1.as_ref().map(|c| (**c).clone()),
            page: (*key.2).clone(),
            deps: dep_children(children, children.get(key).map_or(&[], Vec::as_slice)),
        })
        .collect()
}

/// Resolve every mirrored external resource — `[[image source]]`, `[[[url]]]`
/// link targets, and `url()`/`@import` references inside `[[module css]]` — to
/// its content-addressed `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` URL and
/// substitute. Each resource is declared as a [`repo_resource`]
/// `secondary_get` dependency, so the result is reactive to an attachment
/// being re-mirrored (new hash) anywhere in the (already include-resolved)
/// tree. URLs that aren't mirrored (hotlinks) are left untouched so the client
/// loads them straight from the origin.
pub(super) async fn resolve_resources<S: Storage<KolorinkoRT>>(
    content: Content,
    site: &SafePathComponent,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> Content {
    let mut tails: Vec<String> = Vec::new();
    collect_external_refs(&content, &mut tails);
    if tails.is_empty() {
        return content;
    }
    let mut resolved: HashMap<String, CaRef> = HashMap::new();
    for tail in &tails {
        let Some(path) = RepoAssetPath::new(percent_decode(tail)) else {
            continue;
        };
        let ca = crate::runtime::repo_resource(meta.clone(), site.clone(), path)
            .secondary_get(ctx)
            .await;
        if let Some(ca_ref) = &*ca {
            resolved.insert(tail.clone(), ca_ref.clone());
        }
    }
    substitute_resources(content, site, &resolved)
}

/// Walk `content` and collect every mirrored-attachment `host/path` tail
/// reachable from an image source, a URL link target, or a stylesheet
/// reference — deduplicated, in first-appearance order. Mirrors the recursion
/// of [`collect_include_targets`] over the node tree.
pub(super) fn collect_external_refs(content: &Content, out: &mut Vec<String>) {
    let push = |t: String, out: &mut Vec<String>| {
        if !out.iter().any(|x| x == &t) {
            out.push(t);
        }
    };
    for node in content {
        match node {
            Node::Image { source, .. } => {
                if let Some(t) = ref_tail_of(source) {
                    push(t, out);
                }
            }
            Node::Link {
                target: LinkTarget::Url(u),
                ..
            } => {
                if let Some(t) = http_tail(u, None) {
                    push(t, out);
                }
            }
            Node::Stylesheet(css) => {
                for t in http_refs(css) {
                    push(t, out);
                }
            }
            Node::Container { content, .. } | Node::Heading { content, .. } => {
                collect_external_refs(content, out);
            }
            Node::Table(rows) => {
                for row in rows {
                    for cell in row {
                        collect_external_refs(&cell.content, out);
                    }
                }
            }
            Node::BlockTable(t) => {
                for row in &t.rows {
                    collect_external_refs(&row.content, out);
                }
            }
            Node::BlockCell(c) => collect_external_refs(&c.content, out),
            Node::SupSubscript { sup, sub } => {
                collect_external_refs(sup, out);
                collect_external_refs(sub, out);
            }
            Node::Link { text, .. } | Node::Footnote(text) => collect_external_refs(text, out),
            Node::Tabview(tabs) => {
                for tab in tabs {
                    collect_external_refs(&tab.name, out);
                    collect_external_refs(&tab.content, out);
                }
            }
            Node::ListPages(lp) => {
                collect_external_refs(&lp.prepend, out);
                collect_external_refs(&lp.repeat, out);
                collect_external_refs(&lp.append, out);
            }
            Node::List(list) => {
                for_each_content_in_list(list, &mut |c| collect_external_refs(c, out))
            }
            _ => {}
        }
    }
}

/// The http `host/path` tail of an image `source`, but only when it is purely
/// literal text (no module/include variables) — a variable URL can't be
/// content-addressed statically and is left for the client to resolve at render.
pub(super) fn ref_tail_of(source: &[TextObj]) -> Option<String> {
    let mut url = String::new();
    for obj in source {
        match obj {
            TextObj::Plain(s) => url.push_str(s),
            _ => return None,
        }
    }
    http_tail(&url, None)
}

/// Replace every mirrored-attachment reference in `content` with its
/// content-addressed URL from `resolved` (`host/path` tail → [`CaRef`]),
/// recursing over the same node tree as [`collect_external_refs`]. References
/// absent from `resolved` (un-mirrored hotlinks) pass through unchanged.
pub(super) fn substitute_resources(
    content: Content,
    site: &SafePathComponent,
    resolved: &HashMap<String, CaRef>,
) -> Content {
    let ca_for = |tail: &str| resolved.get(tail).map(|ca| ca_url(site, ca));
    content
        .into_iter()
        .map(|node| match node {
            Node::Image {
                align,
                source,
                params,
            } => Node::Image {
                align,
                source: subst_source(source, ca_for),
                params,
            },
            Node::Link { target, text } => {
                let text = substitute_resources(text, site, resolved);
                let target = match target {
                    LinkTarget::Url(u) => match http_tail(&u, None).and_then(|t| ca_for(&t)) {
                        Some(ca) => LinkTarget::Url(ca),
                        None => LinkTarget::Url(u),
                    },
                    other => other,
                };
                Node::Link { target, text }
            }
            Node::Stylesheet(css) => Node::Stylesheet(rewrite_with(&css, None, ca_for)),
            Node::Container { kind, content } => Node::Container {
                kind,
                content: substitute_resources(content, site, resolved),
            },
            Node::Heading { level, content } => Node::Heading {
                level,
                content: substitute_resources(content, site, resolved),
            },
            Node::Table(rows) => Node::Table(
                rows.into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| TableCell {
                                colspan: cell.colspan,
                                header: cell.header,
                                align: cell.align,
                                content: substitute_resources(cell.content, site, resolved),
                            })
                            .collect()
                    })
                    .collect(),
            ),
            Node::BlockTable(t) => Node::BlockTable(BlockTable {
                params: t.params,
                rows: t
                    .rows
                    .into_iter()
                    .map(|r| BlockRow {
                        params: r.params,
                        content: substitute_resources(r.content, site, resolved),
                    })
                    .collect(),
            }),
            Node::BlockCell(c) => Node::BlockCell(BlockCell {
                header: c.header,
                params: c.params,
                content: substitute_resources(c.content, site, resolved),
            }),
            Node::SupSubscript { sup, sub } => Node::SupSubscript {
                sup: substitute_resources(sup, site, resolved),
                sub: substitute_resources(sub, site, resolved),
            },
            Node::Footnote(c) => Node::Footnote(substitute_resources(c, site, resolved)),
            Node::Tabview(tabs) => Node::Tabview(
                tabs.into_iter()
                    .map(|tab| Tab {
                        name: substitute_resources(tab.name, site, resolved),
                        content: substitute_resources(tab.content, site, resolved),
                    })
                    .collect(),
            ),
            Node::ListPages(lp) => Node::ListPages(ListPages {
                params: lp.params,
                prepend: substitute_resources(lp.prepend, site, resolved),
                repeat: substitute_resources(lp.repeat, site, resolved),
                append: substitute_resources(lp.append, site, resolved),
            }),
            Node::List(list) => {
                Node::List(map_list(list, &|c| substitute_resources(c, site, resolved)))
            }
            other => other,
        })
        .collect()
}

/// Rewrite a purely-literal image `source` (`[Plain(url)]`) to its CA URL when
/// `ca_for` resolves it; leave sources with variables or non-http URLs as-is.
pub(super) fn subst_source<F: Fn(&str) -> Option<String>>(
    source: Vec<TextObj>,
    ca_for: F,
) -> Vec<TextObj> {
    let url = source.iter().try_fold(String::new(), |mut acc, o| match o {
        TextObj::Plain(s) => {
            acc.push_str(s);
            Some(acc)
        }
        _ => None,
    });
    if let Some(url) = url
        && let Some(tail) = http_tail(&url, None)
        && let Some(ca) = ca_for(&tail)
    {
        return vec![TextObj::Plain(ca)];
    }
    source
}

/// Walk `content` and record every `[[include]]` target not already in
/// `visited` (and not already batched in `out`), so [`resolve`] can fetch the
/// whole batch at once. Sync recursion over the tree — no awaits.
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
            Node::Container { content, .. } | Node::Heading { content, .. } => {
                collect_include_targets(content, current_site, visited, out);
            }
            Node::Table(rows) => {
                for row in rows {
                    for cell in row {
                        collect_include_targets(&cell.content, current_site, visited, out);
                    }
                }
            }
            Node::BlockTable(t) => {
                for row in &t.rows {
                    collect_include_targets(&row.content, current_site, visited, out);
                }
            }
            Node::BlockCell(c) => {
                collect_include_targets(&c.content, current_site, visited, out);
            }
            Node::SupSubscript { sup, sub } => {
                collect_include_targets(sup, current_site, visited, out);
                collect_include_targets(sub, current_site, visited, out);
            }
            Node::Link { text, .. } | Node::Footnote(text) => {
                collect_include_targets(text, current_site, visited, out);
            }
            Node::Tabview(tabs) => {
                for tab in tabs {
                    collect_include_targets(&tab.name, current_site, visited, out);
                    collect_include_targets(&tab.content, current_site, visited, out);
                }
            }
            Node::ListPages(lp) => {
                collect_include_targets(&lp.prepend, current_site, visited, out);
                collect_include_targets(&lp.repeat, current_site, visited, out);
                collect_include_targets(&lp.append, current_site, visited, out);
            }
            Node::List(list) => for_each_content_in_list(list, &mut |c| {
                collect_include_targets(c, current_site, visited, out)
            }),
            _ => {}
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
            Node::Container { kind, content } => out.push(Node::Container {
                kind,
                content: substitute_includes(content, current_site, fetched),
            }),
            Node::Heading { level, content } => out.push(Node::Heading {
                level,
                content: substitute_includes(content, current_site, fetched),
            }),
            Node::Table(rows) => out.push(Node::Table(
                rows.into_iter()
                    .map(|row| {
                        row.into_iter()
                            .map(|cell| kolorinko_wikitext::TableCell {
                                colspan: cell.colspan,
                                header: cell.header,
                                align: cell.align,
                                content: substitute_includes(cell.content, current_site, fetched),
                            })
                            .collect()
                    })
                    .collect(),
            )),
            Node::BlockTable(t) => out.push(Node::BlockTable(BlockTable {
                params: t.params,
                rows: t
                    .rows
                    .into_iter()
                    .map(|r| BlockRow {
                        params: r.params,
                        content: substitute_includes(r.content, current_site, fetched),
                    })
                    .collect(),
            })),
            Node::BlockCell(c) => out.push(Node::BlockCell(BlockCell {
                header: c.header,
                params: c.params,
                content: substitute_includes(c.content, current_site, fetched),
            })),
            Node::SupSubscript { sup, sub } => out.push(Node::SupSubscript {
                sup: substitute_includes(sup, current_site, fetched),
                sub: substitute_includes(sub, current_site, fetched),
            }),
            Node::Link { target, text } => out.push(Node::Link {
                target,
                text: substitute_includes(text, current_site, fetched),
            }),
            Node::Footnote(c) => out.push(Node::Footnote(substitute_includes(
                c,
                current_site,
                fetched,
            ))),
            Node::Tabview(tabs) => out.push(Node::Tabview(
                tabs.into_iter()
                    .map(|tab| kolorinko_wikitext::Tab {
                        name: substitute_includes(tab.name, current_site, fetched),
                        content: substitute_includes(tab.content, current_site, fetched),
                    })
                    .collect(),
            )),
            Node::ListPages(lp) => out.push(Node::ListPages(kolorinko_wikitext::ListPages {
                params: lp.params,
                prepend: substitute_includes(lp.prepend, current_site, fetched),
                repeat: substitute_includes(lp.repeat, current_site, fetched),
                append: substitute_includes(lp.append, current_site, fetched),
            })),
            Node::List(list) => out.push(Node::List(map_list(list, &|c| {
                substitute_includes(c, current_site, fetched)
            }))),
            leaf => out.push(leaf),
        }
    }
    out
}

/// Replace every [`TextObj::IncludeVar`] in `content` using `vars`: a standalone
/// variable expands to its (recursively substituted) [`Content`]; inside an
/// attribute or image source it is flattened to plain text. An unresolved
/// variable falls back to its `//default`, or to nothing when it has none.
///
/// `vars` keeps duplicate keys in source order; a lookup takes the first
/// non-empty value, so the `key={$key}|key=default` include idiom resolves to
/// the passed value when set and to the default otherwise.
pub(super) fn apply_include_vars(content: Content, vars: &[(String, Content)]) -> Content {
    content
        .into_iter()
        .flat_map(|n| subst_node(n, vars))
        .collect()
}

/// A substitution value is "empty" when it renders to nothing — the form an
/// unset `{$key}` passthrough takes (an empty [`Content`]). The fallback idiom
/// skips such values to reach the literal default.
fn content_is_empty(c: &[Node]) -> bool {
    c.iter().all(|n| match n {
        Node::Text(TextObj::Plain(s)) => s.is_empty(),
        _ => false,
    })
}

/// Look up `name` among ordered include `vars`, taking the first value that is
/// not empty.
fn include_var_value<'a>(vars: &'a [(String, Content)], name: &str) -> Option<&'a Content> {
    vars.iter()
        .find(|(k, v)| k == name && !content_is_empty(v))
        .map(|(_, v)| v)
}

pub(super) fn subst_node(node: Node, vars: &[(String, Content)]) -> Content {
    match node {
        Node::Text(TextObj::IncludeVar { name, default }) => match include_var_value(vars, &name) {
            Some(v) => apply_include_vars(v.clone(), vars),
            None => default
                .map(|d| apply_include_vars(d, vars))
                .unwrap_or_default(),
        },
        Node::Text(other) => vec![Node::Text(other)],
        Node::Container { kind, content } => vec![Node::Container {
            kind: subst_kind(kind, vars),
            content: apply_include_vars(content, vars),
        }],
        Node::Heading { level, content } => vec![Node::Heading {
            level,
            content: apply_include_vars(content, vars),
        }],
        Node::Image {
            align,
            source,
            params,
        } => vec![Node::Image {
            align,
            source: subst_textobjs(source, vars),
            params: subst_params(params, vars),
        }],
        Node::Table(rows) => vec![Node::Table(
            rows.into_iter()
                .map(|row| {
                    row.into_iter()
                        .map(|c| TableCell {
                            colspan: c.colspan,
                            header: c.header,
                            align: c.align,
                            content: apply_include_vars(c.content, vars),
                        })
                        .collect()
                })
                .collect(),
        )],
        Node::BlockTable(t) => vec![Node::BlockTable(BlockTable {
            params: subst_params(t.params, vars),
            rows: t
                .rows
                .into_iter()
                .map(|r| BlockRow {
                    params: subst_params(r.params, vars),
                    content: apply_include_vars(r.content, vars),
                })
                .collect(),
        })],
        Node::BlockCell(c) => vec![Node::BlockCell(BlockCell {
            header: c.header,
            params: subst_params(c.params, vars),
            content: apply_include_vars(c.content, vars),
        })],
        Node::SupSubscript { sup, sub } => vec![Node::SupSubscript {
            sup: apply_include_vars(sup, vars),
            sub: apply_include_vars(sub, vars),
        }],
        Node::Link { target, text } => vec![Node::Link {
            target,
            text: apply_include_vars(text, vars),
        }],
        Node::Include(inc) => vec![Node::Include(Include {
            source: inc.source,
            vars: inc
                .vars
                .into_iter()
                .map(|(k, v)| (k, apply_include_vars(v, vars)))
                .collect(),
        })],
        Node::ListPages(lp) => vec![Node::ListPages(ListPages {
            params: lp.params,
            prepend: apply_include_vars(lp.prepend, vars),
            repeat: apply_include_vars(lp.repeat, vars),
            append: apply_include_vars(lp.append, vars),
        })],
        Node::Footnote(c) => vec![Node::Footnote(apply_include_vars(c, vars))],
        Node::Tabview(tabs) => vec![Node::Tabview(
            tabs.into_iter()
                .map(|t| Tab {
                    name: apply_include_vars(t.name, vars),
                    content: apply_include_vars(t.content, vars),
                })
                .collect(),
        )],
        Node::List(list) => vec![Node::List(map_list(list, &|c| apply_include_vars(c, vars)))],
        Node::Date { .. }
        | Node::HorizontalRule
        | Node::Raw(_)
        | Node::Stylesheet(_)
        | Node::Module(_)
        | Node::Code(_) => {
            vec![node]
        }
    }
}

/// Walk a [`List`], producing a new one whose every item body (and nested
/// sublist body) is transformed by `f`.
pub(super) fn map_list<F: Fn(Content) -> Content>(list: List, f: &F) -> List {
    List {
        ordered: list.ordered,
        items: list
            .items
            .into_iter()
            .map(|item| ListItem {
                content: f(item.content),
                sublist: item.sublist.map(|b| Box::new(map_list(*b, f))),
            })
            .collect(),
    }
}

/// Borrow-walking twin of [`map_list`]: visit every item body in `list` (and
/// nested sublists) with `f`.
pub(super) fn for_each_content_in_list<F: FnMut(&Content)>(list: &List, f: &mut F) {
    for item in &list.items {
        f(&item.content);
        if let Some(sub) = &item.sublist {
            for_each_content_in_list(sub, f);
        }
    }
}

pub(super) fn subst_kind(kind: ContainerKind, vars: &[(String, Content)]) -> ContainerKind {
    match kind {
        ContainerKind::Div {
            inline,
            block,
            params,
        } => ContainerKind::Div {
            inline,
            block,
            params: subst_params(params, vars),
        },
        other => other,
    }
}

pub(super) fn subst_params(
    params: HashMap<String, Vec<TextObj>>,
    vars: &[(String, Content)],
) -> HashMap<String, Vec<TextObj>> {
    params
        .into_iter()
        .map(|(k, v)| (k, subst_textobjs(v, vars)))
        .collect()
}

pub(super) fn subst_textobjs(objs: Vec<TextObj>, vars: &[(String, Content)]) -> Vec<TextObj> {
    let mut out: Vec<TextObj> = Vec::new();
    for o in objs {
        let resolved: Vec<TextObj> = match o {
            TextObj::IncludeVar { name, default } => match include_var_value(vars, &name) {
                Some(v) => flatten_textobjs(&apply_include_vars(v.clone(), vars)),
                None => match default {
                    Some(d) => flatten_textobjs(&apply_include_vars(d, vars)),
                    None => Vec::new(),
                },
            },
            other => vec![other],
        };
        for r in resolved {
            match (&r, out.last_mut()) {
                (TextObj::Plain(s), Some(TextObj::Plain(prev))) => prev.push_str(s),
                _ => out.push(r),
            }
        }
    }
    out
}

/// Flatten parsed [`Content`] back into plain [`TextObj`] text for the contexts
/// (attribute values, image sources) that only carry text.
pub(super) fn flatten_textobjs(content: &Content) -> Vec<TextObj> {
    let mut s = String::new();
    collect_plain(content, &mut s);
    if s.is_empty() {
        Vec::new()
    } else {
        vec![TextObj::Plain(s)]
    }
}

pub(super) fn collect_plain(content: &Content, out: &mut String) {
    for n in content {
        match n {
            Node::Text(TextObj::Plain(s)) => out.push_str(s),
            Node::Text(TextObj::IncludeVar { default, .. }) => {
                if let Some(d) = default {
                    collect_plain(d, out);
                }
            }
            Node::Text(TextObj::ModuleVar { default, .. }) => {
                if let Some(d) = default {
                    out.push_str(d);
                }
            }
            Node::Container { content, .. }
            | Node::Heading { content, .. }
            | Node::Footnote(content) => collect_plain(content, out),
            Node::Link { text, .. } => collect_plain(text, out),
            Node::SupSubscript { sup, sub } => {
                collect_plain(sup, out);
                collect_plain(sub, out);
            }
            _ => {}
        }
    }
}

/// Resolve an include's [`PageRef`] to `(site, slug)` on the current site.
/// The parser parks the first `:`-segment of the source in [`PageRef::space`];
/// for same-site page refs that segment is the category, so `space` → category
/// and the trailing path → name. Cross-site includes (`space` = another site)
/// are not yet supported. Unresolvable targets (bad path component) return
/// `None` and the directive is left in place.
pub(super) fn include_target(
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
