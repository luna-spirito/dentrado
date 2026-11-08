//! Routing: mapping events and gears to the core that owns them.

use std::{collections::HashMap, num::NonZero, sync::Arc};

use crate::{
    core::{
        api::{GearMeta, RuntimeTypes},
        wire::{MergeError, RunGearError, WireEventBody, WireLocCtx},
    },
    types::{GlobalCoreId, GlobalHash},
};

/// Result of routing events to their target cores.
pub(crate) struct RoutedEvents<R: RuntimeTypes> {
    pub(crate) wire_ctx: Arc<WireLocCtx<R>>,
    pub(crate) events: Arc<[WireEventBody<R::Group, R::Body>]>,
    pub(crate) global_core_ids: Arc<[GlobalCoreId]>,
    /// From `target_core` to `event ids`.
    pub(crate) core_seeds: HashMap<u32, Vec<u32>>,
}

/// Route events to their target cores.
pub(crate) fn route_events<R: RuntimeTypes>(
    wire_ctx: WireLocCtx<R>,
    events: Vec<WireEventBody<R::Group, R::Body>>,
    num_cores: NonZero<u32>,
) -> Result<RoutedEvents<R>, MergeError> {
    let global_core_ids: Vec<GlobalCoreId> = events
        .iter()
        .map(|e| e.group.global_hash(&wire_ctx).map(GlobalCoreId::from_hash))
        .collect::<Result<_, _>>()
        .map_err(MergeError::Route)?;

    let wire_ctx = Arc::new(wire_ctx);
    let events: Arc<[WireEventBody<R::Group, R::Body>]> = Arc::from(events);
    let global_core_ids: Arc<[GlobalCoreId]> = Arc::from(global_core_ids);

    let mut core_seeds: HashMap<u32, Vec<u32>> = HashMap::new();
    for (i, gcid) in global_core_ids.iter().enumerate() {
        let target_core = gcid.route(num_cores);
        core_seeds.entry(target_core).or_default().push(i as u32);
    }

    Ok(RoutedEvents {
        wire_ctx,
        events,
        global_core_ids,
        core_seeds,
    })
}

/// Route a gear to the core that owns its group.
pub(crate) fn route_gear<R: RuntimeTypes>(
    meta: &GearMeta<R>,
    wire_ctx: &WireLocCtx<R>,
    num_cores: NonZero<u32>,
) -> Result<u32, RunGearError> {
    let global_core_id = meta
        .group()
        .global_hash(wire_ctx)
        .map(GlobalCoreId::from_hash)
        .map_err(RunGearError::Route)?;
    Ok(global_core_id.route(num_cores))
}
