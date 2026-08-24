//! Abstract syntax tree for Wikidot page markup, plus the page-delivery
//! payload types ([`ArticleView`] / [`ArticleMeta`] / [`RevMeta`]) shared by
//! the kolorinko server and its wasm client.
//!
//! Translated from the PureScript modules `Pagx.Hered.Tipoj` and
//! `Pagx.Hered.Analiz`.
//!
//! The original stored headings, footnotes, `ListPages` modules and
//! `[[include]]` directives out-of-tree, looked up by integer index
//! (`PagxInfIndeks`) into a side table (`PagxInfKon` / `PagxInf`). That
//! indirection existed mainly to keep `EncodeJson` from looping on the
//! recursive tree. Rust has no such problem, so here every piece of data is
//! inlined directly into the tree and a parsed page is simply a [`Content`]
//! (`Vec<Node>`).
//!
//! Syntax reference:
//! <https://www.wikidot.com/doc-wiki-syntax:inline-formatting>

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

mod dates;
mod traverse;

pub use dates::{civil_from_days, days_from_civil};

/// Degrade every unpaired `[[collapsible]]` opener — a
/// [`Node::CollapsibleHeader`] whose closer never arrived during parsing —
/// to its verbatim source text, the same fate the old parser gave an
/// unclosed container. Headers already inside a [`Node::Collapsible`] are
/// paired and stay. Called once on every finished parse.
pub fn degrade_unclosed_collapsibles(content: &mut Content) {
    for node in content.iter_mut() {
        degrade_opener_node(node);
    }
}

fn degrade_opener_node(node: &mut Node) {
    match node {
        Node::CollapsibleHeader { raw, .. } => *node = Node::Raw(std::mem::take(raw)),
        Node::Collapsible { body, .. } => degrade_unclosed_collapsibles(body),
        Node::Include(include) => {
            for (_, value) in &mut include.vars {
                degrade_unclosed_collapsibles(value);
            }
        }
        Node::Text(TextObj::IncludeVar { default, .. }) => {
            if let Some(d) = default {
                degrade_unclosed_collapsibles(d);
            }
        }
        other => {
            let owned = std::mem::replace(other, Node::Raw(String::new()));
            *other = owned.map_node(&mut |mut children| {
                degrade_unclosed_collapsibles(&mut children);
                children
            });
        }
    }
}

/// Assign `toc0`, `toc1`, … anchors to every heading in document order,
/// matching the id scheme Wikidot emits for in-page table-of-contents links.
/// Headings already carrying an explicit anchor (from `[[# name]]` syntax)
/// are left untouched.
pub fn assign_toc_anchors(content: &mut Content) {
    let mut n = 0u32;
    assign_toc_anchors_inner(content, &mut n);
}

fn assign_toc_anchors_inner(content: &mut Content, n: &mut u32) {
    for node in content.iter_mut() {
        if let Node::Heading {
            anchor: a @ None, ..
        } = node
        {
            *a = Some(format!("toc{}", *n));
            *n += 1;
        }
        let owned = std::mem::replace(node, Node::Raw(String::new()));
        *node = owned.map_node(&mut |mut c| {
            assign_toc_anchors_inner(&mut c, n);
            c
        });
    }
}

/// A parsed page: a flat list of top-level nodes.
pub type Content = Vec<Node>;

/// `key="value"` attributes of bracket constructs (`[[div …]]`, modules, …).
pub type Params = HashMap<String, Vec<TextObj>>;

/// Horizontal alignment, optionally floating (text wraps around it).
///
/// Corresponds to PureScript `Arangx` / `Arangx'`. Covers the `[[<]]`,
/// `[[f<]]`, `[[=]]`, `[[>]]`, `[[f>]]`, `[[==]]` constructs as well as the
/// `[[image …]]` and table-cell alignment prefixes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Align {
    /// `true` for the floating forms (`f<`, `f>`) that wrap text around the
    /// block.
    pub floating: bool,
    pub side: AlignSide,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AlignSide {
    /// `<`
    Left,
    /// `=`
    Center,
    /// `>`
    Right,
    /// `==`
    Justify,
}

/// Character-level inline text style (`//`, `**`, `__`, `--`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextStyle {
    /// `//italics//`
    Italic,
    /// `**bold**`
    Bold,
    /// `__underline__`
    Underline,
    /// `--strikethrough--`
    Strikethrough,
}

/// What kind of container a [`Node::Container`] is. The children always live on
/// the `Node::Container` itself; this enum only describes the wrapper.
///
/// Corresponds to PureScript `KonsujInf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContainerKind {
    /// `//…//`, `**…**`, `__…__`, `--…--` applied to a span of children.
    /// PureScript `KunStil`.
    Style(TextStyle),

    /// `[[div …]]`, `[[div_ …]]` or `[[span …]]` with arbitrary `key="value"`
    /// attributes. `inline` selects the tag (`<span>` when true, `<div>` when
    /// false). `block` selects whether the body is auto-paragraphed: `[[div]]`
    /// paragraphs (`true`); `[[div_]]` and `[[span]]` render inline (`false`).
    /// Attribute values may contain variable references, hence `Vec<TextObj>`.
    Div {
        inline: bool,
        block: bool,
        params: HashMap<String, Vec<TextObj>>,
    },

    /// `> quote` blockquote lines (adjacent lines merged). PureScript `Cit`.
    Quote,

    /// `[[size …]] … [[/size]]`. The string is the raw size argument
    /// (`"120%"`, `"larger"`, `"2em"`, …). PureScript `TekstLargx`.
    Size(String),

    /// `{{monospace}}` (`<tt>`).
    Tt,

    /// `##color|text##` coloured text. PureScript `TekstKolor`.
    Color(String),

    /// `[[<]]` / `[[=]]` / `[[>]]` / `[[==]] … [[/<]]` alignment block.
    /// PureScript `Arangx`.
    Align(Align),

    /// `[[iftags …]] … [[/iftags]]`. PureScript `SeEt`. Note: the original
    /// parser folds unprefixed tags into the "required" set together with
    /// `+tag`, so the `tag1 tag2` OR-distinction is lost here.
    IfTags {
        /// Tags the page must have (all of them): `+tag` and unprefixed tags.
        has_all: Vec<String>,
        /// Tags the page must not have (any of them): `-tag`.
        has_none: Vec<String>,
    },
}

/// A "text" run — plain text that may contain module/include variable
/// references but no richer markup. Corresponds to PureScript `TekstObj`.
///
/// Used for the bits of a page that are not full markup: the source and
/// attribute values of [`Node::Image`], the attribute values of
/// [`ContainerKind::Div`], and as the leaves of the tree via [`Node::Text`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextObj {
    /// Plain literal text. PureScript `Tekst`.
    Plain(String),

    /// `%%name|default%%` — a module / ListPages variable with an optional
    /// literal default. PureScript `Param`.
    ModuleVar {
        name: String,
        default: Option<String>,
    },

    /// `{$name//default$}` — an include variable whose default is itself parsed
    /// markup. PureScript `Anst`.
    IncludeVar {
        name: String,
        default: Option<Content>,
    },
}

impl TextObj {
    /// A run of text objs concatenated into a plain string — `Some` only
    /// while every part is [`TextObj::Plain`]; any variable slot in the run
    /// makes it `None`.
    pub fn plain_concat(objs: &[TextObj]) -> Option<String> {
        objs.iter().try_fold(String::new(), |mut acc, o| match o {
            TextObj::Plain(s) => {
                acc.push_str(s);
                Some(acc)
            }
            _ => None,
        })
    }
}

/// A node in the parsed page tree. Corresponds to PureScript `PagxPart`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Node {
    /// Raw, unparsed source — the parser's fallback when a region cannot be
    /// understood (PureScript `Fiask`). Carries the original text verbatim so
    /// nothing is silently dropped.
    Raw(String),

    /// A run of text, possibly with variable references.
    Text(TextObj),

    /// A container wrapping some children. PureScript `Konsuj`.
    Container {
        kind: ContainerKind,
        content: Content,
    },

    /// `+ Heading`, `++ Sub-heading`, … Inlined from the old `subtitol` table
    /// (PureScript `Titol`). `anchor` is the render target id (`id="tocN"`),
    /// assigned by the page-assembly pass in document order; `None` until
    /// then (a bare parse carries no numbering).
    Heading {
        /// Number of leading `+` characters.
        level: u32,
        anchor: Option<String>,
        content: Content,
    },

    /// `[[# name]]` — an inline anchor target (`<a name="…"></a>`).
    AnchorTarget(String),

    /// `[[image source attr="val" …]]` (PureScript `Bild`). `source` is a list
    /// of [`TextObj`]s so it may contain substitutions, and so is each
    /// attribute value.
    Image {
        align: Option<Align>,
        source: Vec<TextObj>,
        params: HashMap<String, Vec<TextObj>>,
    },

    /// `|| cell || cell ||` table. PureScript `Tabel`.
    Table(Vec<Vec<TableCell>>),

    /// `[[table]]` grid table — the bracketed, nestable form. The table holds
    /// explicit `[[row]]` blocks; each row's body is generic [`Content`] in
    /// which `[[cell]]` / `[[hcell]]` appear as [`Node::BlockCell`] nodes (often
    /// wrapped in `[[iftags]]` conditionals).
    BlockTable(BlockTable),

    /// `[[cell …]] … [[/cell]]` (`header == false`, `<td>`) or `[[hcell …]] …
    /// [[/hcell]]` (`header == true`, `<th>`). Parsed anywhere (so it can sit
    /// inside an `[[iftags]]` wrapper within a grid-table row) and gathered into
    /// `<tr>` cells at render time.
    BlockCell(BlockCell),

    /// `^sup^` / `,sub,` — superscript and subscript, parsed together.
    /// PureScript `SupSub`.
    SupSubscript { sup: Content, sub: Content },

    /// `[[module css]] … [[/module]]` — a raw CSS stylesheet. PureScript
    /// `Stilar`.
    Stylesheet(String),

    /// A hyperlink. `[[[target|text]]]`, `[[[target]]]`, a bare `http://…`,
    /// or `[[a class="…" href="…"]]text[[/a]]`. The target is classified
    /// into a [`LinkTarget`] (auto-rewritten by the renderer); `class` carries
    /// the optional `[[a]]` class attribute.
    Link {
        target: LinkTarget,
        text: Content,
        class: Option<String>,
    },

    /// `[[include source vars…]]`. Inlined from the old `subpagx` table
    /// (PureScript `Subpagx`). The included page's own content is fetched
    /// later, not at parse time, so only the reference and the substitution
    /// variables live here.
    Include(Include),

    /// `[[module ListPages …]] … [[/module]]`. Inlined from the old `listPagx`
    /// table (PureScript `ListPagx`).
    ListPages(ListPages),

    /// `%%created_at|format%%` and friends (PureScript `Dat`), produced when a
    /// ListPages template is instantiated with a concrete page. The integer is
    /// a Unix timestamp.
    Date {
        timestamp: i64,
        format: Option<String>,
    },

    /// `[[#ifexpr cond | then]]` / `[[#ifexpr cond | then | else]]` — a
    /// conditional on module variables (`%%rating%%`, `%%total%%`, …),
    /// evaluated at assembly time once the variables are in scope. The
    /// condition is kept as raw text objects so variable substitution can
    /// flatten it; both branches are parsed markup.
    IfExpr {
        cond: Vec<TextObj>,
        then: Content,
        els: Content,
    },

    /// `[[collapsible show="…" hide="…"]] … [[/collapsible]]` — built by the
    /// merge pass when the closer arrives, not by balanced pairing (see
    /// [`Node::CollapsibleHeader`]). `header` is the inline formatting
    /// context around the opener — ordinary container nodes around a
    /// [`Node::CollapsibleHeader`] leaf; the renderer walks it once per
    /// toggle link, which is how Wikidot duplicates the active `[[size]]` /
    /// `[[span]]` / style marks around both links — the idiom
    /// `[[size 120%]][[collapsible …]][[/size]]` relies on. `body` is
    /// everything between the opener and the closer.
    Collapsible { header: Content, body: Content },

    /// A `[[collapsible]]` toggle-link position, planted by the opener at its
    /// exact spot. It rides the tree through whatever containers close
    /// around it, and the closer — wherever it arrives — wraps the inline
    /// chain around it into the `header` of a [`Node::Collapsible`] (see
    /// the merge pass's collapsible pairing). Which label a render shows
    /// (and which way the toggle flips) comes from the render context;
    /// `folded` is the `folded="no"` initial state, `raw` the opener's
    /// verbatim source for the unclosed-opener degradation.
    CollapsibleHeader {
        folded: bool,
        open: String,
        close: String,
        raw: String,
    },

    /// `[[user name]]` (`avatar == false`) or `[[*user name]]` (`avatar ==
    /// true`, avatar variant). The wikidot.com user-info link is derived from
    /// the name; the export carries no user ids, so the avatar image and the
    /// `onclick` handlers of the live site are not reproduced.
    User { name: String, avatar: bool },

    /// `~~~~` / `~~~~<` / `~~~~>` — a clear-float block.
    Clearfloat(ClearSide),

    /// `[[footnote]] … [[/footnote]]` at parse time; the page-assembly pass
    /// collects the bodies in document order and rewrites each occurrence to
    /// a [`Node::FootnoteRef`].
    Footnote(Content),

    /// The numbered reference a collected footnote rendered to (`<sup
    /// class="footnoteref">`).
    FootnoteRef(u32),

    /// `[[footnoteblock]]` — where the collected footnote bodies render.
    /// A bare parse yields the empty marker; the page-assembly pass fills it
    /// with the page's collected bodies (or appends a filled block at the end
    /// of the content when no marker stands in the page).
    FootnoteBlock(Vec<Content>),

    /// `[[tabview]] … [[tab Name]] … [[/tab]] … [[/tabview]]`. Inlined from the
    /// old `Libro` / `subvoj` tables. `id` is the tabview's page-unique index
    /// (assigned by the page-assembly pass) — the `wiki-tabview-<id>` /
    /// `wiki-tab-<id>-<n>` element ids derive from it.
    Tabview { id: u32, tabs: Vec<Tab> },

    /// `----` horizontal rule. PureScript `Hr`.
    HorizontalRule,

    /// A single-tag `[[module Name …]]`. Interactive/dynamic modules with no
    /// static body: the renderer suppresses them, except `NewPage` (the
    /// new-page form).
    Module { name: String, params: Params },

    /// A paired `[[module Name …]] … [[/module]]` — a module with a body
    /// template resolved at assembly time (FrontForum, CountPages, ListUsers,
    /// …) into whatever content it produces.
    ModuleBlock {
        name: String,
        params: Params,
        body: Content,
    },

    /// `[[code]] … [[/code]]` — verbatim preformatted source (not parsed as
    /// wikitext). `raw` is the exact interior as stored — byte-faithful for
    /// Wikidot's `/code/N` endpoint, which serves it verbatim; rendering
    /// trims it. `ty` is the opener's `type` attribute as written
    /// (`type="css"` blocks are served as stylesheets).
    Code { ty: Option<String>, raw: String },

    /// `* item` / `# item` bullet list, nestable by indentation. PureScript
    /// `Listo`.
    List(List),
}

/// A bullet (`*`, `<ul>`) or numbered (`#`, `<ol>`) list. Items are
/// homogeneous in marker within one list; nesting is expressed via
/// [`ListItem::sublist`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct List {
    pub ordered: bool,
    pub items: Vec<ListItem>,
}

/// One entry of a [`List`]. The item's own text lives in `content`; deeper
/// items (more indented in the source) form `sublist`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListItem {
    pub content: Content,
    pub sublist: Option<Box<List>>,
}

/// One cell of a [`Node::Table`]. Corresponds to PureScript `TabelEl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableCell {
    /// How many columns this cell spans (extra leading `||` separators).
    pub colspan: u32,
    /// `true` if the cell was marked as a header with a leading `~`.
    pub header: bool,
    /// Optional cell-level alignment (`<`, `=`, `>`).
    pub align: Option<Align>,
    pub content: Content,
}

/// `[[table]]` grid table — the bracketed, nestable form. Each level carries
/// arbitrary `key="value"` attributes; cells are normal (`[[cell]]` → `<td>`)
/// or header (`[[hcell]]` → `<th>`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockTable {
    pub params: HashMap<String, Vec<TextObj>>,
    pub rows: Vec<BlockRow>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockRow {
    pub params: HashMap<String, Vec<TextObj>>,
    /// Row body: cells ([`Node::BlockCell`]) possibly wrapped in `[[iftags]]`
    /// or other containers, plus inter-cell whitespace.
    pub content: Content,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockCell {
    /// `true` for `[[hcell]]` (renders `<th>`), `false` for `[[cell]]` (`<td>`).
    pub header: bool,
    pub params: HashMap<String, Vec<TextObj>>,
    pub content: Content,
}

/// Where a [`Node::Link`] points. Corresponds to the PureScript
/// `Var { plen, space }`: `plen` for external URLs, `space` for internal wiki
/// paths (with `:` rewritten to `/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkTarget {
    /// External `http://` / `https://` URL (or a same-page `#fragment`).
    Url(String),
    /// Internal wiki reference that link resolution never classified as a
    /// page slug — a site-root link (`[/ label]`, empty path) or a
    /// multi-segment route (`forum/t-1`). Renders as the slug-family route
    /// (the server 301s it); never colored.
    Page(PageRef),
    /// Internal wiki reference that resolution classified as a page slug
    /// and the site has no such page: Wikidot's red `newpage` link (same
    /// slug-family href, plus the `newpage` class).
    Missing(PageRef),
    /// A same-site page reference that resolution looked up and found: the
    /// target's canonical identity (the exporter's numeric page id —
    /// `LocalId::from_page_id` payload — plus its current title), from which
    /// the renderer builds the titled canonical route directly. Renaming the
    /// target page re-resolves the referrer, so the route stays valid.
    Canonical { page_id: String, title: String },
    /// A target from any link kind (`[[a href=…]]`, `[[[…]]]`, `[…]`) that
    /// still carries variable slots (`{$x}` / `%%x%%`): not classifiable as
    /// URL or page until the variables resolve. Substitution re-classifies
    /// the flattened text; whatever is still unresolved at render falls back
    /// to its text projection.
    Unresolved(Vec<TextObj>),
}

/// A reference to a wiki page, shared by [`LinkTarget`] and [`Include`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageRef {
    /// Source space when the reference crosses spaces (e.g. `wikidot:foo`).
    /// `None` means the current space.
    pub space: Option<String>,
    /// Page path segments.
    pub path: Vec<String>,
}

/// `[[include source key="value" …]]`. Corresponds to PureScript `subpagx`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Include {
    pub source: PageRef,
    /// Substitution variables in source order, each value parsed markup.
    /// Duplicate keys are preserved rather than collapsed: the Wikidot
    /// `key={$key}|key=default` fallback idiom needs both the passthrough and
    /// the literal default to survive parsing, with the first non-empty value
    /// winning at substitution time.
    pub vars: Vec<(String, Content)>,
}

/// One tab of a [`Node::Tabview`]. Corresponds to an entry of PureScript
/// `Libro`'s array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub name: Content,
    pub content: Content,
}

/// Which floats a [`Node::Clearfloat`] clears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClearSide {
    Both,
    Left,
    Right,
}

/// `[[module ListPages …]]`. Corresponds to PureScript `ListPagx'`.
///
/// The body is split into three parts (`prependLine` / per-page body /
/// `appendLine`) so the renderer can loop over the matching pages and splice
/// `repeat` in for each one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPages {
    pub params: ListPagesParams,
    /// Rendered once before the matching pages (`prependLine`).
    pub prepend: Content,
    /// Rendered once per matching page, with that page's variables in scope.
    pub repeat: Content,
    /// Rendered once after the matching pages (`appendLine`).
    pub append: Content,
}

/// Selection / ordering parameters of a [`ListPages`] module, as parsed from
/// the `[[module ListPages …]]` argument list. Selectors that reference the
/// *current* page (`category="."`, `tags="="`, …) are kept verbatim here and
/// resolved against the rendering page when the module is assembled.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ListPagesParams {
    pub category: Option<String>,
    pub tags: Option<TagsFilter>,
    pub created_by: Option<String>,
    pub created_at: Option<TimeFilter>,
    pub updated_at: Option<TimeFilter>,
    /// `fullname="…"` — select exactly one page (also how `range="."`, the
    /// "current page" range selector, is resolved at assembly time).
    #[serde(default)]
    pub fullname: Option<String>,
    /// `name="…"` — select by page name without category. `"."` (and the
    /// documented `"="`) is resolved to the current page's name at assembly
    /// time.
    #[serde(default)]
    pub name: Option<String>,
    /// `pagetype="normal"` (default; no `_` prefix), `"hidden"`, or `"*"`.
    #[serde(default)]
    pub pagetype: Option<String>,
    pub order: Option<ListOrder>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    /// `perPage="n"` — a static render shows the first pagination page only.
    #[serde(default)]
    pub per_page: Option<i64>,
    /// `separate="no"` compiles the items into one container instead of a
    /// `list-pages-item` div each. Default `true`.
    #[serde(default = "default_true")]
    pub separate: bool,
    /// `wrapper="no"` omits the outer `list-pages-box` div. Default `true`.
    #[serde(default = "default_true")]
    pub wrapper: bool,
}

fn default_true() -> bool {
    true
}

/// Parsed `tags="…"`: a space- / comma-separated list of `+req`, `-excl` and
/// plain tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TagsFilter {
    /// Tags of which at least one must be present (plain tags).
    pub any: Vec<String>,
    /// Tags that must all be present (`+tag`).
    pub all: Vec<String>,
    /// Tags that must not be present (`-tag`).
    pub none: Vec<String>,
    /// `tags="-"` — pages with no tags at all.
    #[serde(default)]
    pub no_tags: bool,
}

/// A time filter for `created_at` / `updated_at`. Corresponds to PureScript
/// `TempAmpl`; the stored integer is always a count of seconds (for the
/// relative forms) or a Unix timestamp (for the absolute forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimeFilter {
    /// `last N unit` → within the last N seconds. PureScript `ALast`.
    Last(i64),
    /// `later than N unit` → older than N seconds ago. PureScript `AAntau`.
    OlderThan(i64),
    /// `< date` / `<= date` → before the given Unix timestamp. PureScript
    /// `AMalpli`.
    Before(i64),
    /// `> date` / `>= date` → after the given Unix timestamp. PureScript
    /// `APli`.
    After(i64),
    /// `= date range` → between two Unix timestamps. PureScript `AInter`.
    Between(i64, i64),
}

/// Ordering of a [`ListPages`] result.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ListOrder {
    /// Sort key (`"name"`, `"created_at"`, `"rating"`, …).
    pub by: String,
    pub ascending: bool,
}

// ── page-delivery payload ─────────────────────────────────────────────────
//
// The gear layer renders a page into [`Content`] and ships it to the client
// together with the page's metadata and edit-history summary. These types are
// the wire payload of a page subscription, shared (via this crate) between the
// server and the wasm frontend so neither side re-declares the shape.

/// Metadata of a single page, as recorded in the export's `_meta` file.
/// `slug` is the canonical Wikidot fullname (`category:name` or just `name`);
/// `category`/`name` are derivable from it but carried explicitly for the
/// client.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleMeta {
    pub title: String,
    pub tags: Vec<String>,
    pub slug: String,
    /// Wikidot numeric page id (e.g. `"1305054470"`).
    pub page_id: String,
}

/// One entry of a page's edit history (no revision body — bodies are fetched
/// on demand by the postponed revision gear).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevMeta {
    pub revision: u64,
    pub revision_id: String,
    pub timestamp: i64,
    pub author: String,
}

/// One `[[include]]` dependency of a rendered page: the included page's
/// address, plus (recursively) the dependencies fetched while resolving it.
/// Recorded by include resolution as pages are fetched, so it is exactly the
/// set of requested articles — a spanning tree of the include graph (a page
/// included from several places appears under its first includer).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageDep {
    pub site: String,
    pub category: Option<String>,
    pub page: String,
    pub deps: Vec<PageDep>,
}

/// A fully rendered page: metadata, edit-history summary, the resolved
/// [`Content`] (all `[[include]]` directives expanded), and the tree of pages
/// fetched while resolving those includes (empty before resolution).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleView {
    pub meta: ArticleMeta,
    pub revisions: Vec<RevMeta>,
    pub content: Content,
    pub deps: Vec<PageDep>,
}

/// Shippable projection of one page: metadata, the latest revision's raw body,
/// and the revision-history summary (no resolved content, no revision bodies).
/// Owned `String`s — no `Rc`/`Arc` — because it crosses cores.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArticleLatest {
    pub meta: ArticleMeta,
    pub body: String,
    pub revisions: Vec<RevMeta>,
}
