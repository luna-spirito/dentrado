use super::*;
use std::collections::VecDeque;

// =========================================================================
// `article_latest_parsed` gear
// =========================================================================

/// Cache for [`article_latest_parsed`]: the last body and the [`ArticleView`]
/// parsed from it. Both are snapshot-shared (`Arc`), so an unchanged page is
/// recognised by body equality and its cached parse reused — only a
/// genuinely-changed page is re-parsed.
#[derive(Default, Clone, Debug)]
pub(crate) struct ParsedCache {
    pub(super) body: Option<Arc<str>>,
    pub(super) view: Option<ArticleView>,
}

/// Parse a page's latest body into an [`ArticleView`] **without** resolving
/// `[[include]]` directives. Keyed canonically and living off the `repo`
/// core, it reads the page out of the one [`repo_snap`] dependency per run
/// ([`secondary_get`](dentrado::core::gear::GearQuery::secondary_get) — a
/// local lookup thereafter; never depending on another parse gear), so the
/// parse layer is acyclic.
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    let snap = crate::runtime::repo_snap().secondary_get(ctx).await;
    let latest = latest(&snap, space, local);
    let latest = latest.as_ref();
    let body: Arc<str> = latest.map_or_else(|| Arc::from(""), |p| Arc::clone(p.body));
    if cache.body.as_deref() == Some(&*body)
        && let Some(view) = &cache.view
    {
        return view.clone();
    }
    let (meta, revisions) = latest.map_or_else(
        || (ArticleMeta::default(), Vec::new()),
        |p| (p.meta.clone(), p.revisions.to_vec()),
    );
    let view = ArticleView {
        meta,
        revisions,
        content: parse(&body),
        deps: Vec::new(),
    };
    *cache = ParsedCache {
        body: Some(body),
        view: Some(view.clone()),
    };
    view
}

// =========================================================================
// `article_latest` gear — the canonical-address resolution pipeline
// =========================================================================

/// No carry-over state: the result is fully re-derived each run from the
/// follow target and the snapshot it reads (which the framework re-runs on
/// any change).
#[derive(Default, Clone, Debug)]
pub(crate) struct LatestCache;

/// Render a page's final [`ArticleView`] by running [`resolve_full`] — the
/// `[[include]]` / `[[module ListPages]]` / `[[iftags]]` / link / resource
/// resolution. The gear is keyed by the canonical address `(space, local)` —
/// exactly what the URL and the client subscription name — and follows the
/// page's own parse ([`article_latest_parsed`], co-located off the `repo`
/// core): the parse's `meta` carries the current slug, so the site/slug
/// context is rename-reactive without a `repo` round-trip. The one
/// [`repo_snap`] read per run brings the whole corpus local, and the body
/// plus every include, ListPages selection, link set, and resource in the
/// cone is then resolved from it by pure lookups — includes spliced into
/// the raw body **before** parsing (Wikidot's own textual assembly, which
/// lets a component's half-open `[[div]]` or `[[cell]]` pair with the
/// includer's closer). Reactivity rides that single dependency: any repo
/// change re-runs this gear, and the parse/body caches keep the unchanged
/// 99% cheap. The tree of every page fetched along the way rides along as
/// the view's `deps`. An unregistered space or unknown local id yields an
/// empty view (the HTTP layer turns that into a 404).
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    _parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut LatestCache,
) -> ArticleView {
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return ArticleView::default();
    };
    let snap = crate::runtime::repo_snap().secondary_get(ctx).await;
    let latest = latest(&snap, space, local);
    let latest = latest.as_ref();
    let page_meta = latest.map_or_else(ArticleMeta::default, |p| p.meta.clone());
    let body = latest.map_or_else(|| Arc::from(""), |p| Arc::clone(p.body));
    let revisions = latest.map_or_else(Vec::new, |p| p.revisions.to_vec());
    // A missing page parses to an empty view (blank slug) — the 404 shape.
    let Some(slug) = parse_slug(&page_meta.slug) else {
        return ArticleView::default();
    };
    let host = HostCtx {
        fullname: page_meta.slug.clone(),
        category: slug.0.as_ref().map(|c| (**c).clone()),
        tags: page_meta.tags.clone(),
    };
    let mut state = ResolveState::new(space, site, RepoSnapshot::clone(&snap));
    let (mut content, deps) = resolve_full(body, slug, host, &mut state);
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
/// dataset site, the snapshot every fetch reads from (one per run — this is
/// what makes the whole resolution loop await-free), the raw body of every
/// fetched page (fetch once, splice wherever a directive or a transclusion
/// re-encounters it), the resolved `%%content%%` bodies keyed by fullname,
/// and the pages whose resolution has already run, which is what stops
/// transclusion cycles.
pub(super) struct ResolveState {
    pub(super) space: SpaceId,
    pub(super) site: SafePathComponent,
    pub(super) snap: RepoSnapshot,
    pub(super) raws: HashMap<Key, Arc<str>>,
    pub(super) bodies: HashMap<String, Content>,
    pub(super) resolved: HashSet<Key>,
}

impl ResolveState {
    fn new(space: SpaceId, site: SafePathComponent, snap: RepoSnapshot) -> Self {
        Self {
            space,
            site,
            snap,
            raws: HashMap::new(),
            bodies: HashMap::new(),
            resolved: HashSet::new(),
        }
    }
}

/// The full resolution pipeline — textual `[[include]]` assembly and the
/// parse of its result, `[[module ListPages]]`, `[[iftags]]`, internal
/// links, mirrored resources — run on `body` in the context of the hosting
/// page (`site`, `slug`, `host`), returning the resolved content together
/// with its dependency tree: the include cone plus every listed page fetched
/// for a `%%content%%` transclusion. Shared between the gear's own resolution
/// and the recursive resolution of a `%%content%%` transclusion (a ListPages
/// template embedding a listed page's rendered body), all through one
/// [`ResolveState`]: bodies are fetched and resolved once per run no matter
/// how many transclusions reach for them, and a page already being resolved
/// (the rendering page, or a listed page further up the recursion) is never
/// re-entered. Every dataset read is a local snapshot lookup — the whole
/// pipeline is synchronous.
pub(super) fn resolve_full(
    body: Arc<str>,
    slug: Slug,
    host: HostCtx,
    state: &mut ResolveState,
) -> (Content, Vec<PageDep>) {
    let origin = (state.site.clone(), slug.0, slug.1);
    state.resolved.insert(origin.clone());
    let (assembled, mut deps) = resolve_include(&body, &origin, state);
    let (content, listed) = resolve_listpages(parse(&assembled), state, &host);
    deps.extend(listed);
    let content = evaluate_iftags(content, &host.tags);
    let content = resolve_links(content, &state.site, &state.snap);
    let content = resolve_resources(content, &state.site, &state.snap);
    (content, deps)
}

/// Resolve every `[[include]]` directive anywhere inside the raw `body` of
/// `origin`: first the whole include cone is fetched breadth-first — each
/// body a local snapshot read, cached in `state.raws`, so a diamond
/// A→B→D, A→C→D fetches D once — then the body is assembled in a single
/// recursive pass ([`splice_includes`]) that breaks data-level cycles
/// (A includes B includes A) by tracking the recursion path. Returns the
/// assembled text with the dependency tree: one node per fetched page,
/// nested under the page whose body first included it.
///
/// Includes are slug-addressed while the snapshot's canonical projection
/// is keyed by page id, so each hop bridges through
/// [`RepoSnapshot::local_id`] (includes are always same-site, hence the
/// same space); a page the site doesn't have splices empty — the same
/// blank a canonical miss produced before the re-keying.
pub(super) fn resolve_include(
    body: &str,
    origin: &Key,
    state: &mut ResolveState,
) -> (String, Vec<PageDep>) {
    let mut edges: Vec<(Key, Key)> = Vec::new();
    let mut queue: VecDeque<(Key, Arc<str>)> = VecDeque::from([(origin.clone(), Arc::from(body))]);
    while let Some((includer, text)) = queue.pop_front() {
        for d in live_directives(&text) {
            let Some((inc_site, inc_slug)) = d.target(&state.site) else {
                continue;
            };
            let key = (inc_site, inc_slug.0.clone(), inc_slug.1.clone());
            if key == *origin || state.raws.contains_key(&key) {
                continue;
            }
            let content = match local_id(&state.snap, &state.site, &inc_slug) {
                Some((inc_local, _)) => latest(&state.snap, state.space, inc_local)
                    .map_or_else(|| Arc::from(""), |p| Arc::clone(p.body)),
                None => Arc::from(""),
            };
            edges.push((includer.clone(), key.clone()));
            state.raws.insert(key.clone(), Arc::clone(&content));
            queue.push_back((key, content));
        }
    }
    (
        splice_includes(
            &subst_vars(body, &[]),
            &state.site,
            &state.raws,
            std::slice::from_ref(origin),
        ),
        dep_tree(origin, edges),
    )
}
