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
// `wire_skip(repo_meta)` marks the server-config id field the client never
// supplies: it stays a real id field on the server but is absent from the wire
// `GearId` (the server injects its configured `repo_meta` when dispatching).

#[dentrado::gear(
    timer(
        period = std::num::NonZero::new(u64::from(repo_meta.interval()))
            .unwrap_or_else(|| std::num::NonZero::new(900).expect("900 != 0")),
    ),
    local,
    name = Repo,
)]
pub(crate) async fn repo(repo_meta: RepoMeta, tick: bool, cache: &mut RepoCache) -> Rc<RepoData> {
    crate::wikidot_page::repo(&repo_meta, tick, cache).await
}

#[dentrado::gear(
    follow(target = GearId::Repo(_repo_meta)),
    shared,
    name = RepoLArticleLatest,
    wire_skip(_repo_meta),
)]
pub(crate) async fn repo_l_article_latest(
    _repo_meta: RepoMeta,
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLArticleCache,
) -> ArticleLatest {
    crate::wikidot_page::repo_l_article_latest(&repo_data, &site, &slug).await
}

#[dentrado::gear(
    follow(target = GearId::Repo(_repo_meta)),
    shared,
    name = RepoResource,
    wire_skip(_repo_meta),
)]
pub(crate) fn repo_resource(
    _repo_meta: RepoMeta,
    site: SafePathComponent,
    path: RepoAssetPath,
    repo_data: Rc<RepoData>,
    _cache: &mut RepoResourceCache,
) -> Option<CaRef> {
    crate::wikidot_page::repo_resource(&repo_data, &site, &path)
}

#[dentrado::gear(
    event,
    shared,
    name = Asset,
    wire_skip(repo_meta),
)]
pub(crate) async fn asset<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    site: SafePathComponent,
    hash: String,
    ext: String,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut AssetCache,
) -> Option<Body> {
    crate::wikidot_page::asset(&repo_meta, &site, &hash, &ext, ctx).await
}

#[dentrado::gear(
    follow(target = GearId::Repo(repo_meta)),
    shared,
    name = Shell,
    wire_skip(repo_meta),
)]
pub(crate) async fn shell<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    site: SafePathComponent,
    repo_data: Rc<RepoData>,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _cache: &mut ShellCache,
) -> SiteShell {
    crate::wikidot_page::shell(repo_meta, &repo_data, site, ctx).await
}

#[dentrado::gear(
    event,
    shared,
    name = ArticleLatestParsed,
    wire_skip(repo_meta),
)]
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    crate::wikidot_page::article_latest_parsed(&repo_meta, &site, &slug, ctx, cache).await
}

#[dentrado::gear(
    follow(target = GearId::ArticleLatestParsed { repo_meta, site, slug }),
    shared,
    name = ArticleLatest,
    wire_skip(repo_meta),
)]
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    repo_meta: RepoMeta,
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut LatestCache,
) -> ArticleView {
    crate::wikidot_page::article_latest(&repo_meta, site, slug, parsed, ctx, cache).await
}
