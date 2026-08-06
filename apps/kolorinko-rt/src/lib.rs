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
#[derive(
    Debug,
    Clone,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    dentrado_types::Localizable,
)]
#[serde(transparent)]
pub struct SafePathComponent(String);

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
#[dentrado_macros::gears_schema(file = "gears.def.rs")]
pub mod wire {
    use crate::SafePathComponent;
    use kolorinko_wikitext::{ArticleLatest, ArticleView};

    /// Client → server: start or stop a subscription. The server dispatches
    /// `id` onto its runtime `GearId` (injecting its configured `repo_meta`).
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub enum ClientMsg {
        Subscribe { id: GearId },
        Cancel { sub: u64 },
    }

    /// Server → client: a subscription was accepted, produced an update, or was
    /// dropped. `sub` is the server-assigned handle the client cancels with.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub enum ServerMsg {
        Subscribed { sub: u64, id: GearId },
        Update { sub: u64, out: GearOut },
        Dropped { sub: u64 },
    }
}
