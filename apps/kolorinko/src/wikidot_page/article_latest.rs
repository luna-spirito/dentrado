use super::*;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

// =========================================================================
// `article_latest_parsed` gear
// =========================================================================

/// Cache for [`article_latest_parsed`]: the archive SHA-256 the view was
/// parsed from. An unchanged archive reuses the cached parse; a changed
/// archive (or a rename, which only changes the metadata) re-parses and the
/// fresh [`PageMeta`] is stamped over the cached view.
#[derive(Default, Clone, Debug)]
pub(crate) struct ParsedCache {
    pub(super) archive: Option<String>,
    pub(super) view: Option<ArticleView>,
}

/// The persisted form of [`ParsedCache`].
#[derive(serde::Serialize, serde::Deserialize)]
struct ParsedImage {
    archive: Option<String>,
    view: Option<ArticleView>,
}

/// The gear-view key of the persisted parse image.
const PARSED_KEY: &[u8] = b"parsed";

/// Load the parse cache from the view; an absent or unreadable image is a
/// cold start (the parse is recomputable, never authoritative).
async fn load_parsed<V: GearView>(view: &mut V) -> ParsedCache {
    let Some(bytes) = chunked::load(view, PARSED_KEY).await else {
        return ParsedCache::default();
    };
    match postcard::from_bytes::<ParsedImage>(&bytes) {
        Ok(img) => ParsedCache {
            archive: img.archive,
            view: img.view,
        },
        Err(e) => {
            error!("discarding an unreadable parse cache: {e}");
            ParsedCache::default()
        }
    }
}

/// Stage the parse cache as a chunked image in the view.
async fn store_parsed<V: GearView>(view: &mut V, cache: &ParsedCache) {
    let image = ParsedImage {
        archive: cache.archive.clone(),
        view: cache.view.clone(),
    };
    let bytes = postcard::to_stdvec(&image).expect("a parse cache image serializes");
    chunked::store(view, PARSED_KEY, &bytes).await;
}

/// Parse a page's latest body into an [`ArticleView`] **without** resolving
/// `[[include]]` directives. Keyed canonically and living off the `repo`
/// core, it reads the site's [`RepoMeta`] and the page's archive directly
/// (never depending on another parse gear), so the parse layer is acyclic.
pub(crate) async fn article_latest_parsed<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
) -> ArticleView {
    let cache = load_parsed(view).await;
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return ArticleView::default();
    };
    let meta = crate::runtime::repo(site.clone()).secondary_get(ctx).await;
    let Some(page) = latest(&meta, local) else {
        return ArticleView::default();
    };
    let archive = page.archive.as_ref().map(|a| a.sha256.clone());
    if archive.is_some()
        && cache.archive == archive
        && let Some(cached) = &cache.view
    {
        let mut parsed = cached.clone();
        parsed.meta = page.article_meta();
        parsed.revisions = page.revisions.clone();
        return parsed;
    }
    let body = read_latest_body(&site, page)
        .await
        .unwrap_or_else(|| Arc::from(""));
    let parsed = ArticleView {
        meta: page.article_meta(),
        revisions: page.revisions.clone(),
        content: parse(&body),
        deps: Vec::new(),
    };
    store_parsed(
        view,
        &ParsedCache {
            archive,
            view: Some(parsed.clone()),
        },
    )
    .await;
    parsed
}

// =========================================================================
// `article_latest` gear — the canonical-address resolution pipeline
// =========================================================================

/// Render a page's final [`ArticleView`] by running [`resolve_full`] — the
/// `[[include]]` / `[[module ListPages]]` / `[[iftags]]` / link / resource
/// resolution. The gear is keyed by the canonical address `(space, local)` —
/// exactly what the URL and the client subscription name. Metadata is fetched
/// lazily, one site at a time: the page's own site first, then any site a
/// cross-site include actually names. Page bodies are read directly from the
/// immutable archives named by that metadata. An unregistered space or
/// unknown local id yields an empty view (the HTTP layer turns that into a
/// 404).
pub(crate) async fn article_latest<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _view: &mut S::View<'_>,
) -> ArticleView {
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return ArticleView::default();
    };
    let mut source = GearSource::new(ctx);
    let Some(meta) = source.meta(&site).await else {
        return ArticleView::default();
    };
    let Some(page) = latest(&meta, local) else {
        return ArticleView::default();
    };
    let body = source
        .body(&site, page)
        .await
        .unwrap_or_else(|| Arc::from(""));
    // A missing page parses to an empty view (blank slug) — the 404 shape.
    let Some(slug) = parse_slug(&page.slug) else {
        return ArticleView::default();
    };
    let host = HostCtx {
        fullname: page.slug.clone(),
        category: slug.0.as_ref().map(|c| (**c).clone()),
        tags: page.tags.clone(),
    };
    let page_meta = page.article_meta();
    let revisions = page.revisions.clone();
    let mut resolver = Resolver::new(site, &mut source);
    let (mut content, deps) = resolve_full(&mut resolver, body, slug, host).await;
    kolorinko_wikitext::assign_toc_anchors(&mut content);
    ArticleView {
        meta: page_meta,
        revisions,
        content,
        deps,
    }
}

// =========================================================================
// Resolution source + cache
// =========================================================================

/// The data a resolution run needs beyond its cache: one site's metadata and
/// one page's latest body. Implemented by the live gear (metadata through
/// `Repo`, bodies through the publication's immutable archives) and by the
/// tests' pinned fixture tree.
#[allow(async_fn_in_trait)]
pub(super) trait PageSource {
    /// One site's metadata, fetched on first use and cached by the source.
    async fn meta(&mut self, site: &SafePathComponent) -> Option<RepoMeta>;

    /// One page's raw latest body, read from its archive.
    async fn body(&mut self, site: &SafePathComponent, page: &PageMeta) -> Option<Arc<str>>;
}

/// The live source: `Repo` metadata through the gear graph, bodies from the
/// publication directory the process globals name.
pub(super) struct GearSource<'a, S: IsStorage<KolorinkoRT>> {
    ctx: &'a mut GearCtx<KolorinkoRT, S>,
    metas: HashMap<SafePathComponent, RepoMeta>,
}

impl<'a, S: IsStorage<KolorinkoRT>> GearSource<'a, S> {
    pub(super) fn new(ctx: &'a mut GearCtx<KolorinkoRT, S>) -> Self {
        Self {
            ctx,
            metas: HashMap::new(),
        }
    }
}

impl<S: IsStorage<KolorinkoRT>> PageSource for GearSource<'_, S> {
    async fn meta(&mut self, site: &SafePathComponent) -> Option<RepoMeta> {
        if let Some(meta) = self.metas.get(site) {
            return Some(meta.clone());
        }
        let meta = (*crate::runtime::repo(site.clone())
            .secondary_get(&*self.ctx)
            .await)
            .clone();
        self.metas.insert(site.clone(), meta.clone());
        Some(meta)
    }

    async fn body(&mut self, site: &SafePathComponent, page: &PageMeta) -> Option<Arc<str>> {
        read_latest_body(site, page).await
    }
}

/// Per-run memos. Nothing here is authoritative state: each entry is a lookup
/// the resolver could repeat (a page body already read, a `%%content%%`
/// transclusion already resolved). The include visited set is local to
/// [`resolve_include`]; `transcluded` is the transclusion visited set, which
/// must span the whole run to break cycles.
#[derive(Default)]
pub(super) struct ResolveCache {
    pub(super) raws: HashMap<Key, Arc<str>>,
    pub(super) bodies: HashMap<String, Content>,
    pub(super) transcluded: HashSet<Key>,
}

/// One resolution run: the site being rendered, its [`PageSource`], and the
/// run's [`ResolveCache`]. Everything the resolver needs is passed
/// explicitly — there is no process-global resolution state.
pub(super) struct Resolver<'a, S: PageSource> {
    pub(super) site: SafePathComponent,
    source: &'a mut S,
    pub(super) cache: ResolveCache,
}

impl<'a, S: PageSource> Resolver<'a, S> {
    pub(super) fn new(site: SafePathComponent, source: &'a mut S) -> Self {
        Self {
            site,
            source,
            cache: ResolveCache::default(),
        }
    }

    /// The metadata of the site being resolved (or a named cross-site
    /// include target), fetched lazily and memoised by the source.
    pub(super) async fn meta(&mut self, site: &SafePathComponent) -> Option<RepoMeta> {
        self.source.meta(site).await
    }

    /// One page's raw latest body, fetched once per run and cached.
    pub(super) async fn body(&mut self, site: &SafePathComponent, slug: &Slug) -> Option<Arc<str>> {
        let key = (site.clone(), slug.0.clone(), slug.1.clone());
        if let Some(body) = self.cache.raws.get(&key) {
            return Some(Arc::clone(body));
        }
        let page = article(&self.meta(site).await?, slug)?.clone();
        let body = self.source.body(site, &page).await?;
        self.cache.raws.insert(key, Arc::clone(&body));
        Some(body)
    }

    /// [`Resolver::body`] by canonical page id (a ListPages transclusion
    /// always names a page on the resolving site).
    pub(super) async fn body_by_id(
        &mut self,
        site: &SafePathComponent,
        page_id: u64,
    ) -> Option<Arc<str>> {
        let slug = {
            let meta = self.meta(site).await?;
            parse_slug(&meta.pages.get(&page_id)?.slug)?
        };
        self.body(site, &slug).await
    }
}

/// The full resolution pipeline — textual `[[include]]` assembly and the
/// parse of its result, `[[module ListPages]]`, `[[iftags]]`, internal
/// links, mirrored resources — run on `body` in the context of the hosting
/// page (`site`, `slug`, `host`), returning the resolved content together
/// with its dependency tree: the include cone plus every listed page fetched
/// for a `%%content%%` transclusion. Shared between the gear's own resolution
/// and the recursive resolution of a `%%content%%` transclusion, all through
/// one [`Resolver`].
///
/// Boxed because the pipeline recurses (`%%content%%` transclusion can reach
/// another ListPages); the indirection is the recursion's type eraser.
pub(super) fn resolve_full<'a, 's: 'a, S: PageSource + 'a>(
    resolver: &'a mut Resolver<'s, S>,
    body: Arc<str>,
    slug: Slug,
    host: HostCtx,
) -> Pin<Box<dyn Future<Output = (Content, Vec<PageDep>)> + 'a>> {
    Box::pin(async move {
        let site = resolver.site.clone();
        let origin = (site.clone(), slug.0.clone(), slug.1.clone());
        resolver.cache.transcluded.insert(origin.clone());
        // A data-form page renders through its category's `_template` layout
        // (the form fields substituted) — before anything textual runs, so the
        // expansion's own includes and directives resolve like page text.
        let (expanded, form_deps) = expand_form(&body, &origin, &slug, resolver).await;
        let (assembled, mut deps) = resolve_include(&expanded, &origin, resolver).await;
        deps.extend(form_deps);
        let (content, listed) = resolve_listpages(parse(&assembled), resolver, &host).await;
        deps.extend(listed);
        let content = evaluate_iftags(content, &host.tags);
        let content = rewrite_own_page_links(content, &site);
        let meta = resolver.meta(&site).await;
        let content = resolve_links(content, meta.as_ref());
        let content = resolve_resources(content, &site, meta.as_ref());
        (content, deps)
    })
}

/// Resolve every `[[include]]` directive anywhere inside the raw `body` of
/// `origin`: first the whole include cone is fetched breadth-first — each
/// body a metadata lookup plus a direct archive read, cached in
/// `resolver.cache.raws`, so a diamond A→B→D, A→C→D fetches D once — then
/// the body is assembled in a single recursive pass ([`splice_includes`])
/// that breaks data-level cycles (A includes B includes A) by tracking the
/// recursion path. Returns the assembled text with the dependency tree: one
/// node per fetched page, nested under the page whose body first included it.
pub(super) async fn resolve_include<S: PageSource>(
    body: &str,
    origin: &Key,
    resolver: &mut Resolver<'_, S>,
) -> (String, Vec<PageDep>) {
    let mut edges: Vec<(Key, Key)> = Vec::new();
    // The include cone's visited set is local to this pass: a page cached by
    // an earlier lookup (a form template, another transclusion) must still be
    // walked here for its own directives and dep edges.
    let mut visited: HashSet<Key> = HashSet::from([origin.clone()]);
    let mut queue: VecDeque<(Key, Arc<str>)> = VecDeque::from([(origin.clone(), Arc::from(body))]);
    while let Some((includer, text)) = queue.pop_front() {
        for d in live_directives(&text) {
            let Some((inc_site, inc_slug)) = d.target(&resolver.site) else {
                continue;
            };
            let key = (inc_site, inc_slug.0.clone(), inc_slug.1.clone());
            if !visited.insert(key.clone()) {
                continue;
            }
            let content = resolver
                .body(&key.0, &inc_slug)
                .await
                .unwrap_or_else(|| Arc::from(""));
            edges.push((includer.clone(), key.clone()));
            queue.push_back((key, content));
        }
    }
    (
        splice_includes(
            &subst_vars(body, &[]),
            &resolver.site,
            &resolver.cache.raws,
            std::slice::from_ref(origin),
        ),
        dep_tree(origin, edges),
    )
}
