use std::sync::Arc;

use crate::wikidot_page::{LoadCache, RepoCache, RepoData, RepoLArticleCache, RepoMeta};
use crate::{safe_path::SafePathComponent, wikidot_parser::types::Content};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};

/// The Kolorinko runtime.
///
/// This file is the **gear hub**: it declares every gear as a thin forwarding
/// fn inside the `#[gears]` module below. The `#[gears]` aggregator reads each
/// `#[gear]`-marked fn's signature to derive the `GearId` / `GearOut` /
/// `GearCache` / `Group` enums, the `IsRuntime` impl, the `GlobalHash` impl for
/// `Group`, and the per-gear `GearQuery` builders (the typed dependency
/// layer). The *implementations* live in
/// domain modules (`wikidot_page::repo`, `wikidot_page::load_page`, …) — so a
/// gear's signature is stated once here (as the wiring manifest) and the body
/// lives next to the helpers it calls.
///
/// Adding a gear: one more `#[gear]`-marked forwarding fn here, plus its impl in
/// the relevant domain module. Yes, the signature appears in both places —
/// that's the cost of a closed `GearId` enum with bodies spread across files.
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

// The `gears` module itself is private: its generated enums (`GearId`,
// `GearOutLocal`, …) are internal wiring details. Only the typed `GearQuery`
// builders below are surfaced.
#[dentrado::gears(runtime = KolorinkoRT)]
mod gears {
    use super::*;

    // `repo` is an oracle: it polls the remote git repository on a timer
    // (every `interval` seconds) and rebuilds the in-memory dataset. `local` —
    // the whole `Arc<RepoData>` is pinned to this gear's core (it holds
    // `im::HashMap`s, and shipping a snapshot per request would be wasteful);
    // it is read only through the `repo_l_article` lens below, which lives on
    // the same core via `follow`.
    #[dentrado::gear(
        timer(
            period = std::num::NonZero::new(u64::from(repo_meta.interval()))
                .unwrap_or_else(|| std::num::NonZero::new(900).expect("900 != 0")),
        ),
        local,
        name = Repo,
    )]
    pub(crate) async fn repo(
        repo_meta: RepoMeta,
        tick: bool,
        cache: &mut RepoCache,
    ) -> Arc<RepoData> {
        crate::wikidot_page::repo(&repo_meta, tick, cache)
    }

    // `repo_l_article` is a *lens* over the local `repo` oracle: a `follow`
    // gear is co-located with its target (same core), so it can read `repo`'s
    // `GearResult::Local(RepoOut)` and project the *raw body text of the page's
    // latest revision* into a shippable `Arc<str>`. That is the cross-core
    // bridge: `load` (which may live on any core) reads the text via
    // `secondary_get`.
    #[dentrado::gear(follow(target = GearId::Repo(_repo_meta)), name = RepoLArticle)]
    pub(crate) async fn repo_l_article(
        _repo_meta: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent),
        repo_data: Arc<RepoData>,
        cache: &mut RepoLArticleCache,
    ) -> Arc<str> {
        crate::wikidot_page::repo_l_article(&repo_data, &site, &slug, cache)
    }

    // `load` is event-driven: it runs on first activation and whenever its
    // `repo_l_article` dependency produces new output. A unique phantom group
    // that nothing ever posts to means only dependency kicks (and first
    // activation) ever run it.
    #[dentrado::gear(event, name = Load)]
    pub(crate) async fn load<S: Storage<KolorinkoRT>>(
        repo: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent),
        ctx: &mut GearCtx<KolorinkoRT, S>,
        cache: &mut LoadCache,
    ) -> Arc<Content> {
        crate::wikidot_page::load_page(&repo, &site, &slug, ctx, cache).await
    }
}

// `GearId` is deliberately not re-exported: it is an internal detail, and gear
// ids are constructed only through the generated `GearQuery` builders (`load`,
// `repo_l_article`). `GearOut` stays exposed for pattern-matching subscription
// results.
pub(crate) use gears::{GearOut, load, repo_l_article};
