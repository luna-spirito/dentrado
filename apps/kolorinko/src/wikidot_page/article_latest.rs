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
/// `[[include]]` directives. Keyed canonically and living off the `repo`
/// core, it pulls the page through the [`repo_l_article_latest`] lens via
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) (never
/// depending on another parse gear), so the parse layer is acyclic.
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    let latest = crate::runtime::repo_l_article_latest(space, local)
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
// `article_latest` gear — the canonical-address resolution pipeline
// =========================================================================

/// No carry-over state: the result is fully re-derived each run from the
/// follow target and the parse gears it depends on (which the framework
/// re-runs on any change).
#[derive(Default, Clone, Debug)]
pub(crate) struct LatestCache;

/// Render a page's final [`ArticleView`] by running [`resolve_full`] — the
/// `[[include]]` / `[[module ListPages]]` / `[[iftags]]` / resource resolution
/// pipeline. The gear is keyed by the canonical address `(space, local)` —
/// exactly what the URL and the client subscription name — and follows the
/// page's own parse ([`article_latest_parsed`], co-located off the `repo`
/// core): the parse's `meta` carries the current slug, so the site/slug
/// context is rename-reactive without a `repo` round-trip. Declaring each
/// fetched page and ListPages selection the same way (an [`article_latest_parsed`]
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependency, includes bridged through [`repo_l_local_id`]) makes the whole
/// result reactive — an edit anywhere in the include or transclusion cone
/// re-runs this gear. The tree of every page fetched along the way rides
/// along as the view's `deps`. An unregistered space or unknown local id
/// yields an empty view (the HTTP layer turns that into a 404).
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    _local: LocalId,
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut LatestCache,
) -> ArticleView {
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return ArticleView::default();
    };
    let ArticleView {
        meta: page_meta,
        revisions,
        content,
        ..
    } = parsed.clone();
    // A missing page parses to an empty view (blank slug) — the 404 shape.
    let Some(slug) = slug_to_key(&page_meta.slug) else {
        return ArticleView::default();
    };
    let host = HostCtx {
        fullname: page_meta.slug.clone(),
        category: slug.0.as_ref().map(|c| (**c).clone()),
        tags: page_meta.tags.clone(),
    };
    let mut state = ResolveState::new(space, site);
    let (mut content, deps) = resolve_full(content, slug, host, &mut state, ctx).await;
    kolorinko_wikitext::assign_toc_anchors(&mut content);
    ArticleView {
        meta: page_meta,
        revisions,
        content,
        deps,
    }
}

/// State shared by one [`resolve_full`] run and every `%%content%%`
/// transclusion it recurses into: the (constant) canonical space and its
/// dataset site, the parsed body of every fetched page — fetch once, splice
/// wherever a directive or a transclusion re-encounters it — the resolved
/// `%%content%%` bodies keyed by fullname, and the pages whose resolution has
/// already run, which is what stops transclusion cycles.
pub(super) struct ResolveState {
    pub(super) space: SpaceId,
    pub(super) site: SafePathComponent,
    pub(super) raws: HashMap<Key, Content>,
    pub(super) bodies: HashMap<String, Content>,
    pub(super) resolved: HashSet<Key>,
}

impl ResolveState {
    fn new(space: SpaceId, site: SafePathComponent) -> Self {
        Self {
            space,
            site,
            raws: HashMap::new(),
            bodies: HashMap::new(),
            resolved: HashSet::new(),
        }
    }
}

/// The full resolution pipeline — `[[include]]`, `[[module ListPages]]`,
/// `[[iftags]]`, mirrored resources — run on `content` in the context of
/// page (`site`, `slug`, `host`), returning the resolved content together
/// with its dependency tree: the include cone plus every listed page fetched
/// for a `%%content%%` transclusion. Shared between the gear's own resolution
/// and the recursive resolution of a `%%content%%` transclusion (a ListPages
/// template embedding a listed page's rendered body), all through one
/// [`ResolveState`]: bodies are fetched and resolved once per run no matter
/// how many transclusions reach for them, and a page already being resolved
/// (the rendering page, or a listed page further up the recursion) is never
/// re-entered.
pub(super) async fn resolve_full<S: Storage<KolorinkoRT>>(
    content: Content,
    slug: Slug,
    host: HostCtx,
    state: &mut ResolveState,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    Box::pin(async move {
        let origin = (state.site.clone(), slug.0, slug.1);
        state.resolved.insert(origin.clone());
        let (content, mut deps) = resolve_include(content, &origin, state, ctx).await;
        let (content, listed) = resolve_listpages(content, state, &host, ctx).await;
        deps.extend(listed);
        let content = evaluate_iftags(content, &host.tags);
        let content = resolve_resources(content, &state.site, ctx).await;
        (content, deps)
    })
    .await
}

/// Resolve every `[[include]]` directive anywhere inside the body of
/// `origin`: first the whole include cone is fetched breadth-first — each
/// page declared as a [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependency (so the result is reactive to edits anywhere in the cone) and
/// cached in `state.raws`, so a diamond A→B→D, A→C→D fetches D once — then
/// the body is assembled in a single recursive pass
/// ([`substitute_includes`]) that breaks data-level cycles (A includes B
/// includes A) by tracking the recursion path. Returns the assembled
/// content with the dependency tree: one node per fetched page, nested
/// under the page whose body first included it.
///
/// Includes are slug-addressed while the parse gears are canonical, so each
/// hop bridges through the [`repo_l_local_id`] lens (includes are always
/// same-site, hence the same space); a page the site doesn't have splices
/// empty — the same blank the lens-level miss produced before the canonical
/// re-keying.
pub(super) async fn resolve_include<S: Storage<KolorinkoRT>>(
    content: Content,
    origin: &Key,
    state: &mut ResolveState,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    let mut edges: Vec<(Key, Key)> = Vec::new();
    let mut queue: VecDeque<(Key, Content)> = VecDeque::from([(origin.clone(), content.clone())]);
    while let Some((includer, body)) = queue.pop_front() {
        let mut targets: Vec<(Key, Slug)> = Vec::new();
        collect_include_targets(&body, &state.site, &state.raws, &mut targets);
        for (key, inc_slug) in targets {
            if key == *origin {
                continue;
            }
            let id = crate::runtime::repo_l_local_id(state.site.clone(), inc_slug.clone())
                .secondary_get(ctx)
                .await;
            let content = match &*id {
                Some((inc_local, _)) => {
                    crate::runtime::article_latest_parsed(state.space, *inc_local)
                        .secondary_get(ctx)
                        .await
                        .content
                        .clone()
                }
                None => Vec::new(),
            };
            edges.push((includer.clone(), key.clone()));
            state.raws.insert(key.clone(), content.clone());
            queue.push_back((key, content));
        }
    }
    (
        substitute_includes(
            content,
            &state.site,
            &state.raws,
            std::slice::from_ref(origin),
        ),
        dep_tree(origin, edges),
    )
}
