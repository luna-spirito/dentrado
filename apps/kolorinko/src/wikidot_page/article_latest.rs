use super::*;
use std::collections::VecDeque;

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
    let body = read_latest_body(&site, page).unwrap_or_else(|| Arc::from(""));
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
/// exactly what the URL and the client subscription name — and follows the
/// page's own parse ([`article_latest_parsed`], co-located off the `repo`
/// core). Every configured site's [`RepoMeta`] is fetched through one
/// `secondary_get` per run (cross-site includes need it), and page bodies are
/// read directly from the immutable archives named by that metadata.
/// An unregistered space or unknown local id yields an empty view (the HTTP
/// layer turns that into a 404).
pub(crate) async fn article_latest<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    _parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _view: &mut S::View<'_>,
) -> ArticleView {
    let Some(site) = crate::globals::site_of(&space).cloned() else {
        return ArticleView::default();
    };
    let mut metas = HashMap::new();
    for configured in crate::globals::sites() {
        let meta = crate::runtime::repo(configured.clone())
            .secondary_get(ctx)
            .await;
        metas.insert(configured.clone(), (*meta).clone());
    }
    let Some(meta) = metas.get(&site) else {
        return ArticleView::default();
    };
    let Some(page) = latest(meta, local) else {
        return ArticleView::default();
    };
    let page_meta = page.article_meta();
    let revisions = page.revisions.clone();
    let body = read_latest_body(&site, page).unwrap_or_else(|| Arc::from(""));
    // A missing page parses to an empty view (blank slug) — the 404 shape.
    let Some(slug) = parse_slug(&page.slug) else {
        return ArticleView::default();
    };
    let host = HostCtx {
        fullname: page.slug.clone(),
        category: slug.0.as_ref().map(|c| (**c).clone()),
        tags: page.tags.clone(),
    };
    let mut state = ResolveState::new(site, metas);
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
/// transclusion it recurses into: the dataset site being resolved, the
/// metadata of every configured site (one [`RepoMeta`] per site, cloned once
/// per run), the raw body of every fetched page (fetch once, splice wherever a
/// directive or a transclusion re-encounters it), the resolved `%%content%%`
/// bodies keyed by fullname, and the pages whose resolution has already run,
/// which is what stops transclusion cycles.
pub(super) struct ResolveState {
    pub(super) site: SafePathComponent,
    pub(super) metas: HashMap<SafePathComponent, RepoMeta>,
    pub(super) raws: HashMap<Key, Arc<str>>,
    pub(super) bodies: HashMap<String, Content>,
    pub(super) resolved: HashSet<Key>,
    /// The directory holding every site's publication (`out/`), when the run
    /// resolves outside the process globals — tests pinning a fixture tree.
    pub(super) out_root: Option<PathBuf>,
}

impl ResolveState {
    pub(super) fn new(
        site: SafePathComponent,
        metas: HashMap<SafePathComponent, RepoMeta>,
    ) -> Self {
        Self {
            site,
            metas,
            raws: HashMap::new(),
            bodies: HashMap::new(),
            resolved: HashSet::new(),
            out_root: None,
        }
    }

    /// The metadata of the site being resolved (or a named cross-site
    /// include target).
    pub(super) fn meta(&self, site: &SafePathComponent) -> Option<&RepoMeta> {
        self.metas.get(site)
    }

    /// One page's current metadata on a site.
    pub(super) fn page(&self, site: &SafePathComponent, slug: &Slug) -> Option<&PageMeta> {
        article(self.meta(site)?, slug)
    }

    /// One page's raw latest body, read once per run and cached. Tests that
    /// exercise textual assembly pre-populate `raws`, so no publication is
    /// needed.
    pub(super) fn body(&mut self, site: &SafePathComponent, slug: &Slug) -> Option<Arc<str>> {
        let key = (site.clone(), slug.0.clone(), slug.1.clone());
        if let Some(body) = self.raws.get(&key) {
            return Some(Arc::clone(body));
        }
        let page = self.page(site, slug)?.clone();
        let body = match &self.out_root {
            Some(root) => read_latest_body_at(&root.join(&**site), &page)?,
            None => read_latest_body(site, &page)?,
        };
        self.raws.insert(key, Arc::clone(&body));
        Some(body)
    }

    /// [`ResolveState::body`] by canonical page id (a ListPages transclusion
    /// always names a page on the resolving site).
    pub(super) fn body_by_id(
        &mut self,
        site: &SafePathComponent,
        page_id: u64,
    ) -> Option<Arc<str>> {
        let slug = {
            let page = self.meta(site)?.pages.get(&page_id)?;
            parse_slug(&page.slug)?
        };
        self.body(site, &slug)
    }
}

/// The full resolution pipeline — textual `[[include]]` assembly and the
/// parse of its result, `[[module ListPages]]`, `[[iftags]]`, internal
/// links, mirrored resources — run on `body` in the context of the hosting
/// page (`site`, `slug`, `host`), returning the resolved content together
/// with its dependency tree: the include cone plus every listed page fetched
/// for a `%%content%%` transclusion. Shared between the gear's own resolution
/// and the recursive resolution of a `%%content%%` transclusion, all through
/// one [`ResolveState`].
pub(super) fn resolve_full(
    body: Arc<str>,
    slug: Slug,
    host: HostCtx,
    state: &mut ResolveState,
) -> (Content, Vec<PageDep>) {
    let origin = (state.site.clone(), slug.0.clone(), slug.1.clone());
    state.resolved.insert(origin.clone());
    // A data-form page renders through its category's `_template` layout
    // (the form fields substituted) — before anything textual runs, so the
    // expansion's own includes and directives resolve like page text.
    let (expanded, form_deps) = expand_form(&body, &origin, &slug, state);
    let (assembled, mut deps) = resolve_include(&expanded, &origin, state);
    deps.extend(form_deps);
    let (content, listed) = resolve_listpages(parse(&assembled), state, &host);
    deps.extend(listed);
    let content = evaluate_iftags(content, &host.tags);
    let content = rewrite_own_page_links(content, &state.site);
    let content = resolve_links(content, state.meta(&state.site));
    let content = resolve_resources(content, &state.site, state.meta(&state.site));
    (content, deps)
}

/// Resolve every `[[include]]` directive anywhere inside the raw `body` of
/// `origin`: first the whole include cone is fetched breadth-first — each
/// body a metadata lookup plus a direct archive read, cached in
/// `state.raws`, so a diamond A→B→D, A→C→D fetches D once — then the body is
/// assembled in a single recursive pass ([`splice_includes`]) that breaks
/// data-level cycles (A includes B includes A) by tracking the recursion
/// path. Returns the assembled text with the dependency tree: one node per
/// fetched page, nested under the page whose body first included it.
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
            let content = state
                .body(&key.0, &inc_slug)
                .unwrap_or_else(|| Arc::from(""));
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
