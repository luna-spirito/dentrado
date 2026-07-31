use std::{
    any::Any,
    fmt::Debug,
    hash::{DefaultHasher, Hash, Hasher},
    num::NonZero,
    ops::{Deref, DerefMut},
    sync::Arc,
};

use crate::{
    wikidot_page::{LoadCache, RepoCache, RepoData, load_page, repo},
    wikidot_parser::types::Content,
};
use dentrado::{
    core::{
        gear::{GearInput, GearMeta, IsRuntime},
        storage::{CacheSer, PageId, Storage},
    },
    types::{GlobalCoreId, LocMsgTypeId, Localizable},
};

use crate::{safe_path::SafePathComponent, wikidot_page::RepoMeta};

#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum GearId {
    Repo(RepoMeta),
    Load {
        repo: RepoMeta,
        site: SafePathComponent,
        slug: (Option<SafePathComponent>, SafePathComponent), // `draft:my` should be stored as `("draft_", "mine")`
    },
}

#[derive(Debug, Clone)]
pub(crate) enum GearOut {
    RepoOut(Arc<RepoData>),
    LoadOut(Arc<Content>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Group {
    Phantom(u32), // For () gears
}

const PHANTOM_MSG: LocMsgTypeId = LocMsgTypeId(0);

impl Localizable for GearId {
    async fn localize<Rm: dentrado::types::Remapper>(
        self,
        _remapper: &mut Rm,
    ) -> Result<Self, Rm::Err> {
        use GearId::*;
        match self {
            Repo { .. } => Ok(self),
            Load { .. } => Ok(self),
        }
    }
}

impl Localizable for GearOut {
    async fn localize<Rm: dentrado::types::Remapper>(
        self,
        _remapper: &mut Rm,
    ) -> Result<Self, Rm::Err> {
        use GearOut::*;
        match self {
            RepoOut { .. } => Ok(self),
            LoadOut { .. } => Ok(self),
        }
    }
}

impl Localizable for Group {
    async fn localize<Rm: dentrado::types::Remapper>(
        self,
        _remapper: &mut Rm,
    ) -> Result<Self, Rm::Err> {
        use Group::*;
        match self {
            Phantom(_) => Ok(self),
        }
    }
}

pub(crate) trait Boxable: Debug + Any {
    fn clone_boxed(&self) -> Box<dyn Boxable>;
}
impl<T: Clone + Debug + 'static> Boxable for T {
    fn clone_boxed(&self) -> Box<dyn Boxable> {
        Box::new(self.clone())
    }
}

#[derive(Debug)]
pub(crate) struct Boxed(Box<dyn Boxable>);
impl Clone for Boxed {
    fn clone(&self) -> Self {
        Self(self.0.clone_boxed())
    }
}
impl DerefMut for Boxed {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut *self.0
    }
}

impl Deref for Boxed {
    type Target = dyn Any;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl CacheSer for Boxed {
    fn page_roots(&self) -> &[PageId] {
        &[]
    }
}

impl IsRuntime for KolorinkoRT {
    type GearId = GearId;

    type GearOut = GearOut;

    type Module = ();

    type Group = Group;

    type Body = ();

    type Data = ();

    type GearCache<W>
        = Boxed
    where
        W: Debug + Clone + 'static;

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<[u8; 32], dentrado::types::GroupRouteError> {
        Ok([0; 32])
    }

    fn route_group(
        key: &Self::Group,
        _resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<dentrado::types::GlobalCoreId, dentrado::types::GroupRouteError> {
        match key {
            Group::Phantom(x) => Ok(GlobalCoreId(*x)),
        }
    }

    fn meta(gear: &Self::GearId) -> GearMeta<Self> {
        match gear {
            // `repo` is an oracle: it polls the remote git repository on a
            // timer (every `interval` seconds) and rebuilds the full in-memory
            // dataset. Its group still routes it to a deterministic core, but
            // the trigger is the epoch ticker, not events.
            GearId::Repo(repo_meta) => {
                let mut hasher = DefaultHasher::new();
                repo_meta.hash(&mut hasher);
                GearMeta::Timer {
                    group: Group::Phantom(hasher.finish() as u32),
                    period: NonZero::new(u64::from(repo_meta.interval()))
                        .unwrap_or_else(|| NonZero::new(900).expect("900 != 0")),
                }
            }
            // `load` is event-driven in the sense that it has no timer of its
            // own; it runs when first activated and whenever its `repo`
            // dependency produces new output. Its group is a unique phantom
            // group that nothing ever posts events to, so only dependency
            // kicks (and first activation) ever run it.
            GearId::Load { repo, site, slug } => {
                let mut hasher = DefaultHasher::new();
                repo.hash(&mut hasher);
                site.hash(&mut hasher);
                slug.hash(&mut hasher);
                GearMeta::Event {
                    msg_type: PHANTOM_MSG,
                    group: Group::Phantom(hasher.finish() as u32),
                }
            }
        }
    }

    // Heterogeneous per-gear cache payloads are erased behind `Box<dyn Any>`;
    // each gear variant downcasts to its own cache type inside `run_step`.
    fn make_cache<Watermark: Debug + Clone + Default + 'static>(
        gear: &Self::GearId,
    ) -> Self::GearCache<Watermark> {
        Boxed(match gear {
            GearId::Repo(_) => Box::new(RepoCache::default()),
            GearId::Load { .. } => Box::new(LoadCache::default()),
        })
    }

    async fn run_step<S: Storage<Self>>(
        ctx: &mut dentrado::core::core_ctx::GearCtx<Self, S>,
        input: GearInput,
        cache: &mut Self::GearCache<S::Watermark>,
    ) -> Self::GearOut {
        match ctx.gear().clone() {
            // Oracle: pull + rebuild on a tick, return the cached dataset
            // otherwise. The `repo` gear is the only thing that touches the
            // working tree.
            GearId::Repo(repo_meta) => {
                let tick = matches!(input, GearInput::Timer { tick: true });
                GearOut::RepoOut(repo(
                    &repo_meta,
                    tick,
                    cache.downcast_mut().expect("Repo cache"),
                ))
            }
            // Event gear whose only real input is its `repo` dependency.
            // `load_page` ignores `input` (it indexes the `repo` dataset via
            // `secondary_get`), and this gear is event-sourced so a `Timer`
            // input never arrives here — either way, just (re)compute. The
            // per-instance cache holds the last parsed page so an unchanged
            // page (same `Arc<str>` from the persistent map) isn't re-parsed.
            GearId::Load { repo, site, slug } => {
                let load_cache = cache.downcast_mut().expect("Load cache");
                GearOut::LoadOut(load_page(&repo, &site, &slug, ctx, load_cache).await)
            }
        }
    }
}
