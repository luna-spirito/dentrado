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

pub use crate::ids::{
    LocalId, SYSTEM_PREFIX, SpaceId, encode_path_segment, format_page_route, parse_local_route,
    parse_page_route, simplify, title_slug,
};

/// A page slug: `(category, page)` — Wikidot's `category:name` flattened.
pub type Slug = (Option<SafePathComponent>, SafePathComponent);

/// The landing page a bare site root resolves to when the site's `shell`
/// names none — `start`, Wikidot's own default.
pub const START_PAGE: &str = "start";

/// The fetch-fallback gear endpoint (`POST`): one wire `ClientMsg::Subscribe`
/// in, one `ServerMsg::Push` out — the plain-HTTP alternative to WebTransport
/// for clients it fails (no push channel; the client asks once per
/// navigation, a content-hash echo skipping unchanged payloads exactly like
/// the WebTransport wire). Lives in the `/-…` system namespace
/// ([`SYSTEM_PREFIX`]); spelled once here so the server's two HTTP stacks
/// and the client's fallback transport can never drift.
pub const LEGACY_PATH: &str = "/-/legacy";

/// `category:name` / `name` → a slug, mirroring how the dataset keys pages:
/// the colon form is Wikidot's canonical page URL and must resolve
/// identically to the generated `/<site>/<cat>/<page>` links. `None` when
/// either segment is not a single safe path component.
#[must_use]
pub fn parse_slug(seg: &str) -> Option<Slug> {
    match seg.split_once(':') {
        Some((cat, name)) => Some((
            Some(SafePathComponent::new(cat.to_string())?),
            SafePathComponent::new(name.to_string())?,
        )),
        None => Some((None, SafePathComponent::new(seg.to_string())?)),
    }
}

/// The default landing slug — [`START_PAGE`] as a bare (no-category) slug,
/// what a site root resolves to when the site's `shell` names no landing of
/// its own.
#[must_use]
pub fn start_slug() -> Slug {
    let name = SafePathComponent::new(START_PAGE.to_owned());
    (None, name.expect("\"start\" is one safe component"))
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
        [s] => Some((spc(s)?, start_slug())),
        [s, p] => Some((spc(s)?, parse_slug(p)?)),
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
/// Serialized onto `/-/repo/<site>/files/<xx>/<yy>/<hash>.<ext>` by the resolver.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CaRef {
    /// 64-char lowercase hex SHA-256.
    pub hash: String,
    /// Extension without the dot (`"jpg"`, `"png"`, …).
    pub ext: String,
}

/// A content-addressed body id: the SHA-256 of the materialised body text,
/// truncated to 20 bytes — the body-store key of [`RepoSnapshot`]. The
/// snapshot stays content-addressed (a changed body is a new id, so nothing
/// is ever invalidated, only added; identical bodies dedup to one entry)
/// without dragging the storage format into this crate (it must build for
/// wasm).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct BlobId(pub [u8; 20]);

impl BlobId {
    /// From a `git2::Oid`'s raw bytes (the server-side boundary conversion).
    pub const fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }

    /// The raw bytes (for a `git2::Oid::from_bytes` round-trip).
    pub const fn bytes(&self) -> [u8; 20] {
        self.0
    }
}

/// The whole-corpus snapshot the publication worker builds and every consumer core
/// reads: each site's metadata and `files/` index, plus **every latest page
/// body materialised exactly once** (`Arc<str>`: frontmatter stripped,
/// NBSP-normalised) — RAM is the cheaper currency, and the lazy per-visit
/// odb reads this replaced measured as roughly half the CPU of a
/// full-corpus render. Persistent [`imbl::HashMap`]s throughout, so cloning
/// (the `repo` → `repo_snap` bridge, and once per page resolution) is O(1)
/// and an update is non-destructive. `Send + Sync`: no repository handle,
/// no `Rc` — it crosses the worker→core channel once, then crosses cores by
/// reference. The serde impls exist only because the gear-output enums
/// derive them; an internal snapshot is never serialized in practice
/// (never exposed, never pushed).
#[derive(Default, Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RepoSnapshot {
    /// The mirrored sites, nested by slug family.
    pub sites: imbl::HashMap<SafePathComponent, WDWebsite>,
    /// Materialised latest bodies by content-addressed blob id.
    pub bodies: imbl::HashMap<BlobId, std::sync::Arc<str>>,
}

/// One mirrored site: its pages nested by category; the site chrome from
/// `<site>/shell` (title, subtitle, the theme-root path into `files/`, and
/// `landing` — the slug a bare site root resolves to, [`start_slug`] unless
/// the shell names one); and the content-addressed `files/` index — each
/// mirrored attachment's `<host>/<path>` tail (percent-decoded) mapped to its
/// [`CaRef`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct WDWebsite {
    pub articles:
        imbl::HashMap<Option<SafePathComponent>, imbl::HashMap<SafePathComponent, Article>>,
    /// Wikidot numeric page id → the page's current slug. Page ids are stable
    /// across renames (the slug is not), so this index stays authoritative
    /// while `articles` re-keys — the incremental update maintains both.
    pub by_page_id: imbl::HashMap<u64, Slug>,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// The theme stylesheet's `<host>/<path>` tail (`files/` prefix stripped);
    /// resolved against [`WDWebsite::files`] to a CA URL by the `shell` gear.
    pub theme_root: Option<RepoAssetPath>,
    /// The landing slug — the shell's `landing` key (`start` by default;
    /// always present, like the landing itself).
    pub landing: Slug,
    pub files: imbl::HashMap<RepoAssetPath, CaRef>,
}

impl Default for WDWebsite {
    fn default() -> Self {
        Self {
            articles: imbl::HashMap::new(),
            by_page_id: imbl::HashMap::new(),
            title: None,
            subtitle: None,
            theme_root: None,
            landing: start_slug(),
            files: imbl::HashMap::new(),
        }
    }
}

/// One page: metadata, the full revision-history summary, and the
/// content-addressed id of the latest body. The latest body's text lives
/// materialised once in the owning [`RepoSnapshot`]; older revisions stay
/// in the per-page archive on disk for a revision-serving gear to page in
/// later.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Article {
    pub meta: kolorinko_wikitext::ArticleMeta,
    pub latest_body: BlobId,
    pub revisions: Vec<kolorinko_wikitext::RevMeta>,
}

/// One `[[code]]` block served through Wikidot's `/code/N` endpoint shape —
/// the legacy slug family's `/<cat:><name>/code/<N>` tail (on the space-\
///segmented main origin or a wiki's own domain). The opener's
/// `type="css"`-ness picks the MIME (`text/css` vs `text/plain`), which is
/// what makes `@import`ing the block work as a stylesheet. Like any other
/// asset the body is zstd-compressed **once in the gear** (never per
/// request) and carries its strong ETag alongside, so the HTTP layer's hot
/// path is a refcount bump. HTTP-only output: never shipped over
/// WebTransport.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CodeBlock {
    /// The opener carried `type="css"` (any case) → serve `text/css`.
    pub css: bool,
    /// The block's interior, verbatim from the page source (no entity
    /// re-escaping — a deliberate divergence from Wikidot's one-extra-`&amp;`
    /// behaviour), compressed like every other asset.
    pub body: Body,
    /// Strong quoted ETag over the *decoded* interior (stable across
    /// encodings, so a zstd-capable and a plain client revalidate
    /// identically).
    pub etag: String,
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

/// A batched slug → canonical query: the set of a page's internal link
/// targets, resolved to their `(local id, title)` pairs by one lens read.
/// Sorted and deduplicated by the resolver before it becomes a gear id, so
/// the id is a pure function of the *set* (edit-order churn never
/// re-instantiates the gear). Plain data with no local ids, so localization
/// is the identity.
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
pub struct PageQuery(#[localizable(skip)] pub Vec<Slug>);

/// The [`PageQuery`] answer: positional — entry `i` is the `(local id,
/// title)` of `query[i]`, `None` when the site has no such page (the
/// referrer renders that link as a `newpage`).
pub type PageQueryResult = Vec<Option<(LocalId, String)>>;

/// One page selected by a ListPages module: everything a template body can
/// reference through `%%…%%` variables.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ListedPage {
    /// Page name without category.
    pub name: String,
    /// Page category, `None` for a root (`_default`) page.
    pub category: Option<String>,
    /// The exporter's numeric page id — the canonical `LocalId` payload
    /// (`LocalId::from_page_id`).
    pub page_id: String,
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct SiteShell {
    pub title: Option<String>,
    pub subtitle: Option<String>,
    /// The Wikidot site slug this space mirrors (`<site>.wikidot.com`), named
    /// by the registry — the read-only banner's "mirroring …" backlink points
    /// there. `None` only for a space the registry doesn't know (the
    /// context-less debug render).
    pub site: Option<String>,
    /// The landing page's canonical address — what a bare `/{space}` (and `/`
    /// on the space's own domain) resolves to — as `(space, local, title)`: the
    /// header's site link targets its titled canonical route (a route the
    /// client router intercepts, so navigation stays in the app) instead of
    /// the bare root the server answers with a 301. `None` while the shell
    /// loads, for a space the registry doesn't know, or when the dataset
    /// lacks the landing page.
    pub root: Option<(SpaceId, LocalId, String)>,
    /// CA URL `/-/repo/<site>/files/<xx>/<yy>/<hash>.css`, or `None` if the site
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
/// `addr`/`route` are gone with the legacy routes: the canonical
/// `(space, local)` address below is both what the URL names and the
/// subscription keys for `page`/`shell`, so hydration needs no resolution
/// round-trip — and the decorative title segment, when wanted, is re-derived
/// from the page's own title ([`title_slug`]).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SsrState {
    pub page: ArticleView,
    pub page_hash: String,
    pub shell: SiteShell,
    pub shell_hash: String,
    pub space: SpaceId,
    pub local: LocalId,
}

pub const SSR_STATE_ID: &str = "kolorinko-ssr";

/// The `window` global naming the space a served origin already implies:
/// injected as [`default_space_script`] into every HTML document the server
/// sends on a wiki's own configured domain (never on the main origin, where
/// every URL carries its own space). The client pairs it with
/// [`parse_local_route`] — `/L…` paths address this space — and collapses
/// `/{default}/L…` hrefs to `/L…`, so the space segment appears in a URL only
/// when it differs from the host's own.
pub const DEFAULT_SPACE_GLOBAL: &str = "__DEFAULT_SPACE_ID__";

/// The `<script>` payload publishing [`DEFAULT_SPACE_GLOBAL`] to the client:
/// `window.__DEFAULT_SPACE_ID__="<space>"`. The spelling is the contract
/// between the injecting server and the reading client — like
/// [`SSR_STATE_ID`], it travels in the document, not the wire.
#[must_use]
pub fn default_space_script(space: SpaceId) -> String {
    format!(r#"<script>window.{DEFAULT_SPACE_GLOBAL}="{space}";</script>"#)
}

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
    use crate::{Body, CodeBlock, LocalId, RepoSnapshot, SiteShell, SpaceId};
    use kolorinko_wikitext::ArticleView;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::GearOut;

    /// The injected script's exact spelling is the server↔client contract:
    /// the server writes it, the client reads `window.__DEFAULT_SPACE_ID__`.
    #[test]
    fn default_space_script_shape() {
        let space = SpaceId::parse("S70P6lbBZxbc-kcpGOCYmZA").unwrap();
        assert_eq!(
            default_space_script(space),
            r#"<script>window.__DEFAULT_SPACE_ID__="S70P6lbBZxbc-kcpGOCYmZA";</script>"#
        );
    }

    /// Wire exposure is an allowlist: only `#[gear(exposed)]` gears appear in
    /// the wire `GearId` (an internal gear's JSON shape fails to deserialize —
    /// a client cannot even name it), and `GearOut::is_exposed` gates which
    /// outputs a push bridge may ship.
    #[test]
    fn wire_exposure_allowlist() {
        let id = wire::GearId::ArticleLatest {
            space: SpaceId::parse("S70P6lbBZxbc-kcpGOCYmZA").unwrap(),
            local: LocalId::from_page_id("109108").unwrap(),
        };
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<wire::GearId>(&json).unwrap(), id);

        // An internal gear is unnamed on the wire: its JSON shape is no longer
        // a `GearId` (which is also why `is_exposed` exists — its output keeps
        // a `GearOut` variant that a push bridge must drop).
        let internal = r#"{\"RepoLListPages\":{\"site\":\"x\",\"query\":{}}}"#;
        assert!(serde_json::from_str::<wire::GearId>(internal).is_err());
        assert!(!GearOut::RepoSnapOut(RepoSnapshot::default()).is_exposed());
        assert!(GearOut::ShellOut(SiteShell::default()).is_exposed());
    }
}
