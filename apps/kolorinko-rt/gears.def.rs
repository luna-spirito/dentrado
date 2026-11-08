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
//   field-for-field `From<wire::GearId>` id bridge.
// - `#[gears_schema(file = "...")]` (in this crate's `lib.rs`) emits a
//   wasm-safe, dentrado-free wire schema (serde `GearId` / `GearOut` /
//   `GearQuery`) for the client. It reads only the signatures; the bodies are
//   dropped.
//
// Gear identity is purely content addressing, and the *client-facing* gears
// are addressed by the canonical URL identity (`space`, `local`): what a URL
// names is exactly what a subscription names. The export repo (url, dir,
// interval) lives in the process-global config ([`crate::globals`]).
//
// The publication is read as one immutable generation per site. Each site's
// [`Repo`] gear decodes the generation's `pages`/`files`/`shell` sections and
// publishes [`RepoMeta`] — paths and metadata, never page bodies — as a
// `shared` value. Every consumer reads the site's metadata through one
// `secondary_get` per gear run, then reads the actual page archive or blob
// directly from the immutable object store:
//
//   Repo { site } (timer oracle, shared; one per site)
//     → ArticleLatestParsed (event; metadata read + archive body read/parse)
//     → ArticleLatest (follows the parse; metadata for every configured site
//       plus archive body reads resolve includes/ListPages/links/resources)
//     → CodeBlock (follows the same parse: a lens over its output)
//     → Shell (event; site metadata + resolved nav pages)
//
// `asset` stays site/hash-keyed and reads blobs lazily from the
// publication's content-addressed `files_ca/` store; the reverse index's
// recorded type decides the served MIME and the CSS-rewrite branch (the
// request URL's extension is decorative). Wire exposure is an explicit
// **allowlist**: only `#[gear(exposed)]` gears appear in the wire `GearId`
// and can be pushed to a client — everything else is server-internal by
// default (unnamed on the wire, and its output variants gated by the
// generated `GearOut::is_exposed` filter the push bridge applies).

/// One site's immutable publication metadata. A timer oracle per site: a
/// tick re-reads the `HEAD` generation, reusing the prior output's revision
/// frontmatter for unchanged archive hashes. The output is `shared`, so every
/// core holds one reference to the same [`RepoMeta`].
#[dentrado::gear(timer(period = crate::globals::interval()), shared, name = Repo)]
pub(crate) async fn repo<S: IsStorage<KolorinkoRT>>(
    site: SafePathComponent,
    tick: bool,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
) -> RepoMeta {
    crate::wikidot_page::repo(site, tick, ctx, view).await
}

#[dentrado::gear(event, shared, name = Asset)]
pub(crate) async fn asset<S: IsStorage<KolorinkoRT>>(
    site: SafePathComponent,
    hash: String,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _view: &mut S::View<'_>,
) -> Option<ServedBlob> {
    crate::wikidot_page::asset(&site, &hash, ctx).await
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
pub(crate) fn code_block<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    n: u32,
    parsed: &ArticleView,
    _view: &mut S::View<'_>,
) -> Option<CodeBlock> {
    crate::wikidot_page::code_block(space, local, n, parsed)
}

/// The whole site chrome in one shot: title/subtitle, the theme-root URL, the
/// landing page's canonical address, and the resolved `nav:top` / `nav:side`
/// pages. Reads the site's [`Repo`] metadata and resolves each nav page
/// through an `article_latest` dependency.
#[dentrado::gear(event, shared, exposed, name = Shell)]
pub(crate) async fn shell<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    _view: &mut S::View<'_>,
) -> SiteShell {
    crate::wikidot_page::shell(space, ctx).await
}

// Parses a page's latest body (read directly from the immutable archive named
// by its site's `Repo` metadata) into an unresolved `ArticleView`. Keyed by
// the canonical address, so it lives on its own core — off `Repo`'s — and its
// output is what `article_latest` / `code_block` statically follow.
#[dentrado::gear(event, shared, name = ArticleLatestParsed)]
pub(crate) async fn article_latest_parsed<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
) -> ArticleView {
    crate::wikidot_page::article_latest_parsed(space, local, ctx, view).await
}

#[dentrado::gear(
    follow(target = GearId::ArticleLatestParsed { space, local }),
    shared,
    exposed,
    name = ArticleLatest,
)]
pub(crate) async fn article_latest<S: IsStorage<KolorinkoRT>>(
    space: SpaceId,
    local: LocalId,
    parsed: &ArticleView,
    ctx: &mut GearCtx<KolorinkoRT, S>,
    view: &mut S::View<'_>,
) -> ArticleView {
    crate::wikidot_page::article_latest(space, local, parsed, ctx, view).await
}
