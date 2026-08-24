// The single source of truth for the kolorinko gears. This file is **macro
// input only** — it is never compiled directly. Two attribute macros read it:
//
// - `#[gears(file = "...", runtime = KolorinkoRT)]` (in the server's
//   `runtime.rs`) emits the full dentrado runtime — the impl fns below keep
//   their bodies (the `crate::wikidot_page::…` calls resolve there), and the
//   gear names are freed for the generated `GearQuery` builders.
// - `#[gears_schema(file = "...")]` (in this crate's `lib.rs`) emits a
//   wasm-safe, dentrado-free wire schema (serde `GearId` / `GearOut` /
//   `GearQuery`) for the client. It reads only the signatures; the bodies are
//   dropped.
//
// Gear identity is purely content addressing, and the *client-facing* gears
// are addressed by the canonical URL identity (`space`, `local`): what a URL
// names is exactly what a subscription names. The export repo (url, dir,
// interval) lives in the process-global config ([`crate::globals`]) — the
// `repo` oracle below is a singleton with no id fields, its timer reads the
// interval from the globals. The slug-keyed gears (`repo_l_article_latest`,
// `article_latest_parsed`, …) are the server-internal resolution cone: they
// never appear in a client subscription (the wire schema carries them only
// because the schema is generated from this one file).

#[dentrado::gear(timer(period = crate::globals::interval()), local, name = Repo)]
pub(crate) async fn repo(tick: bool, cache: &mut RepoCache) -> Rc<RepoData> {
    crate::wikidot_page::repo(crate::globals::repo(), tick, cache).await
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = RepoLArticleLatest,
)]
pub(crate) async fn repo_l_article_latest(
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLArticleCache,
) -> ArticleLatest {
    crate::wikidot_page::repo_l_article_latest(&repo_data, &site, &slug).await
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = RepoLListPages,
)]
pub(crate) fn repo_l_list_pages(
    site: SafePathComponent,
    query: ListPagesQuery,
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLListPagesCache,
) -> ListPagesResult {
    crate::wikidot_page::repo_l_list_pages(&repo_data, &site, &query)
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = RepoResource,
)]
pub(crate) fn repo_resource(
    site: SafePathComponent,
    path: RepoAssetPath,
    repo_data: Rc<RepoData>,
    _cache: &mut RepoResourceCache,
) -> Option<CaRef> {
    crate::wikidot_page::repo_resource(&repo_data, &site, &path)
}

#[dentrado::gear(event, shared, name = Asset)]
pub(crate) async fn asset<S: Storage<KolorinkoRT>>(
    site: SafePathComponent,
    hash: String,
    ext: String,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut AssetCache,
) -> Option<Body> {
    crate::wikidot_page::asset(&site, &hash, &ext, ctx).await
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = Shell,
)]
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    repo_data: Rc<RepoData>,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut ShellCache,
) -> SiteShell {
    crate::wikidot_page::shell(&repo_data, space, ctx).await
}

#[dentrado::gear(event, shared, name = ArticleLatestParsed)]
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    crate::wikidot_page::article_latest_parsed(&site, &slug, ctx, cache).await
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = ArticleLatest,
)]
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    repo_data: Rc<RepoData>,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut LatestCache,
) -> ArticleView {
    crate::wikidot_page::article_latest(space, local, &repo_data, ctx, cache).await
}
