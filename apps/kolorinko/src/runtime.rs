use std::rc::Rc;

use crate::wikidot_page::{
    LatestCache, ParsedCache, RepoAssetCache, RepoCache, RepoData, RepoLArticleCache,
    RepoLThemeRootsCache, RepoMeta,
};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use kolorinko_rt::{AssetKind, RepoAssetOut, RepoAssetPath, SafePathComponent};
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
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[dentrado::gears(runtime = KolorinkoRT, file = "../kolorinko-rt/gears.def.rs")]
mod gears {
    use super::*;
}

// `GearId` stays internal: gear ids are constructed only through the generated
// `GearQuery` builders. `GearOut` and the builders are re-exported for the
// server's subscription/dispatch code.
pub(crate) use gears::{
    GearOut, GearOutShared, article_latest, article_latest_parsed, repo_asset,
    repo_l_article_latest, repo_l_theme_roots,
};
