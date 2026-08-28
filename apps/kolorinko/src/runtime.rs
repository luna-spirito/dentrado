use std::rc::Rc;

use crate::wikidot_page::{
    AssetCache, CodeBlockCache, LatestCache, ParsedCache, RepoCache, RepoData, RepoLArticleCache,
    RepoLListPagesCache, RepoLLocalIdCache, RepoLQueryPagesCache, RepoResourceCache, ShellCache,
};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use kolorinko_rt::{
    Body, CaRef, CodeBlock, ListPagesQuery, ListPagesResult, LocalId, PageQuery, PageQueryResult,
    RepoAssetPath, SafePathComponent, SiteShell, SpaceId,
};
use kolorinko_wikitext::{ArticleLatest, ArticleView};

/// The Kolorinko runtime.
///
/// This file is now a thin **skeleton**: the gear declarations live in
/// [`kolorinko_rt`]'s `gears.def.rs`, read by *two* macros so each gear is
/// declared exactly once — this `#[gears]` (the native dentrado runtime: impl
/// fns + generated `GearId`/`GearOut`/`GearCache`/`IsRuntime`/`GearQuery`
/// builders) and the client's `#[gears_schema]` (the wasm wire schema). The
/// impl fn bodies (`crate::wikidot_page::…`) resolve here; the implementations
/// themselves live in [`wikidot_page`].
///
/// `wire = kolorinko_rt::wire` deduplicates the payload layer too: the wire
/// schema's `GearOut` is aliased as this runtime's `GearOut`/`GearOutShared`
/// (both generated from the one gear file, so the variants agree by
/// construction), and a generated `From<wire::GearId>` bridges the ids — the
/// runtime id is the strict superset carrying the `local`/`internal` gears.
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[dentrado::gears(
    runtime = KolorinkoRT,
    file = "../kolorinko-rt/gears.def.rs",
    wire = kolorinko_rt::wire
)]
mod gears {
    use super::*;
}

// `GearId` stays internal: gear ids are constructed only through the generated
// `GearQuery` builders — the one exception is the generated wire→runtime
// `From`, which a client-supplied wire id crosses at the subscription
// boundary. `GearOutShared` (an alias of `kolorinko_rt::wire::GearOut`) and
// the builders are re-exported for the server's dispatch/stale-read code.
pub(crate) use gears::{
    GearOutShared, article_latest, asset, code_block, repo_l_article_latest, repo_l_list_pages,
    repo_l_local_id, repo_l_query_pages, repo_resource, shell,
};
