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
        data: &Self::Data,
        resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<[u8; 32], dentrado::types::GroupRouteError> {
        todo!()
    }

    fn route_group(
        key: &Self::Group,
        resolver: &dyn dentrado::types::GlobalResolver,
    ) -> Result<dentrado::types::GlobalCoreId, dentrado::types::GroupRouteError> {
        todo!()
    }

    fn meta(gear: &Self::GearId) -> (dentrado::types::LocMsgTypeId, Self::Group) {
        todo!()
    }

    fn make_cache(gear: &Self::GearId) -> Box<dyn std::any::Any> {
        todo!()
    }

    fn run_step(
        gear: &Self::GearId,
        core: &dentrado::core::core_ctx::Core<Self>,
        group: Option<dentrado::types::LocGroupId>,
        cache: &mut dyn std::any::Any,
    ) -> Self::GearOut {
        todo!()
    }
}
