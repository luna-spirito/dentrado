//! Shared gear schema and wire protocol for the kolorinko server (`kolorinko`)
//! and web client (`kolorinko-web`).
//!
//! The gears are declared exactly once in [`gears.def`](../gears.def.rs); the
//! [`gears_schema`](dentrado_macros::gears_schema) macro reads that file and
//! emits a wasm-safe, dentrado-free [`wire`] schema (serde `GearId` / `GearOut`
//! / `GearQuery` + a generic subscribe/cancel/push envelope). The server's
//! `#[dears]`/`#[gears]` reads the *same* file for its runtime, so there is no
//! hand-written duplicate of the gear list.

use kolorinko_wikitext::{ArticleView, ListPagesParams};

mod ids;

use std::{
    ops::Deref,
    path::{Component, Path},
};

pub use crate::ids::{parse_canonical, LocalId, PageAddr, SpaceId, SYSTEM_PREFIX};

/// A page slug: `(category, page)` — Wikidot's `category:name` flattened.
pub type Slug = (Option<SafePathComponent>, SafePathComponent);

/// The landing page a bare `/<site>/` resolves to.
pub const START_PAGE: &str = "start";

/// `category:name` / `name` → a slug, mirroring how the dataset keys pages
/// (the mirror's `slug_parts`): the colon form is Wikidot's canonical page
/// URL and must resolve identically to the generated `/<site>/<cat>/<page>`
/// links.
fn slug_of(seg: &str) -> Option<Slug> {
    match seg.split_once(':') {
        Some((cat, name)) => Some((
            Some(SafePathComponent::new(cat.to_string())?),
            SafePathComponent::new(name.to_string())?,
        )),
        None => Some((None, SafePathComponent::new(seg.to_string())?)),
    }
}

/// `/<site>[/<category>/<page>]` → `(site, slug)`. A bare `/<site>/` resolves
/// to the site's `start` landing page. A `/<site>/<category>:<page>` segment
/// splits on the colon like the dataset keys it. `None` for bare `/`, asset
/// paths, wrong arity, or unsafe segments. Shared by the web client's router,
/// the server's SSR dispatch, and the render CLI, so all three agree on what
/// a route is.
pub fn parse_route(path: &str) -> Option<(SafePathComponent, Slug)> {
    let segs: Vec<&str> = path
        .trim_start_matches('/')
        .split(['/', '#', '?'])
        .filter(|s| !s.is_empty())
        .collect();
    let spc = |s: &str| SafePathComponent::new(s.to_string());
    match segs.as_slice() {
        [s] => Some((spc(s)?, (None, spc(START_PAGE)?))),
        [s, p] => Some((spc(s)?, slug_of(p)?)),
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

impl Deref for SafePathComponent {
    type Target = String;

    fn deref(&self) -> &Self::Target {
        &self.0
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

/// A `[[module ListPages]]` selection as a gear-id payload: the module's
/// parsed parameters, context selectors (`category="."`, `tags="="`, …)
/// already resolved against the rendering page. Plain data with no local
/// ids, so localization is the identity.
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
pub struct ListPagesQuery(#[localizable(skip)] pub ListPagesParams);

/// One page selected by a ListPages module: everything a template body can
/// reference through `%%…%%` variables.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListedPage {
    /// Page name without category.
    pub name: String,
    /// Page category, `None` for a root (`_default`) page.
    pub category: Option<String>,
    pub title: String,
    pub tags: Vec<String>,
    pub created_by: String,
    pub created_at: i64,
    pub updated_by: String,
    pub updated_at: i64,
    pub revisions: u64,
}

impl ListedPage {
    /// Canonical Wikidot fullname: `category:name`, or just the name for a
    /// root page.
    #[must_use]
    pub fn fullname(&self) -> String {
        match &self.category {
            Some(c) => format!("{c}:{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// The outcome of a ListPages selection: the (ordered, truncated to the first
/// pagination page) matching pages plus the total match count, which the
/// `%%total%%` template variable reports ignoring the limit.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListPagesResult {
    pub pages: Vec<ListedPage>,
    pub total: i64,
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
///
/// The `*_hash` fields are the content hashes (see [`crate::wire`]) of the
/// corresponding wire outputs: a hydrating client echoes them in `Subscribe`
/// so the server re-sends nothing that the rendered page already shows.
///
/// `addr` is the resolved dataset address (site + slug) — the subscription
/// keys for `page`/`shell`, so a canonical-URL hydration needs no resolution
/// round-trip. `route` is the canonical `(space, local)` address when the URL
/// was canonical (`None` on legacy paths).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SsrState {
    pub page: ArticleView,
    pub page_hash: String,
    pub shell: SiteShell,
    pub shell_hash: String,
    pub addr: PageAddr,
    pub route: Option<(SpaceId, LocalId)>,
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
/// emitted by the macro; `ClientMsg` / `ServerMsg` are the dumb subscribe/push
/// envelope the server speaks generically (no domain `enum Request`).
///
/// One bidi stream per subscription: the client's `Subscribe` is the stream's
/// only outgoing frame, and either side closing the stream ends the
/// subscription (client close = cancel, server close = dropped) — so no
/// cancel/drop messages exist.
///
/// Content hashes (SHA-256 over the wire `GearOut` JSON, computed only by the
/// server) let both sides skip re-sending unchanged payloads: the client
/// echoes the hash it already holds (a previous push's, or the SSR state's)
/// in `Subscribe`, and the server pushes only when the output's hash differs.
#[dentrado_macros::gears_schema(file = "gears.def.rs")]
pub mod wire {
    use crate::{
        Body, CaRef, ListPagesQuery, ListPagesResult, LocalId, PageAddr, RepoAssetPath,
        SafePathComponent, SiteShell, SpaceId,
    };
    use kolorinko_wikitext::{ArticleLatest, ArticleView};

    /// Client → server: subscribe to a gear. This is the stream's only
    /// client→server message. `hash` is the SHA-256 of the wire `GearOut` JSON
    /// the client already holds — from the last push on this subscription, or
    /// from the embedded SSR state on a hydration boot. `None` means "send
    /// everything" (plain CSR boot).
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub enum ClientMsg {
        Subscribe { id: GearId, hash: Option<String> },
    }

    /// Server → client: an output whose hash differs from what the client last
    /// held. Unchanged outputs are skipped entirely (no frame), and the open
    /// stream itself is the liveness guarantee. The hash is what the client
    /// echoes back in `Subscribe` on the next (re)connection.
    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    #[serde(tag = "t", rename_all = "lowercase")]
    pub enum ServerMsg {
        Push { out: GearOut, hash: String },
    }
}
