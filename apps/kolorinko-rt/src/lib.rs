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

use kolorinko_wikitext::ArticleView;

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

/// Which mirrored subtree a [`RepoAsset`] gear reads from: `<site>/theme/…`
/// (stylesheets, rewritten to local refs) or `<site>/files/…` (attachments).
///
/// [`RepoAsset`]: kolorinko_rt gear
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    dentrado_types::Localizable,
)]
pub enum AssetKind {
    Theme,
    Files,
}

impl AssetKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Files => "files",
        }
    }

    /// Parse the `kind` segment of a `/repo/<site>/<kind>/…` URL, or `None`
    /// for anything outside the `theme`/`files` namespaces.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "theme" => Some(Self::Theme),
            "files" => Some(Self::Files),
            _ => None,
        }
    }
}

/// A validated relative path under `<site>/<kind>/`: no `..`, no empty/`.`
/// segments, not absolute. Carries the `<host>/<path…>` tail of a
/// `/repo/<site>/<kind>/<host>/<path…>` request, so it doubles as the origin
/// URL the missing-asset redirect falls back onto (`https://{this}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, dentrado_types::Localizable)]
pub struct RepoAssetPath(String);

impl RepoAssetPath {
    /// Validate `input` is a non-empty relative path of normal segments.
    #[must_use]
    pub fn new(input: String) -> Option<Self> {
        if input.is_empty()
            || input
                .split('/')
                .any(|s| s.is_empty() || s == "." || s == "..")
        {
            return None;
        }
        // Reject absolute paths (`/x`) and any non-`Normal` component
        // (RootDir, CurDir, ParentDir, Prefix). Multi-segment relative paths
        // of normals are valid (`host/path/to/file.css`).
        if !Path::new(&input)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            return None;
        }
        Some(Self(input))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for RepoAssetPath {
    fn as_ref(&self) -> &Path {
        self.0.as_ref()
    }
}

// Validate on deserialize (not just `new`): a wire `GearId::RepoAsset` arriving
// from a client must not carry a traversal path, since the gear joins it into
// the repo tree. Mirrors `SafePathComponent`'s defensive deserialize.
impl<'de> serde::Deserialize<'de> for RepoAssetPath {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?)
            .ok_or_else(|| serde::de::Error::custom("invalid repo asset path"))
    }
}

impl serde::Serialize for RepoAssetPath {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}

/// The body of a served asset: stored zstd-compressed when compression helped,
/// otherwise raw. Shared by static assets (loaded once at startup) and the
/// [`RepoAsset`] gear's output. Carried behind a [`bytes::Bytes`] so serving is
/// a refcount bump, never a memcpy.
///
/// [`RepoAsset`]: kolorinko_rt gear
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum Body {
    Raw(bytes::Bytes),
    Zstd(bytes::Bytes),
}

/// A resolved content-addressed asset reference: the SHA-256 (lowercase hex)
/// of its bytes plus the original filename extension. The extension rides in
/// the *reference* (not in the CA blob's name, which is the bare hash) so the
/// MIME is derivable without a side table — [`crate::wire`] never needs the
/// blob's type, only this pair.
///
/// Serialized onto `/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` by the resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaRef {
    /// 64-char lowercase hex SHA-256.
    pub hash: String,
    /// Extension without the dot (`"jpg"`, `"png"`, …).
    pub ext: String,
}

/// Output of the [`RepoAsset`] gear: the asset's bytes (compressed when that
/// helped) or a redirect back onto the original host when the file is missing.
///
/// [`RepoAsset`]: kolorinko_rt gear
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum RepoAssetOut {
    Ok(Body),
    Redirect { location: String },
}

/// One site's persistent chrome, fetched atomically in a single `site`-keyed
/// subscription: the site title + subtitle, the theme stylesheet as a
/// content-addressed URL, and the fully include-resolved `nav:top` / `nav:side`
/// pages. Bundled so the client requests the whole site frame once and keeps it
/// live across page navigation within the site.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SiteShell {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// CA URL `/repo/<site>/files/<xx>/<yy>/<hash>.css`, or `None` if the site
    /// has no theme root mirrored into `files/`.
    pub theme_root: Option<String>,
    pub nav_top: ArticleView,
    pub nav_side: ArticleView,
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
    use crate::{
        AssetKind, Body, CaRef, RepoAssetOut, RepoAssetPath, SafePathComponent, SiteShell,
    };
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
