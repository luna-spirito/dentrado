// The single source of truth for the kolorinko gears. This file is **macro
// input only** — it is never compiled directly. Two attribute macros read it:
//
// - `#[gears(file = "...", runtime = KolorinkoRT, wire = kolorinko_rt::wire)]`
//   (in the server's `runtime.rs`) emits the full dentrado runtime — the impl
//   fns below keep their bodies (the `crate::wikidot_page::…` calls resolve
//   there), and the gear names are freed for the generated `GearQuery`
//   builders. `wire = …` additionally aliases the wire schema's `GearOut` as
//   the runtime's `GearOut`/`GearOutShared` (one payload enum for both sides,
//   no per-variant relabel at the wire boundary) and generates the
//   field-for-field `From<wire::GearId>` id bridge.// - `#[gears_schema(file = "...")]` (in this crate's `lib.rs`) emits a
//   wasm-safe, dentrado-free wire schema (serde `GearId` / `GearOut` /
//   `GearQuery`) for the client. It reads only the signatures; the bodies are
//   dropped.
//
// Gear identity is purely content addressing, and the *client-facing* gears
// are addressed by the canonical URL identity (`space`, `local`): what a URL
// names is exactly what a subscription names. The export repo (url, dir,
// interval) lives in the process-global config ([`crate::globals`]) — the
// `repo` oracle below is a singleton with no id fields, its timer reads the
// interval from the globals.
//
// The page pipeline is split across cores: `repo` is a `local` oracle pinned
// to its own core, and every gear off that core reads it only through the
// `follow` lenses (co-located with `repo`, outputs shared across cores by
// reference). The parse/resolution gears are keyed by the canonical address,
// so each `follow` target is statically derivable from the follower's own
// id fields:
//
//   repo → repo_l_article_latest (lens over the dataset)
//        → article_latest_parsed (event; pulls the lens via `secondary_get`,
//          lives off the `repo` core)
//        → article_latest (follows `ArticleLatestParsed { space, local }`,
//          co-located with the parse)
//        → code_block (follows the same parse: a lens over its output)
//
// `repo_l_local_id` is the slug-family → canonical bridge (a legacy
// `(site, slug)` address to its `local` id), `repo_l_query_pages` its batched
// form — the whole (sorted, deduplicated) link set of one page resolved in a
// single lens read, so a thousand-link index page declares one dependency
// instead of a thousand — and `repo_l_list_pages` / `repo_resource` / `asset`
// stay slug/site-keyed. Wire exposure is an explicit **allowlist**: only
// `#[gear(exposed)]` gears appear in the wire `GearId` and can be pushed to
// a client — everything else is server-internal by default (unnamed on the
// wire, and its output variants gated by the generated `GearOut::is_exposed`
// filter the push bridge applies).

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
    space: SpaceId,
    local: LocalId,
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLArticleCache,
) -> ArticleLatest {
    crate::wikidot_page::repo_l_article_latest(&repo_data, space, local).await
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = RepoLLocalId,
)]
pub(crate) fn repo_l_local_id(
    site: SafePathComponent,
    slug: (Option<SafePathComponent>, SafePathComponent),
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLLocalIdCache,
) -> Option<(LocalId, String)> {
    crate::wikidot_page::repo_l_local_id(&repo_data, &site, &slug)
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    name = RepoLQueryPages,
)]
pub(crate) fn repo_l_query_pages(
    site: SafePathComponent,
    query: PageQuery,
    repo_data: Rc<RepoData>,
    _cache: &mut RepoLQueryPagesCache,
) -> PageQueryResult {
    crate::wikidot_page::repo_l_query_pages(&repo_data, &site, &query)
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

// A lens over the shared parse: statically bound to the `(space, local)`
// record of `article_latest_parsed` (the target is derived from this gear's
// own id), co-located with it, and handed its output as a `&ArticleView`
// borrow — no per-run dep reconciliation, no cross-core shipping of the
// whole tree just to extract one block. Re-runs exactly when the parse
// output changes (which itself re-runs only when the page body does).
#[dentrado::gear(
    follow(target = GearId::ArticleLatestParsed { space, local }),
    shared,
    name = CodeBlock,
)]
pub(crate) fn code_block(
    space: SpaceId,
    local: LocalId,
    n: u32,
    parsed: &ArticleView,
    _cache: &mut CodeBlockCache,
) -> Option<CodeBlock> {
    crate::wikidot_page::code_block(space, local, n, parsed)
}

#[dentrado::gear(
    follow(target = GearId::Repo {}),
    shared,
    exposed,
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

// Parses a page's latest body (fetched through the `repo_l_article_latest`
// lens via `secondary_get`) into an unresolved `ArticleView`. Keyed by the
// canonical address, so it lives on its own core — off `repo`'s — and its
// output is what `article_latest` / `code_block` statically follow.
#[dentrado::gear(event, shared, name = ArticleLatestParsed)]
pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut ParsedCache,
) -> ArticleView {
    crate::wikidot_page::article_latest_parsed(space, local, ctx, cache).await
}

#[dentrado::gear(
    follow(target = GearId::ArticleLatestParsed { space, local }),
    shared,
    exposed,
    name = ArticleLatest,
)]
pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    cache: &mut LatestCache,
) -> ArticleView {
    crate::wikidot_page::article_latest(space, local, parsed, ctx, cache).await
}
