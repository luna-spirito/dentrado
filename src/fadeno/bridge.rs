use crate::{
    core::{core_ctx::Core, gear::Runtime, loc_ctx::LocCtx},
    fadeno::{
        types::{Compiled, Instr, KolGear, LocValue, TagRegistry},
        vm,
    },
    types::{GlobalCoreId, GlobalResolver, GroupRouteError, LocGroupId, LocMsgTypeId},
    wire::format::WireLocCtx,
};

#[derive(Debug)]
pub struct FadenoRuntime;

impl Runtime for FadenoRuntime {
    type GearId = KolGear;
    type GearOut = LocValue;
    type Module = FadenoModule;
    type Group = LocValue;
    type Body = LocValue;
    type Data = LocValue;

    fn hash_data(
        data: &LocValue,
        resolver: &dyn GlobalResolver,
    ) -> Result<[u8; 32], GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hash_loc_value(data, resolver, &mut hasher)?;
        Ok(*hasher.finalize().as_bytes())
    }

    fn route_group(
        key: &LocValue,
        wire_ctx: &WireLocCtx<Self>,
    ) -> Result<GlobalCoreId, GroupRouteError> {
        let mut hasher = blake3::Hasher::new();
        hash_loc_value(key, wire_ctx, &mut hasher)?;
        Ok(GlobalCoreId(u32::from_le_bytes(
            hasher.finalize().as_bytes()[..4].try_into().unwrap(),
        )))
    }

    fn meta(gear: &KolGear) -> (LocMsgTypeId, LocValue) {
        (gear.primary_msg_type, gear.primary_group.clone())
    }

    fn make_cache(gear: &KolGear) -> Box<dyn std::any::Any> {
        Box::new(gear.initial_cache.clone())
    }

    fn run_step(
        gear: &KolGear,
        core: &Core<Self>,
        group: Option<LocGroupId>,
        cache: &mut dyn std::any::Any,
    ) -> LocValue {
        let cache = cache
            .downcast_mut::<LocValue>()
            .expect("KolGear cache type mismatch: expected LocValue");
        fadeno_gear_step(core.module(), gear.step.clone(), group, core, cache)
    }
}

pub fn hash_loc_value(
    v: &LocValue,
    resolver: &dyn GlobalResolver,
    hasher: &mut blake3::Hasher,
) -> Result<(), GroupRouteError> {
    match v {
        LocValue::Num(n) => {
            hasher.update(b"N");
            hasher.update(&n.to_le_bytes());
        }
        LocValue::Bool(b) => {
            hasher.update(b"B");
            hasher.update(&[u8::from(*b)]);
        }
        LocValue::Tag(t) => {
            hasher.update(b"T");
            hasher.update(&t.to_le_bytes());
        }

        LocValue::KolEventId(_) => {
            panic!("KolEventId must not appear in routing context — use KolDataId")
        }

        LocValue::KolDataId(id) => {
            hasher.update(b"D");
            let data_id = resolver.resolve_data(*id)?;
            hasher.update(&data_id.timestamp.to_le_bytes());
            hasher.update(&data_id.hash);
        }

        LocValue::KolEventTypeId(id) => {
            hasher.update(b"ET");
            hasher.update(&id.0.to_le_bytes());
        }

        LocValue::KolUserId(lid) => {
            hasher.update(b"U");
            let uid = resolver.resolve_user(*lid)?;
            hasher.update(&uid.id.to_le_bytes());
            hasher.update(&uid.identity_server_pk.0);
        }

        LocValue::List(vs) => {
            hasher.update(b"L");
            for item in vs.iter() {
                hash_loc_value(item, resolver, hasher)?;
            }
        }
        LocValue::Record { fields, .. } => {
            hasher.update(b"R");
            for field in fields.iter() {
                hash_loc_value(field, resolver, hasher)?;
            }
        }

        LocValue::KolPrimary | LocValue::KolSecondary => {
            return Err(GroupRouteError::ContextPlaceholder)
        }

        LocValue::KolGear(_) => return Err(GroupRouteError::DomainValue("KolGear")),
        LocValue::KolStateGraph(_) => return Err(GroupRouteError::DomainValue("KolStateGraph")),
        LocValue::KolStateGraphOut(_) => {
            return Err(GroupRouteError::DomainValue("KolStateGraphOut"))
        }
        LocValue::KolAnchorAgg(_) => return Err(GroupRouteError::DomainValue("KolAnchorAgg")),
        LocValue::KolTextAgg(_) => return Err(GroupRouteError::DomainValue("KolTextAgg")),
        LocValue::KolTextUpd(_) => return Err(GroupRouteError::DomainValue("KolTextUpd")),
        LocValue::KolQuery(_, _) => return Err(GroupRouteError::DomainValue("KolQuery")),
        LocValue::KolSenderId(_) => return Err(GroupRouteError::DomainValue("KolSenderId")),
        LocValue::Panic => return Err(GroupRouteError::DomainValue("Panic")),
        LocValue::Builtin(_) => return Err(GroupRouteError::DomainValue("Builtin")),
        LocValue::Closure(_) => return Err(GroupRouteError::DomainValue("Closure")),
        LocValue::BuiltinsVar => return Err(GroupRouteError::DomainValue("BuiltinsVar")),
        LocValue::Import(_) => return Err(GroupRouteError::DomainValue("Import")),
        LocValue::Partial { .. } => return Err(GroupRouteError::DomainValue("Partial")),
        LocValue::LoopCont { .. } => return Err(GroupRouteError::DomainValue("LoopCont")),
    }
    Ok(())
}

#[derive(Debug)]
pub struct FadenoModule {
    pool: Vec<Instr>,
    constants: Vec<LocValue>,
    tags: TagRegistry,
    all_exports: Vec<LocValue>,
    common: vm::CommonTags,
}

impl Default for FadenoModule {
    fn default() -> Self {
        Self {
            pool: Vec::new(),
            constants: Vec::new(),
            tags: TagRegistry::new(Vec::new(), Vec::new()),
            all_exports: Vec::new(),
            common: vm::CommonTags {
                sender: 0,
                body: 0,
                has: 0,
                edit: 0,
                cache: 0,
                out: 0,
                primary: 0,
                r#type: 0,
                group: 0,
                initial_cache: 0,
                step: 0,
                viewl_tag_set: 0,
                query_result_tag_set: 0,
                delta_tag_set: 0,
                opt_none_tag_set: 0,
                opt_some_tag_set: 0,
                event_rec_tag_set: 0,
                sender_tag_set: 0,
            },
        }
    }
}

impl FadenoModule {
    pub fn new(mut cr: Compiled) -> Result<Self, vm::VmError> {
        let common = vm::CommonTags::ensure(&mut cr.tags);

        Ok(Self {
            all_exports: vm::init(&cr, &common)?,
            pool: cr.pool,
            constants: cr.constants,
            tags: cr.tags,
            common,
        })
    }

    #[must_use]
    pub fn tags(&self) -> &TagRegistry {
        &self.tags
    }

    #[must_use]
    pub fn exports(&self) -> &LocValue {
        self.all_exports.last().expect("no modules compiled")
    }

    pub fn call_with_storage(
        &self,
        func: LocValue,
        args: Vec<LocValue>,
        storage: &LocCtx<FadenoRuntime>,
    ) -> Result<LocValue, vm::VmError> {
        vm::call_with_storage(
            &self.pool,
            &self.constants,
            &self.tags,
            &self.all_exports,
            &self.common,
            func,
            args,
            storage,
        )
    }
}

fn fadeno_gear_step(
    module: &FadenoModule,
    step_closure: LocValue,
    primary: Option<LocGroupId>,
    core: &Core<FadenoRuntime>,
    cache: &mut LocValue,
) -> LocValue {
    let ctx = vm::VmContext {
        pool: &module.pool,
        constants: &module.constants,
        tags: &module.tags,
        imports: &module.all_exports,
        common: &module.common,
    };
    let group_as_primary = LocValue::KolPrimary;
    let secondary_placeholder = LocValue::KolSecondary;
    let cache_inner = std::mem::replace(cache, LocValue::Panic);
    let result = match vm::call_gear_step(
        &ctx,
        core,
        step_closure,
        vec![cache_inner.clone(), group_as_primary, secondary_placeholder],
        primary,
    ) {
        Ok(r) => r,
        Err(e) => panic!("Fadeno step closure failed: {e:?}"),
    };

    let new_cache = module
        .tags()
        .record_get_by_tag(&result, module.common.cache)
        .expect("step result missing .cache");
    let output = module
        .tags()
        .record_get_by_tag(&result, module.common.out)
        .expect("step result missing .out");

    *cache = new_cache;
    output
}
