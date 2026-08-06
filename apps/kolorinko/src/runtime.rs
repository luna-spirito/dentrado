use std::rc::Rc;

use crate::wikidot_page::{
    ArticleLatest, LatestCache, ParsedCache, RepoCache, RepoData, RepoLArticleCache,
};
use crate::{safe_path::SafePathComponent, wikidot_page::RepoMeta};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};
use kolorinko_wikitext::ArticleView;

/// The Kolorinko runtime.
///
/// This file is the **gear hub**: it declares every gear as a thin forwarding
/// fn inside the `#[gears]` module below. The `#[gears]` aggregator reads each
/// `#[gear]`-marked fn's signature to derive the `GearId` / `GearOut` /
/// `GearCache` / `Group` enums, the `IsRuntime` impl, the `GlobalHash` impl for
/// `Group`, and the per-gear `GearQuery` builders (the typed dependency
/// layer). The *implementations* live in the domain module [`wikidot_page`].
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[dentrado::gears(runtime = KolorinkoRT)]
mod gears {
    use super::*;

    // `repo` is a `local` oracle: it polls the remote git repository on a timer
    // and rebuilds the in-memory dataset. `local` pins the whole `Rc<RepoData>`
    // to this gear's owning core (it never crosses a thread, so it may freely
    // hold `Rc`/`!Send` data); it is read only through the `repo_l_article`
    // follow lens below, co-located on the same core.
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
    ) -> Rc<RepoData> {
        crate::wikidot_page::repo(&repo_meta, tick, cache)
    }

    // `repo_l_article_latest` is a *lens* over the local `repo` oracle: a
    // `follow` gear is co-located with its target (same core), so it reads
    // `repo`'s local `Rc<RepoData>` and projects one page into a shippable,
    // owned `ArticleLatest` (metadata + latest body + revision list, no bodies).
    #[dentrado::gear(follow(target = GearId::Repo(_repo_meta)), name = RepoLArticleLatest)]
    pub(crate) async fn repo_l_article_latest(
        _repo_meta: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent),
        repo_data: Rc<RepoData>,
        _cache: &mut RepoLArticleCache,
    ) -> ArticleLatest {
        crate::wikidot_page::repo_l_article_latest(&repo_data, &site, &slug)
    }

    // `article_latest_parsed` parses a page's latest body into an `ArticleView`
    // with `[[include]]` directives **left unresolved**. It depends only on the
    // lens, never on another parse gear — that acyclicity is what lets
    // `article_latest` resolve includes against parse gears without forming a
    // gear-level cycle between two pages that include each other.
    #[dentrado::gear(event, name = ArticleLatestParsed)]
    pub(crate) async fn article_latest_parsed<S: Storage<KolorinkoRT>>(
        repo_meta: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent),
        ctx: &mut GearCtx<KolorinkoRT, S>,
        cache: &mut ParsedCache,
    ) -> ArticleView {
        crate::wikidot_page::article_latest_parsed(&repo_meta, &site, &slug, ctx, cache).await
    }

    // `article_latest` resolves every `[[include]]` and emits the final
    // `ArticleView`. It **follows** its own page's `article_latest_parsed`
    // (co-located: the own-page parse is read locally, with no cross-core hop)
    // and `secondary_get`s the included pages' parse gears (each declared as a
    // dependency, so the result is reactive to edits anywhere in the
    // transitive include cone).
    #[dentrado::gear(follow(target = GearId::ArticleLatestParsed { repo_meta, site, slug }), name = ArticleLatest)]
    pub(crate) async fn article_latest<S: Storage<KolorinkoRT>>(
        repo_meta: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent),
        parsed: ArticleView,
        ctx: &mut GearCtx<KolorinkoRT, S>,
        cache: &mut LatestCache,
    ) -> ArticleView {
        crate::wikidot_page::article_latest(&repo_meta, site, slug, parsed, ctx, cache).await
    }
}

// `GearId` stays internal: gear ids are constructed only through the generated
// `GearQuery` builders. `GearOut` is re-exported so the server can match on
// subscription results.
pub(crate) use gears::{GearOut, article_latest, article_latest_parsed, repo_l_article_latest};
