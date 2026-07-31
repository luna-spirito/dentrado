use std::sync::Arc;

use crate::wikidot_page::{LoadCache, RepoCache, RepoData, RepoMeta};
use crate::{safe_path::SafePathComponent, wikidot_parser::types::Content};
use dentrado::core::{core_ctx::GearCtx, storage::Storage};

/// The Kolorinko runtime.
///
/// This file is the **gear hub**: it declares every gear as a thin forwarding
/// fn inside the `#[gears]` module below. The `#[gears]` aggregator reads each
/// `#[gear]`-marked fn's signature to derive the `GearId` / `GearOut` /
/// `GearCache` / `Group` enums, the `IsRuntime` impl, the `GlobalHash` impl for
/// `Group`, and the typed dependency accessors. The *implementations* live in
/// domain modules (`wikidot_page::repo`, `wikidot_page::load_page`, …) — so a
/// gear's signature is stated once here (as the wiring manifest) and the body
/// lives next to the helpers it calls.
///
/// Adding a gear: one more `#[gear]`-marked forwarding fn here, plus its impl in
/// the relevant domain module. Yes, the signature appears in both places —
/// that's the cost of a closed `GearId` enum with bodies spread across files.
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[dentrado::gears(runtime = KolorinkoRT)]
pub(crate) mod gears {
    use super::*;

    // `repo` is an oracle: it polls the remote git repository on a timer
    // (every `interval` seconds) and rebuilds the in-memory dataset.
    #[dentrado::gear(
        timer(
            period = std::num::NonZero::new(u64::from(repo_meta.interval()))
                .unwrap_or_else(|| std::num::NonZero::new(900).expect("900 != 0")),
        ),
        name = Repo,
    )]
    pub(crate) async fn repo(
        repo_meta: RepoMeta,
        tick: bool,
        cache: &mut RepoCache,
    ) -> Arc<RepoData> {
        crate::wikidot_page::repo(&repo_meta, tick, cache)
    }

    // `load` is event-driven: it runs on first activation and whenever its
    // `repo` dependency produces new output. A unique phantom group that
    // nothing ever posts to means only dependency kicks (and first activation)
    // ever run it.
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

pub(crate) use gears::{GearId, GearOut, dep_repo};
