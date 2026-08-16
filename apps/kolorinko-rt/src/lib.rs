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

/// A page slug: `(category, page)` — Wikidot's `category:name` flattened.
pub type Slug = (Option<SafePathComponent>, SafePathComponent);

/// The landing page a bare `/<site>/` resolves to.
pub const START_PAGE: &str = "start";

/// `/<site>[/<category>/<page>]` → `(site, slug)`. A bare `/<site>/` resolves
/// to the site's `start` landing page. `None` for bare `/`, asset paths, wrong
/// arity, or unsafe segments. Shared by the web client's router, the server's
/// SSR dispatch, and the render CLI, so all three agree on what a route is.
pub fn parse_route(path: &str) -> Option<(SafePathComponent, Slug)> {
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split(['/', '#', '?'])
        .filter(|s| !s.is_empty())
        .collect();
    let spc = |s: &str| SafePathComponent::new(s.to_string());
    match segs.as_slice() {
        [s] => Some((spc(s)?, (None, spc(START_PAGE)?))),
        [s, p] => Some((spc(s)?, (None, spc(p)?))),
        [s, c, p] => Some((spc(s)?, (Some(spc(c)?), spc(p)?))),
        _ => None,
    }
}

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

/// A validated relative path: no `..`, no empty/`.` segments, not absolute.
/// The `host/path…` tail of a mirrored attachment, keyed this way in the
/// `files/` index so [`repo_resource`] can resolve it to a content-addressed
/// [`CaRef`].
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

// Validate on deserialize (not just `new`): a wire `GearId::RepoResource`
// arriving from a client must not carry a traversal path. Mirrors
// `SafePathComponent`'s defensive deserialize.
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
/// [`Asset`] gear's output. Carried behind a [`bytes::Bytes`] so serving is
/// a refcount bump, never a memcpy.
///
/// [`Asset`]: kolorinko_rt gear
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

/// Server-side-rendered page state embedded into the SSR document, so the
/// client hydrates from exactly the data the server rendered with (then keeps
/// the same signals live via WebTransport). [`SSR_STATE_ID`] is the `<script>`
/// element id it travels in; its presence is also the client's hydrate-vs-mount
/// signal.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SsrState {
    pub page: ArticleView,
    pub shell: SiteShell,
}

pub const SSR_STATE_ID: &str = "kolorinko-ssr";

impl SsrState {
    /// Serialize into the body of an `<script type="application/json">`
    /// element. `<` is escaped as `\u003c` so page content can never close the
    /// script element early.
    pub fn to_embedded_json(&self) -> String {
        serde_json::to_string(self)
            .expect("SsrState serializes")
            .replace('<', "\\u003c")
    }

    /// Inverse of [`to_embedded_json`]; `None` on malformed state.
    pub fn from_embedded_json(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
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
    use crate::{Body, CaRef, RepoAssetPath, SafePathComponent, SiteShell};
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
