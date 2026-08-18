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
// `article_latest` gear — the resolution pipeline
// =========================================================================

/// No carry-over state: the result is fully re-derived each run from the parse
/// gears it depends on (which the framework re-runs on any change).
#[derive(Default, Clone, Debug)]
pub(crate) struct LatestCache;

/// Render a page's final [`ArticleView`] by running [`resolve_full`] — the
/// `[[include]]` / `[[module ListPages]]` / `[[iftags]]` / resource resolution
/// pipeline. Declaring each fetched page and ListPages selection as a
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) dependency
/// makes the whole result reactive — an edit anywhere in the include or
/// transclusion cone re-runs this gear. The tree of every page fetched along
/// the way rides along as the view's `deps`.
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
    let host = HostCtx {
        fullname: page_meta.slug.clone(),
        category: slug.0.as_ref().map(|c| (**c).clone()),
        tags: page_meta.tags.clone(),
    };
    let mut visited = HashSet::from([(site.clone(), slug.0.clone(), slug.1.clone())]);
    let (content, deps) = resolve_full(content, site, slug, host, &mut visited, meta, ctx).await;
    ArticleView {
        meta: page_meta,
        revisions,
        content,
        deps,
    }
}

/// The full resolution pipeline — `[[include]]`, `[[module ListPages]]`,
/// `[[iftags]]`, mirrored resources, then include-vars — run on `content` in
/// the context of page (`site`, `slug`, `host`), returning the resolved
/// content together with its dependency tree: the include cone plus every
/// listed page fetched for a `%%content%%` transclusion. Shared between the
/// gear's own resolution and the recursive resolution of a `%%content%%`
/// transclusion (a ListPages template embedding a listed page's rendered
/// body), so a single growing `visited` set breaks every data-level cycle
/// (A includes/transcludes B … A) the same way [`resolve`] does for includes.
pub(super) async fn resolve_full<S: Storage<KolorinkoRT>>(
    content: Content,
    site: SafePathComponent,
    slug: Slug,
    host: HostCtx,
    visited: &mut HashSet<Key>,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    Box::pin(async move {
        // TODO: Single-pass would be nice, but whatever
        let root = (site.clone(), slug.0.clone(), slug.1.clone());
        let (content, mut deps) = resolve_include(content, &root, visited, meta, ctx).await;
        let (content, listed) = resolve_listpages(content, &site, meta, &host, visited, ctx).await;
        deps.extend(listed);
        let content = evaluate_iftags(content, &host.tags);
        let content = resolve_resources(content, &site, meta, ctx).await;
        (apply_include_vars(content, &[]), deps)
    })
    .await
}

/// Resolve every `[[include]]` directive anywhere inside the root page's
/// content, splicing each included page's content in place of the directive,
/// and return the resolved content together with the dependency tree — every
/// page fetched, nested under the page whose body included it.
///
/// Fetching is breadth-first: each page's raw body is walked as it arrives,
/// its include targets — those not already in `visited` (which must hold
/// `root` and grows with every fetch, so a directive into an
/// already-resolved page stays verbatim) — declared as
/// [`article_latest_parsed`]
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependencies (so the whole result is reactive to edits anywhere in the
/// transitive include cone) and fetched, and the `(includer, target)` edge
/// recorded; this breaks data-level cycles (A includes B includes A).
/// Assembly then runs in passes over the pre-fetched bodies — each pass
/// substitutes one level of directives, so include vars cascade top-down
/// (an includer's vars resolve a nested directive's `{$passthrough}` values
/// before that directive itself is spliced) — until a pass finds nothing new.
pub(super) async fn resolve_include<S: Storage<KolorinkoRT>>(
    content: Content,
    root: &Key,
    visited: &mut HashSet<Key>,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    let mut queue: VecDeque<(Key, Content)> = VecDeque::from([(root.clone(), content)]);
    let mut raws: HashMap<Key, Content> = HashMap::new();
    let mut edges: Vec<(Key, Key)> = Vec::new();
    while let Some((origin, body)) = queue.pop_front() {
        let mut targets: Vec<(Key, SafePathComponent, Slug)> = Vec::new();
        collect_include_targets(&body, &root.0, visited, &mut targets);
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
