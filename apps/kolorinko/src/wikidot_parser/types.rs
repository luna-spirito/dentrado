//! Abstract syntax tree for Wikidot page markup.
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

/// A parsed page: a flat list of top-level nodes.
pub type Content = Vec<Node>;

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

    /// `[[div …]]` (`inline == false`, block) or `[[span …]]` (`inline == true`,
    /// inline) with arbitrary `key="value"` attributes. Attribute values may
    /// contain variable references, hence `Vec<TextObj>`. PureScript
    /// `Div { lin }`.
    Div {
        inline: bool,
        params: HashMap<String, Vec<TextObj>>,
    },

    /// `> quote` blockquote lines (adjacent lines merged). PureScript `Cit`.
    Quote,

    /// `[[size …]] … [[/size]]`. The string is the raw size argument
    /// (`"120%"`, `"larger"`, `"2em"`, …). PureScript `TekstLargx`.
    Size(String),

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
    /// (PureScript `Titol`). Anchor ids are a render-time concern, so they are
    /// not stored here.
    Heading {
        /// Number of leading `+` characters.
        level: u32,
        content: Content,
    },

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

    /// `^sup^` / `,sub,` — superscript and subscript, parsed together.
    /// PureScript `SupSub`.
    SupSubscript { sup: Content, sub: Content },

    /// `[[module css]] … [[/module]]` — a raw CSS stylesheet. PureScript
    /// `Stilar`.
    Stylesheet(String),

    /// A hyperlink. `[[[target|text]]]`, `[[[target]]]`, or a bare `http://…`.
    /// PureScript `Ligil`.
    Link { target: LinkTarget, text: Content },

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

    /// `[[footnote]] … [[/footnote]]`. Inlined from the old `piednot` table.
    /// The renderer collects these and emits the footnote block at the foot of
    /// the page (or wherever `[[footnoteblock]]` stands).
    Footnote(Content),

    /// `[[tabview]] … [[tab Name]] … [[/tab]] … [[/tabview]]`. Inlined from the
    /// old `Libro` / `subvoj` tables.
    Tabview(Vec<Tab>),

    /// `----` horizontal rule. PureScript `Hr`.
    HorizontalRule,
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

/// Where a [`Node::Link`] points. Corresponds to the PureScript
/// `Var { plen, space }`: `plen` for external URLs, `space` for internal wiki
/// paths (with `:` rewritten to `/`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkTarget {
    /// External `http://` / `https://` URL.
    Url(String),
    /// Internal wiki page reference (`database:vika-owl`, `science`, …).
    Page(PageRef),
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
    /// Substitution variables; each value is parsed markup.
    pub vars: HashMap<String, Content>,
}

/// One tab of a [`Node::Tabview`]. Corresponds to an entry of PureScript
/// `Libro`'s array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tab {
    pub name: Content,
    pub content: Content,
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

/// Selection / ordering parameters of a [`ListPages`] module.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListPagesParams {
    pub category: Option<String>,
    pub tags: Option<TagsFilter>,
    pub created_by: Option<String>,
    pub created_at: Option<TimeFilter>,
    pub updated_at: Option<TimeFilter>,
    pub order: Option<ListOrder>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
}

/// Parsed `tags="…"`: a space- / comma-separated list of `+req`, `-excl` and
/// plain tags.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagsFilter {
    /// Tags of which at least one must be present (plain tags).
    pub any: Vec<String>,
    /// Tags that must all be present (`+tag`).
    pub all: Vec<String>,
    /// Tags that must not be present (`-tag`).
    pub none: Vec<String>,
}

/// A time filter for `created_at` / `updated_at`. Corresponds to PureScript
/// `TempAmpl`; the stored integer is always a count of seconds (for the
/// relative forms) or a Unix timestamp (for the absolute forms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListOrder {
    /// Sort key (`"name"`, `"created_at"`, `"rating"`, …).
    pub by: String,
    pub ascending: bool,
}
