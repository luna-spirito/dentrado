//! Shared gear schema and wire protocol for the kolorinko server (`kolorinko`)
//! and web client (`kolorinko-web`).
//!
//! The gears are declared exactly once in [`gears.def`](../gears.def.rs); the
//! [`gears_schema`](dentrado_macros::gears_schema) macro reads that file and
//! emits a wasm-safe, dentrado-free [`wire`] schema (serde `GearId` / `GearOut`
//! / `GearQuery` + a generic subscribe/cancel/push envelope). The server's
//! `#[dears]`/`#[gears]` reads the *same* file for its runtime, so there is no
//! hand-written duplicate of the gear list.

use std::path::{Component, Path};

/// A single validated path component (a Wikidot site/category/page-name
/// segment). On the wire it serializes as a plain string; it derives
/// [`Localizable`](dentrado_types::Localizable) so it can appear as a runtime
/// `GearId` field on the server and be repackaged by the wire-localization
/// contract on the client.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, dentrado_types::Localizable)]
#[serde(transparent)]
pub struct SafePathComponent(String);

// Validate on deserialize (not just `new`): a wire `GearId` arriving from a
// client must not carry `..` / absolute / multi-component paths, since the gear
// impls join it into the repo tree. `#[serde(transparent)]` derive would skip
// validation, so deserialize through `new` and reject the frame otherwise.
impl<'de> serde::Deserialize<'de> for SafePathComponent {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?)
            .ok_or_else(|| serde::de::Error::custom("invalid path component"))
    }
}

impl SafePathComponent {
    /// Validate `input` is exactly one normal path component.
    #[must_use]
    pub fn new(input: String) -> Option<Self> {
        let mut components = Path::new(&input).components();
        match (components.next(), components.next()) {
            (Some(Component::Normal(_)), None) => Some(Self(input)),
            _ => None,
        }
    }

    /// Append `_` (used for the `_default` category sentinel).
    #[must_use]
    pub fn with_underline_suffix(mut self) -> Self {
        self.0.push('_');
        self
    }
}

impl AsRef<Path> for SafePathComponent {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

/// The wire schema + protocol envelope, generated from [`gears.def`](../gears.def.rs).
///
/// `GearId` / `GearOut` / `GearQuery` (and one builder per shippable gear) are
/// emitted by the macro; `ClientMsg` / `ServerMsg` are the dumb subscribe/cancel
/// /push envelope the server speaks generically (no domain `enum Request`).
///
/// The client picks every `sub` handle (a monotonic `u64`), so it can route
/// each `Update` back to the `GearQuery::getter` it registered under that handle
/// — the server never assigns ids.
#[dentrado_macros::gears_schema(file = "gears.def.rs")]
pub mod wire {
    use crate::SafePathComponent;
    use kolorinko_wikitext::{ArticleLatest, ArticleView};

    /// Client → server: start or stop a subscription. `sub` is the client's
    /// own handle; the server echoes it back on every `Update`/`Dropped`.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub enum ClientMsg {
        Subscribe { sub: u64, id: GearId },
        Cancel { sub: u64 },
    }

    /// Server → client: an update for a subscription, or notice that the
    /// subscription ended (gear evicted / errored). The client decodes `out`
    /// via the `GearQuery::getter` registered under `sub`.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub enum ServerMsg {
        Update { sub: u64, out: GearOut },
        Dropped { sub: u64 },
    }
}
