use std::{
    hash::{DefaultHasher, Hash, Hasher},
    sync::Arc,
};

use crate::{
    wikidot_page::{LoadCache, RepoCache, load_page, repo},
    wikidot_parser::types::Content,
};
use dentrado::{
    core::gear::IsRuntime,
    types::{GlobalCoreId, LocMsgTypeId, Localizable},
};

use crate::{
    safe_path::SafePathComponent,
    wikidot_page::{RepoMeta, RepoPath},
};

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
    RepoOut(RepoPath),
    LoadOut(Arc<Content>),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Group {
    Phantom(u32), // For () gears
}

const PHANTOM_MSG: LocMsgTypeId = LocMsgTypeId(0);

impl Localizable for GearId {
    fn localize<U, S, D, E>(
        self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Self, E>
    where
        U: FnMut(dentrado::types::LocUserId) -> Result<dentrado::types::LocUserId, E>,
        S: FnMut(dentrado::types::LocSenderId) -> Result<dentrado::types::LocSenderId, E>,
        D: FnMut(dentrado::types::LocDataId) -> Result<dentrado::types::LocDataId, E>,
    {
        use GearId::*;
        match self {
            Repo { .. } => Ok(self),
            Load { .. } => Ok(self),
        }
    }
}

impl Localizable for GearOut {
    fn localize<U, S, D, E>(
        self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Self, E>
    where
        U: FnMut(dentrado::types::LocUserId) -> Result<dentrado::types::LocUserId, E>,
        S: FnMut(dentrado::types::LocSenderId) -> Result<dentrado::types::LocSenderId, E>,
        D: FnMut(dentrado::types::LocDataId) -> Result<dentrado::types::LocDataId, E>,
    {
        use GearOut::*;
        match self {
            RepoOut { .. } => Ok(self),
            LoadOut { .. } => Ok(self),
        }
    }
}

impl Localizable for Group {
    fn localize<U, S, D, E>(
        self,
        _remap_user: &mut U,
        _remap_sender: &mut S,
        _remap_data: &mut D,
    ) -> Result<Self, E>
    where
        U: FnMut(dentrado::types::LocUserId) -> Result<dentrado::types::LocUserId, E>,
        S: FnMut(dentrado::types::LocSenderId) -> Result<dentrado::types::LocSenderId, E>,
        D: FnMut(dentrado::types::LocDataId) -> Result<dentrado::types::LocDataId, E>,
    {
        use Group::*;
        match self {
            Phantom(_) => Ok(self),
        }
    }
}

impl IsRuntime for KolorinkoRT {
    type GearId = GearId;

    type GearOut = GearOut;

    type Module = ();

    type Group = Group;

    type Body = ();

    type Data = ();

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

    fn meta(gear: &Self::GearId) -> (dentrado::types::LocMsgTypeId, Self::Group) {
        match gear {
            GearId::Repo(repo_meta) => {
                let mut hasher = DefaultHasher::new();
                repo_meta.hash(&mut hasher);
                (PHANTOM_MSG, Group::Phantom(hasher.finish() as u32))
            }
            GearId::Load { repo, site, slug } => {
                let mut hasher = DefaultHasher::new();
                repo.hash(&mut hasher);
                site.hash(&mut hasher);
                slug.hash(&mut hasher);
                (PHANTOM_MSG, Group::Phantom(hasher.finish() as u32))
            }
        }
    }

    // TODO: remove boxing?
    fn make_cache(gear: &Self::GearId) -> Box<dyn std::any::Any> {
        match gear {
            GearId::Repo(_) => Box::new(RepoCache::default()),
            GearId::Load { .. } => Box::new(LoadCache::default()),
        }
    }

    fn run_step(
        gear: &Self::GearId,
        core: &dentrado::core::core_ctx::Core<Self>,
        group: Option<dentrado::types::LocGroupId>,
        cache: &mut dyn std::any::Any,
    ) -> Self::GearOut {
        match gear {
            GearId::Repo(repo_meta) => {
                GearOut::RepoOut(repo(repo_meta, group, cache.downcast_mut().unwrap()))
            }
            GearId::Load { repo, site, slug } => GearOut::LoadOut(load_page(
                repo,
                site,
                slug,
                core,
                cache.downcast_mut().unwrap(),
            )),
        }
    }
}
