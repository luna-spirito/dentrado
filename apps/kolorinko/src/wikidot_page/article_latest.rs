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
    let mut state = ResolveState::new(site.clone());
    let (content, deps) = resolve_full(content, slug, host, &mut state, meta, ctx).await;
    ArticleView {
        meta: page_meta,
        revisions,
        content,
        deps,
    }
}

/// State shared by one [`resolve_full`] run and every `%%content%%`
/// transclusion it recurses into: the (constant) site, the parsed body of
/// every fetched page — fetch once, splice wherever a directive or a
/// transclusion re-encounters it — the resolved `%%content%%` bodies keyed
/// by fullname, and the pages whose resolution has already run, which is
/// what stops transclusion cycles.
pub(super) struct ResolveState {
    pub(super) site: SafePathComponent,
    pub(super) raws: HashMap<Key, Content>,
    pub(super) bodies: HashMap<String, Content>,
    pub(super) resolved: HashSet<Key>,
}

impl ResolveState {
    fn new(site: SafePathComponent) -> Self {
        Self {
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
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    Box::pin(async move {
        let origin = (state.site.clone(), slug.0, slug.1);
        state.resolved.insert(origin.clone());
        let (content, mut deps) = resolve_include(content, &origin, state, meta, ctx).await;
        let (content, listed) = resolve_listpages(content, state, meta, &host, ctx).await;
        deps.extend(listed);
        let content = evaluate_iftags(content, &host.tags);
        let content = resolve_resources(content, &state.site, meta, ctx).await;
        (content, deps)
    })
    .await
}

/// Resolve every `[[include]]` directive anywhere inside the body of
/// `origin`: first the whole include cone is fetched breadth-first — each
/// page declared as an [`article_latest_parsed`]
/// [`secondary_get`](dentrado::core::gear::GearQuery::secondary_get)
/// dependency (so the result is reactive to edits anywhere in the cone) and
/// cached in `state.raws`, so a diamond A→B→D, A→C→D fetches D once — then
/// the body is assembled in a single recursive pass
/// ([`substitute_includes`]) that breaks data-level cycles (A includes B
/// includes A) by tracking the recursion path. Returns the assembled
/// content with the dependency tree: one node per fetched page, nested
/// under the page whose body first included it.
pub(super) async fn resolve_include<S: Storage<KolorinkoRT>>(
    content: Content,
    origin: &Key,
    state: &mut ResolveState,
    meta: &RepoMeta,
    ctx: &mut GearCtx<KolorinkoRT, S>,
) -> (Content, Vec<PageDep>) {
    let mut edges: Vec<(Key, Key)> = Vec::new();
    let mut queue: VecDeque<(Key, Content)> = VecDeque::from([(origin.clone(), content.clone())]);
    while let Some((includer, body)) = queue.pop_front() {
        let mut targets: Vec<(Key, SafePathComponent, Slug)> = Vec::new();
        collect_include_targets(&body, &state.site, &state.raws, &mut targets);
        for (key, inc_site, inc_slug) in targets {
            if key == *origin {
                continue;
            }
            let parsed = crate::runtime::article_latest_parsed(
                meta.clone(),
                inc_site.clone(),
                inc_slug.clone(),
            )
            .secondary_get(ctx)
            .await;
            edges.push((includer.clone(), key.clone()));
            state.raws.insert(key.clone(), parsed.content.clone());
            queue.push_back((key, parsed.content.clone()));
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
