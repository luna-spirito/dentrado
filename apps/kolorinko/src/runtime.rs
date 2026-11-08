use dentrado::core::{core_ctx::GearCtx, gear::GearResult, storage::IsStorage};
use kolorinko_rt::{
    CodeBlock, LocalId, RepoMeta, SafePathComponent, ServedBlob, SiteShell, SpaceId, wire,
};
use kolorinko_wikitext::ArticleView;

/// The Kolorinko runtime.
///
/// This file is now a thin **skeleton**: the gear declarations live in
/// [`kolorinko_rt`]'s `gears.def.rs`, read by *two* macros so each gear is
/// declared exactly once — this `#[gears]` (the native dentrado runtime: impl
/// fns + generated `GearId`/`GearOut`/`IsRuntime`/`GearQuery` builders) and
/// the client's `#[gears_schema]` (the wasm wire schema). The
/// impl fn bodies (`crate::wikidot_page::…`) resolve here; the implementations
/// themselves live in [`wikidot_page`].
///
/// `wire = kolorinko_rt::wire` deduplicates the payload layer too: the wire
/// schema's `GearOut` is aliased as this runtime's `GearOut`/`GearOutShared`
/// (both generated from the one gear file, so the variants agree by
/// construction), and a generated `From<wire::GearId>` bridges the ids — the
/// runtime id is the strict superset carrying the `local`/`internal` gears.
#[derive(Debug)]
pub(crate) struct KolorinkoRT;

#[dentrado::gears(
    runtime = KolorinkoRT,
    file = "../kolorinko-rt/gears.def.rs",
    wire = kolorinko_rt::wire
)]
mod gears {
    use super::*;
}

// `GearId` stays internal to production code: gear ids are constructed only
// through the generated `GearQuery` builders — the one exception is the
// generated wire→runtime `From`, which a client-supplied wire id crosses at
// the subscription boundary. (`GearId` itself is re-exported under `cfg(test)`
// so the view round-trip tests can name a gear's view.) `GearOutShared` (an
// alias of `kolorinko_rt::wire::GearOut`) and the builders are re-exported for
// the server's dispatch/stale-read code.
pub(crate) use gears::{GearOutShared, article_latest, asset, code_block, repo, shell};

// The concrete id is otherwise unreachable outside the generated module; the
// view round-trip tests open a gear view by id.
#[cfg(test)]
pub(crate) use gears::GearId;

/// Project a stored gear output onto the wire form, if this family is
/// shippable. A `Ship` output already *is* a wire output, a `Shared` handle
/// clones its payload out of the allocation (one copy per push; TODO serialize
/// from the shared borrow instead — no copies), and `Local` / non-`exposed`
/// outputs never ship.
pub(crate) fn to_wire_out(res: GearResult<KolorinkoRT>) -> Option<wire::GearOut> {
    match res {
        GearResult::Ship(out) => Some(out),
        GearResult::Shared(s) => Some((*s).clone()),
        GearResult::Local(_) => None,
    }
    .filter(|out| out.is_exposed())
}

/// SHA-256 (hex) of a wire `GearOut`'s JSON encoding — the content hash the
/// protocol skips unchanged pushes by (see [`kolorinko_rt::wire`]). Shared with
/// the SSR state embedder so a hydrated page hashes exactly like a pushed one.
pub(crate) fn out_hash(out: &wire::GearOut) -> String {
    let json = serde_json::to_vec(out).expect("GearOut serializes");
    ring::digest::digest(&ring::digest::SHA256, &json)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
