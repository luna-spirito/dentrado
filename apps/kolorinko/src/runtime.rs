use dentrado::core::gear::IsRuntime;

#[derive(Debug)]
pub(crate) struct KolorinkoRT;

// 1)

impl IsRuntime for KolorinkoRT {
    type GearId = ();

    type GearOut = ();

    type Module = ();

    type Group = ();

    type Body = ();

    type Data = ();

    fn hash_data(
        _data: &Self::Data,
        _resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<[u8; 32], dentrado::types::GroupRouteError> {
        todo!()
    }

    fn route_group(
        _key: &Self::Group,
        _resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<dentrado::types::GlobalCoreId, dentrado::types::GroupRouteError> {
        todo!()
    }

    fn meta(_gear: &Self::GearId) -> (dentrado::types::LocMsgTypeId, Self::Group) {
        todo!()
    }

    fn make_cache(_gear: &Self::GearId) -> Box<dyn std::any::Any> {
        todo!()
    }

    fn run_step(
        _gear: &Self::GearId,
        _core: &dentrado::core::core_ctx::Core<Self>,
        _group: Option<dentrado::types::LocGroupId>,
        _cache: &mut dyn std::any::Any,
    ) -> Self::GearOut {
        todo!()
    }
}
