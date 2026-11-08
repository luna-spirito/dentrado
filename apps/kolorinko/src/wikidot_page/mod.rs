//! The Wikidot-evacuation data layer and the gears built on top of it.
//!
//! The data source is an `evakuilo` daemon's publication (its `out/`
//! directory — the configured `evakuilo.dir`), one immutable generation per
//! site:
//! ```text
//! <out>/<site>/HEAD                             ← <root_sha>\n
//! <out>/<site>/roots/aa/bb/<root>.json          ← { pages, files, shell }
//! <out>/<site>/objects/pages/aa/bb/<sha>.json   ← page metadata
//! <out>/<site>/objects/files/aa/bb/<sha>.json   ← attachment index
//! <out>/<site>/objects/shell/aa/bb/<sha>        ← title/subtitle/…
//! <out>/<site>/objects/archives/aa/bb/<sha>.zst ← tar+zstd page history
//! <out>/<site>/files_ca/aa/bb/<sha[4..]>        ← attachment blobs
//! ```
//! Objects are immutable; `HEAD` names the current root atomically. A reader
//! keeps one consistent generation even while publication advances.
//!
//! # Storage model
//! Each site has one [`repo`] metadata gear: it follows `HEAD` to the three
//! sections and publishes [`RepoMeta`] — pages, slugs, revision frontmatter,
//! attachment indexes, and shell chrome. Bodies are deliberately absent:
//! [`article_latest_parsed`] and [`article_latest`] read the archive named by
//! the page's metadata directly from the immutable object store. The metadata
//! gear reuses the previous output's revision frontmatter for archive hashes
//! that did not change, so a tick only decompresses new archives.
//!
//! # Gears
//! - [`repo`] (`timer`, `shared`, one per site): decode the current `HEAD`
//!   generation into [`RepoMeta`].
//! - [`article_latest_parsed`] (`event`, keyed canonically): read the page's
//!   archive, parse its latest body into [`ArticleView`] with `[[include]]`
//!   directives **left unresolved**. Kept separate from [`article_latest`] so
//!   parse gears never depend on one another (which would let two pages that
//!   include each other form a gear cycle).
//! - [`article_latest`] (`event`): the full resolution pipeline — includes
//!   spliced textually, then ListPages, iftags, links, and resources. Reads
//!   the raw archive body directly (a parse view cannot be spliced) and
//!   fetches each site's [`RepoMeta`] lazily, only when an include names it.
//! - [`shell`] (`event`, `shared`): the whole site chrome in one shot —
//!   resolved `nav:top` / `nav:side`, theme-root URL, and landing address.
//! - [`asset`] (`event`, `shared`): content-addressed attachment blobs read
//!   straight from `files_ca/`, with MIME and CSS rewriting decided by the
//!   site's metadata.

use crate::wikidot_parser::{parse, parse_link_target, wikidot_verbatim};
use compio::fs;
use dentrado::core::{
    core_ctx::GearCtx,
    gear::GearResult,
    storage::{GearView, IsStorage},
};
use imbl::HashMap as ImHashMap;
use kolorinko_render::{http_refs, http_tail, rewrite_with};
use kolorinko_rt::{
    CaFile, CodeBlock, ListPagesQuery, ListPagesResult, ListedPage, LocalId, PageArchive, PageMeta,
    PageQuery, PageQueryResult, RepoAssetPath, RepoMeta, SafePathComponent, ServedBlob, SiteShell,
    Slug, SpaceId, parse_slug, start_slug,
};
use kolorinko_wikitext::{
    ArticleView, BlockCell, BlockRow, BlockTable, ContainerKind, Content, Include, LinkTarget,
    ListOrder, ListPages, ListPagesParams, Node, PageDep, PageRef, RevMeta, TextObj, TimeFilter,
};
use log::error;
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::runtime::{GearOutShared, KolorinkoRT};

// The body is split across the submodules below. Shared crate imports live
// here and reach each submodule via `use super::*`. Internal items are
// `pub(super)`; the private globs below share the ones used across siblings
// (and, under test, the include-resolution helpers), while only the genuine
// `crate::wikidot_page::…` API is re-exported `pub(crate)`.
mod article_latest;
mod assets_gear;
mod chunked;
mod code_block;
mod dataset;
mod forms;
mod iftags;
mod includes;
mod lenses;
mod links;
mod listpages;
mod out_walk;
mod repo_gear;
mod resources;
#[cfg(test)]
mod tests;
mod vars;

use article_latest::*;
use dataset::*;
use forms::*;
use iftags::*;
use includes::*;
use links::*;
use listpages::*;
use out_walk::*;
use resources::*;
use vars::*;

pub(crate) use article_latest::{article_latest, article_latest_parsed};
pub(crate) use assets_gear::{asset, ca_url};
pub(crate) use code_block::code_block;
pub(crate) use dataset::{article, latest, list_pages, local_id, query_pages};
pub(crate) use lenses::shell;
pub(crate) use repo_gear::repo;
#[cfg(test)]
pub(crate) use repo_gear::repo_step;
